#!/usr/bin/env python3
"""Check command, argument and macro completion on disposable project copies."""

import argparse
import hashlib
import json
import queue
import re
import shutil
import sys
import tempfile
import time
from pathlib import Path

from build_completion import COMPILER

from editing import Server, position


def case(name, prefix, label, edits, lines, **extra):
    return dict(
        name=name,
        prefix=prefix,
        label=label,
        edits=edits,
        lines=lines,
        head="",
        tail="",
        document="main.tex",
        **extra,
    )


CASES = [
    case(
        "section",
        r"\sec",
        "section",
        ["section", "section{${1:Title}}"],
        [r"\section", r"\section{Title}"],
    ),
    case(
        "subsection",
        r"\subsec",
        "subsection",
        ["subsection", "subsection{${1:Title}}"],
        [r"\subsection", r"\subsection{Title}"],
    ),
    case("textbf", r"\textb", "textbf", ["textbf{${1:text}}"], [r"\textbf{text}"]),
    case("emph", r"\emp", "emph", ["emph{${1:text}}"], [r"\emph{text}"]),
    case("item", r"\ite", "item", ["item", "item $0"], [r"\item", "\\item "]),
    case(
        "frac",
        r"\fra",
        "frac",
        ["frac{${1:numerator}}{${2:denominator}}"],
        [r"\(\frac{numerator}{denominator}\)"],
    ),
    case("alpha", r"\alp", "alpha", ["alpha"], [r"\(\alpha\)"]),
    case("environment", r"\begin{ite", "itemize", ["itemize"], [r"\begin{itemize}"]),
    case(
        "reference",
        r"\ref{sec:codex",
        "sec:codexprobe",
        ["sec:codexprobe"],
        [r"\ref{sec:codexprobe}"],
    ),
    case("citation", r"\cite{codex", "codex2026", ["codex2026"], [r"\cite{codex2026}"]),
    case(
        "graphics",
        r"\includegraphics{figures/codex-f",
        "figures/codex-figure.pdf",
        ["figures/codex-figure.pdf"],
        [r"\includegraphics{figures/codex-figure.pdf}"],
    ),
    case(
        "package",
        r"\usepackage{codex-p",
        "codex-package.sty",
        ["codex-package"],
        [r"\usepackage{codex-package}"],
        preamble=True,
    ),
    case(
        "definition",
        r"\newcomm",
        "newcommand",
        [r"newcommand{\\${1:name}}[${2:1}]{${3:definition}}"],
        [r"\newcommand{\name}[1]{definition}"],
        preamble=True,
    ),
    case(
        "macro",
        r"\codexp",
        "codexprobe",
        ["codexprobe{${1:arg1}}{${2:arg2}}"],
        [r"\codexprobe{arg1}{arg2}"],
    ),
    case(
        "macro_immediate",
        r"\codexp",
        "codexprobe",
        ["codexprobe{${1:arg1}}"],
        [r"\codexprobe{arg1}"],
        arity=1,
    ),
    case(
        "macro_paused",
        r"\codexp",
        "codexprobe",
        ["codexprobe{${1:arg1}}"],
        [r"\codexprobe{arg1}"],
        arity=1,
        pause=1,
    ),
]
for c in CASES:
    if c["name"] in {"frac", "alpha"}:
        c.update(head=r"\(", tail=r"\)")
    elif c["name"] == "item":
        c.update(head="\\begin{itemize}\n", tail="\n\\end{itemize}")
    elif "{" in c["prefix"]:
        c["tail"] = "}"
for name, document in [
    ("abstract_textbf", "Tex/0_Abstract.tex"),
    ("child_textbf", "Tex/1_Introduction.tex"),
]:
    c = dict(next(c for c in CASES if c["name"] == "textbf"))
    c.update(name=name, document=document)
    CASES.append(c)

for original in [c for c in CASES if c["name"] in {"frac", "alpha"}]:
    c = dict(original)
    c.update(
        name=c["name"] + "_dollar",
        head="$",
        tail="$",
        lines=["$" + line[2:-2] + "$" for line in c["lines"]],
    )
    CASES.append(c)


