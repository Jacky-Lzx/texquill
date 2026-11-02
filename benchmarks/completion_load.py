#!/usr/bin/env python3
"""Compare completion while replaying automatic editor requests during typing."""
import argparse
import hashlib
import json
from pathlib import Path
import queue
import shutil
import statistics
import tempfile
import time

from editing import Server, position
from project_latency import comparable


def replay(binary, root, args):
    server = Server(binary, root, args.timeout)
    main = root / args.document
    original = main.read_text()
    insertion = original.index(args.needle) + len(args.needle)
    pending, responses, events = {}, {}, []
    latest_background = {}
    closing = False
    document = {"textDocument": {"uri": main.as_uri()}}
    other_paths = sorted(root.glob("*.bib"))[:1]
    others = [path.as_uri() for path in other_paths]

    def send(method, params):
        server.next_id += 1
        request_id = server.next_id
        pending[request_id] = (method, time.perf_counter())
        server.send({"id": request_id, "method": method, "params": params})
        return request_id

    def background(method, params):
        latest_background[method] = send(method, params)

    def receive(timeout):
        try:
            message = server.messages.get(timeout=timeout)
        except queue.Empty:
            return
        if isinstance(message, Exception):
            raise message
        if "method" in message:
            if "id" in message:
                server.send({"id": message["id"], "result": None})
            if message["method"] == "workspace/diagnostic/refresh" and not closing:
                send("workspace/diagnostic", {"previousResultIds": []})
                for uri in [main.as_uri(), *others]:
                    send("textDocument/diagnostic", {"textDocument": {"uri": uri}})
            return
        request_id = message.get("id")
        method, start = pending.pop(request_id)
        elapsed = (time.perf_counter() - start) * 1000
        events.append({"method": method, "ms": elapsed, "error": message.get("error", {}).get("code")})
        responses[request_id] = message

    try:
        server.request("initialize", {
            "processId": None, "rootUri": root.as_uri(),
            "capabilities": {"workspace": {"diagnostic": {"refreshSupport": True}},
                             "textDocument": {"diagnostic": {}, "completion": {"completionItem": {
                                 "snippetSupport": True, "resolveSupport": {"properties": ["documentation"]},
                             }}}},
            "initializationOptions": {"distribution": {"enabled": False}, "build": {"onSave": False}},
        })
        server.notify("initialized", {})
        for path in [main, *other_paths]:
            source = original if path == main else path.read_text()
            uri = path.as_uri()
            server.notify("textDocument/didOpen", {"textDocument": {
                "uri": uri, "languageId": "latex" if path == main else "bibtex", "version": 1, "text": source,
            }})
        server.request("textDocument/documentSymbol", document)
        previous = None
        typing = time.perf_counter()
        prose = "% typing " + "x" * args.prelude_edits + "\n" if args.prelude_edits else ""
        prefixes = ["% typing " + "x" * n for n in range(args.prelude_edits)]
        prefixes = [(prefix, len(prefix)) for prefix in prefixes]
        commands = ["\\", "\\i", "\\in", "\\inp"]
        if args.path_prefix is not None:
            commands += ["\\input"]
        prefixes += [(prose + prefix, len(prose + prefix)) for prefix in commands]
        if args.path_prefix is not None:
            prefixes += [(prose + "\\input{" + args.path_prefix[:n] + "}",
                          len(prose + "\\input{" + args.path_prefix[:n]))
                         for n in range(len(args.path_prefix) + 1)]
        for index, (prefix, cursor_offset) in enumerate(prefixes):
            due = typing + index * args.interval_ms / 1000
            while time.perf_counter() < due:
                receive(max(0, min(.02, due - time.perf_counter())))
            if previous in pending:
                server.notify("$/cancelRequest", {"id": previous})
            source = original[:insertion] + "\n" + prefix + "\n" + original[insertion:]
            edited = time.perf_counter()
            server.notify("textDocument/didChange", {"textDocument": {
                "uri": main.as_uri(), "version": index + 2,
            }, "contentChanges": [{"text": source}]})
            cursor = {**document, "position": position(source, insertion + 1 + cursor_offset)}
            background("textDocument/linkedEditingRange", cursor)
            background("textDocument/semanticTokens/full", document)
            background("textDocument/inlayHint", {**document, "range": {
                "start": {"line": 0, "character": 0}, "end": position(source, len(source)),
            }})
            background("textDocument/diagnostic", document)
            background("textDocument/documentColor", document)
            background("textDocument/documentHighlight", cursor)
            background("textDocument/documentSymbol", document)
            background("textDocument/codeAction", {**document, "range": {
                "start": cursor["position"], "end": cursor["position"],
            }, "context": {"diagnostics": [], "triggerKind": 2}})
            previous = send("textDocument/completion", {**cursor, "context": {"triggerKind": 1}})
        deadline = time.perf_counter() + args.timeout
        while previous not in responses:
            if time.perf_counter() > deadline:
                raise TimeoutError("completion did not finish under editor load")
            receive(.05)
        elapsed = (time.perf_counter() - edited) * 1000
        response = responses[previous]
        assert "error" not in response, response
        result = response["result"]
        items = result.get("items", []) if isinstance(result, dict) else result
        label = args.path_label if args.path_prefix is not None else "input"
        item = next(item for item in items if item["label"] == label)
        if args.path_prefix is None:
            assert item["textEdit"]["newText"] == "input{${1:file}}", item
        assert item["textEdit"]["range"]["end"] == cursor["position"], item
        while any(request_id not in responses for request_id in latest_background.values()):
            if time.perf_counter() > deadline:
                raise TimeoutError("current background views did not finish")
            receive(.05)
        views = {}
        for method, request_id in latest_background.items():
            response = responses[request_id]
            assert "error" not in response, (method, response)
            views[method] = comparable(response["result"])
        return {"completion_ms": elapsed, "completion": comparable(result), "views": views, "events": events}
    finally:
        closing = True
        if server.process.poll() is None:
            shutdown = send("shutdown", None)
            deadline = time.perf_counter() + args.timeout
            while shutdown not in responses and time.perf_counter() < deadline:
                receive(.05)
            server.notify("exit", None)
            server.process.wait(timeout=args.timeout)
        server.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("project", type=Path)
    parser.add_argument("--before", type=Path, required=True)
    parser.add_argument("--after", type=Path, required=True)
    parser.add_argument("--document", default="main.tex")
    parser.add_argument("--needle", default=r"\maketitle")
    parser.add_argument("--interval-ms", type=float, default=350)
    parser.add_argument("--prelude-edits", type=int, default=0,
                        help="type this many prose edits before requesting the final command")
    parser.add_argument("--path-prefix", help="continue typing a braced input filename, e.g. Tex/")
    parser.add_argument("--path-label", help="expected filename candidate when using --path-prefix")
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--timeout", type=float, default=90)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.runs < 1 or args.interval_ms < 0 or args.timeout <= 0 or args.prelude_edits < 0:
        parser.error("runs/timeout must be positive; interval/prelude-edits must be nonnegative")
    if (args.path_prefix is None) != (args.path_label is None):
        parser.error("--path-prefix and --path-label must be supplied together")
    binaries = {name: getattr(args, name).resolve(strict=True) for name in ["before", "after"]}
    samples = []
    with tempfile.TemporaryDirectory(prefix="texquill-load-") as directory:
        root = Path(directory).resolve() / "project"
        shutil.copytree(args.project.resolve(strict=True), root, symlinks=True,
                        ignore=shutil.ignore_patterns(".git", "build", "target", "node_modules"))
        for run in range(args.runs):
            order = list(binaries)
            if run % 2:
                order.reverse()
            results = {name: replay(binaries[name], root, args) for name in order}
            assert results["before"]["completion"] == results["after"]["completion"], "completion changed"
            assert results["before"]["views"] == results["after"]["views"], "current background views changed"
            samples.append(results)
            print(json.dumps({"run": run, **{name: round(results[name]["completion_ms"], 2) for name in order}}), flush=True)
    report = {"binaries": {name: {"sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
                           for name, path in binaries.items()},
              "interval_ms": args.interval_ms, "prelude_edits": args.prelude_edits,
              "path_prefix": args.path_prefix, "path_label": args.path_label, "samples": samples,
              "median_ms": {name: statistics.median(run[name]["completion_ms"] for run in samples)
                            for name in binaries}}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report["median_ms"]))


if __name__ == "__main__":
    main()