def fixture(project, root):
    shutil.copytree(
        project, root, ignore=shutil.ignore_patterns(".git", "build", "target")
    )
    main = root / "main.tex"
    source = main.read_text().replace(
        r"\begin{document}", "\\newcommand{\\codexprobe}[2]{#1#2}\n\\begin{document}", 1
    )
    source = source.replace(
        r"\maketitle",
        "\\maketitle\n\\section{Completion probe}\\label{sec:codexprobe}",
        1,
    )
    main.write_text(source)
    bib = root / "ref.bib"
    bib.write_text(
        bib.read_text()
        + "\n@article{codex2026,author={Probe, A},title={Completion},journal={Testing},year={2026}}\n"
    )
    (root / "figures").mkdir(exist_ok=True)
    # A valid blank page also permits an editor's optional image preview.
    objects = [
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 20 20] /Resources << >> /Contents 4 0 R >>",
        b"<< /Length 0 >>\nstream\n\nendstream",
    ]
    pdf = b"%PDF-1.4\n"
    positions = []
    for index, value in enumerate(objects, 1):
        positions.append(len(pdf))
        pdf += f"{index} 0 obj\n".encode() + value + b"\nendobj\n"
    start = len(pdf)
    pdf += b"xref\n0 5\n0000000000 65535 f \n"
    pdf += b"".join(f"{at:010d} 00000 n \n".encode() for at in positions)
    pdf += f"trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n{start}\n%%EOF\n".encode()
    (root / "figures/codex-figure.pdf").write_bytes(pdf)
    (root / "codex-package.sty").write_text("\\ProvidesPackage{codex-package}\n")
    return {
        name: (root / name).read_text()
        for name in sorted({c["document"] for c in CASES})
    }


def anchor(source, c):
    if c.get("preamble"):
        return source.index(r"\begin{document}")
    for marker in [r"\maketitle", r"\begin{abstract}", r"\section{Introduction}"]:
        if marker in source:
            return source.index(marker) + len(marker)
    return 0


def expand(value, snippet):
    if not snippet:
        return value
    value = re.sub(r"\$\{\d+:([^{}]*)\}", lambda m: m[1], value)
    value = re.sub(r"\$\d+|\$\{\d+\}", "", value)
    return re.sub(r"\\([\\$}])", r"\1", value)


def offset(source, pos):
    lines = source.splitlines(keepends=True)
    return sum(map(len, lines[: pos["line"]])) + len(
        lines[pos["line"]]
        .encode("utf-16-le")[: 2 * pos["character"]]
        .decode("utf-16-le")
    )


class Session(Server):
    def __init__(self, binary, root, timeout):
        super().__init__(binary, root, timeout)
        self.pending, self.results, self.events = {}, {}, []
        self.versions = {}
        self.documents = []
        self.finished = 0

    def send_request(self, method, params):
        self.next_id += 1
        self.pending[self.next_id] = (method, time.perf_counter())
        self.send({"id": self.next_id, "method": method, "params": params})
        return self.next_id

    def receive(self, timeout=0.01):
        try:
            message = self.messages.get(timeout=timeout)
        except queue.Empty:
            return
        if isinstance(message, Exception):
            raise message
        if "method" in message:
            if "id" in message:
                self.send({"id": message["id"], "result": None})
            if message["method"] == "workspace/diagnostic/refresh":
                self.send_request("workspace/diagnostic", {"previousResultIds": []})
                for uri in self.documents:
                    self.send_request(
                        "textDocument/diagnostic", {"textDocument": {"uri": uri}}
                    )
            if message["method"] == "latex/buildFinished":
                self.finished += 1
                assert message["params"]["status"] == "success", message["params"][
                    "message"
                ]
            return
        method, start = self.pending.pop(message["id"])
        message["elapsed_ms"] = (time.perf_counter() - start) * 1000
        self.results[message["id"]] = message
        self.events.append(
            {
                "method": method,
                "ms": message["elapsed_ms"],
                "error": message.get("error"),
            }
        )

    def until(self, condition):
        deadline = time.perf_counter() + self.timeout
        while not condition():
            if time.perf_counter() >= deadline:
                raise TimeoutError(f"pending: {self.pending}")
            self.receive()

    def quiet(self, seconds):
        deadline = time.perf_counter() + seconds
        while time.perf_counter() < deadline:
            self.receive(min(0.01, deadline - time.perf_counter()))

    def change(self, uri, source):
        self.versions[uri] += 1
        self.notify(
            "textDocument/didChange",
            {
                "textDocument": {"uri": uri, "version": self.versions[uri]},
                "contentChanges": [{"text": source}],
            },
        )

    def completion(self, uri, source, at):
        return self.send_request(
            "textDocument/completion",
            {
                "textDocument": {"uri": uri},
                "position": position(source, at),
                "context": {"triggerKind": 1},
            },
        )

    def background(self, uri, source, at):
        doc = {"textDocument": {"uri": uri}}
        cursor = dict(**doc, position=position(source, at))
        for method, params in [
            ("linkedEditingRange", cursor),
            ("semanticTokens/full", doc),
            ("diagnostic", doc),
            ("documentColor", doc),
            ("documentSymbol", doc),
            ("documentHighlight", cursor),
            (
                "inlayHint",
                dict(
                    **doc,
                    range={
                        "start": {"line": 0, "character": 0},
                        "end": position(source, len(source)),
                    },
                ),
            ),
            (
                "codeAction",
                dict(
                    **doc,
                    range={"start": cursor["position"], "end": cursor["position"]},
                    context={"diagnostics": [], "triggerKind": 2},
                ),
            ),
        ]:
            self.send_request("textDocument/" + method, params)

    def check(self, request, source, at, c):
        message = self.results[request]
        assert "error" not in message, message
        value = message["result"]
        items = value.get("items", []) if isinstance(value, dict) else value
        item = next((i for i in items if i["label"] == c["label"]), None)
        assert item is not None, {
            "case": c["name"],
            "labels": [i["label"] for i in items[:20]],
        }
        edit = item["textEdit"]
        assert edit["newText"] in c["edits"], item
        assert edit["range"]["end"] == position(source, at), item
        value = expand(edit["newText"], item.get("insertTextFormat") == 2)
        start, end = (
            offset(source, edit["range"]["start"]),
            offset(source, edit["range"]["end"]),
        )
        applied = source[:start] + value + source[end:]
        line = applied.splitlines()[edit["range"]["start"]["line"]]
        assert line in c["lines"], {"case": c["name"], "line": line, "item": item}
        return {"ms": message["elapsed_ms"], "candidate": item, "applied_line": line}


def replay(binary, root, sources, mode, args):
    s = Session(binary, root, args.timeout)
    build = {"onSave": mode == "save"}
    if args.compiler == "controlled":
        script = root / "probe-compiler.py"
        script.write_text(COMPILER)
        build.update(
            executable=sys.executable,
            arguments=[str(script), "%o", "%f", "1"],
            outputDirectory="probe-build",
        )
    report = []
    try:
        s.request(
            "initialize",
            {
                "rootUri": root.as_uri(),
                "capabilities": {
                    "workspace": {"diagnostic": {"refreshSupport": True}},
                    "textDocument": {
                        "diagnostic": {},
                        "completion": {"completionItem": {"snippetSupport": True}},
                    },
                },
                "initializationOptions": {
                    "distribution": {"enabled": False},
                    "build": build,
                    "diagnostics": {"chktex": {"enabled": False}},
                },
            },
        )
        s.notify("initialized", {})
        for name, source in sources.items():
            uri = (root / name).as_uri()
            s.documents.append(uri)
            s.versions[uri] = 1
            s.notify(
                "textDocument/didOpen",
                {
                    "textDocument": {
                        "uri": uri,
                        "languageId": "latex",
                        "version": 1,
                        "text": source,
                    }
                },
            )
        s.quiet(1)
        main_uri = (root / "main.tex").as_uri()
        if mode in {"build", "save"}:
            n = s.finished
            s.send_request(
                "workspace/executeCommand",
                {"command": "latex.build", "arguments": [main_uri]},
            )
            s.until(lambda: s.finished > n and not s.pending)
        for c in [c for c in CASES if not args.cases or c["name"] in args.cases]:
            for name, source in sources.items():
                s.change((root / name).as_uri(), source)
            s.quiet(0.5)
            finished = s.finished
            if mode == "build":
                s.send_request(
                    "workspace/executeCommand",
                    {"command": "latex.build", "arguments": [main_uri]},
                )
            elif mode == "save":
                s.notify("textDocument/didSave", {"textDocument": {"uri": main_uri}})
            base = sources[c["document"]]
            if c.get("arity"):
                base = base.replace(r"\codexprobe}[2]{#1#2}", r"\codexprobe}[1]{#1}")
                s.change(main_uri, base)
                s.quiet(c.get("pause", 0))
            uri = (root / c["document"]).as_uri()
            at = anchor(base, c)
            prose = "% typing " + "x" * 20 + "\n" if mode == "rapid" else ""
            prefixes = (
                ["% typing " + "x" * n + "\n" for n in range(20)]
                if mode == "rapid"
                else []
            )
            prefixes += [
                prose + c["head"] + c["prefix"][:n]
                for n in range(1, len(c["prefix"]) + 1)
            ]
            previous = None
            due = time.perf_counter() + 0.02
            for index, prefix in enumerate(prefixes):
                s.quiet(
                    max(0, due + index * args.interval_ms / 1000 - time.perf_counter())
                )
                closing = "" if mode == "rapid" and index < 20 else c["tail"]
                source = base[:at] + "\n" + prefix + closing + "\n" + base[at:]
                cursor = at + 1 + len(prefix)
                s.change(uri, source)
                if previous in s.pending:
                    s.notify("$/cancelRequest", {"id": previous})
                if mode == "rapid":
                    s.background(uri, source, cursor)
                previous = s.completion(uri, source, cursor)
            s.until(lambda request=previous: request in s.results)
            entry = {"mode": mode, "name": c["name"], "prefix": c["prefix"]}
            try:
                entry.update(s.check(previous, source, cursor, c))
                s.quiet(1)
                warm = s.completion(uri, source, cursor)
                s.until(lambda request=warm: request in s.results)
                entry["warm_ms"] = s.check(warm, source, cursor, c)["ms"]
            except (AssertionError, StopIteration) as error:
                entry["error"] = str(error)
            s.until(
                lambda count=finished: (
                    not s.pending
                    and (mode not in {"build", "save"} or s.finished > count)
                )
            )
            report.append(entry)
            print(json.dumps(entry), flush=True)
        return report
    finally:
        # Serial Server.close must not encounter responses from earlier requests.
        if sys.exc_info()[0] is not None:
            s.process.kill()
            s.process.wait()
        else:
            s.until(lambda: not s.pending)
        s.close()


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("project", type=Path)
    p.add_argument("--server", type=Path, default=Path("target/release/texquill"))
    p.add_argument(
        "--modes",
        nargs="+",
        choices=["warm", "rapid", "build", "save"],
        default=["warm", "rapid", "build", "save"],
    )
    p.add_argument("--cases", nargs="+", choices=[c["name"] for c in CASES])
    p.add_argument(
        "--compiler", choices=["controlled", "latexmk"], default="controlled"
    )
    p.add_argument("--timeout", type=float, default=60)
    p.add_argument("--interval-ms", type=float, default=20)
    p.add_argument(
        "--output", type=Path, default=Path("target/performance/completion-matrix.json")
    )
    args = p.parse_args()
    binary = args.server.resolve()
    result = {
        "server_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "cases": CASES,
        "samples": [],
    }
    for mode in args.modes:
        with tempfile.TemporaryDirectory(prefix="texquill-completion-matrix-") as tmp:
            root = Path(tmp).resolve() / "project"
            sources = fixture(args.project, root)
            result["samples"].extend(replay(binary, root, sources, mode, args))
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(result, indent=2) + "\n")
    if any("error" in r for r in result["samples"]):
        raise SystemExit(1)


if __name__ == "__main__":
    main()
