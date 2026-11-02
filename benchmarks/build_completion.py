#!/usr/bin/env python3
"""Locate completion stalls around builds, using a disposable project copy."""

import argparse
import hashlib
import json
from pathlib import Path
import queue
import shutil
import statistics
import sys
import tempfile
import time

from editing import Server, position


COMPILER = r'''import pathlib, sys, time
out, main = map(pathlib.Path, sys.argv[1:3])
out.mkdir(parents=True, exist_ok=True)
(main.parent / "probe-started").write_text(str(time.time()))
time.sleep(float(sys.argv[3]))
(out / (main.stem + ".pdf")).write_bytes(b"%PDF-1.4\n")
(out / (main.stem + ".log")).write_text("Controlled compiler fixture.\n")
(out / (main.stem + ".aux")).write_text("\\relax\n")
files = sorted(p for p in main.parent.rglob("*")
               if p.is_file() and p.suffix in {".tex", ".cls", ".sty", ".bib"})
(out / (main.stem + ".fls")).write_text(
    "PWD " + str(main.parent) + "\n" + "".join("INPUT " + str(p) + "\n" for p in files))
(main.parent / "probe-done").write_text(str(time.time()))
'''


def replay(binary, root, case, args):
    main = root / args.document
    original = main.read_text()
    insertion = original.index(args.needle) + len(args.needle)
    command = original.find("\\input")
    command = command + len("\\inp") if command >= 0 else None
    uri = main.as_uri()
    document = {"textDocument": {"uri": uri}}
    build = {"onSave": case == "save"}
    if args.compiler == "controlled":
        script = root / "probe-compiler.py"
        script.write_text(COMPILER)
        build.update(executable=sys.executable,
                     arguments=[str(script), "%o", "%f", str(args.sleep_seconds)],
                     outputDirectory="probe-build")
    server = Server(binary, root, args.timeout)
    pending, records, events, results = {}, [], [], {}
    base = time.perf_counter()
    base_wall = time.time()
    finished = None

    def send(method, params, **metadata):
        server.next_id += 1
        request_id = server.next_id
        pending[request_id] = (method, time.perf_counter(), metadata)
        server.send({"id": request_id, "method": method, "params": params})
        return request_id

    def complete(source, offset, phase):
        return send("textDocument/completion", {
            **document, "position": position(source, offset),
            "context": {"triggerKind": 1},
        }, phase=phase)

    def receive(timeout):
        nonlocal finished
        try:
            message = server.messages.get(timeout=timeout)
        except queue.Empty:
            return
        if isinstance(message, Exception):
            raise message
        now = time.perf_counter()
        if "method" in message:
            method = message["method"]
            if "id" in message:
                server.send({"id": message["id"], "result": None})
            if method.startswith("latex/"):
                events.append({"method": method, "t_ms": (now - base) * 1000,
                               "params": message.get("params")})
            if method == "latex/buildFinished":
                assert message["params"]["status"] == "success", message["params"].get("message")
                finished = now
            return
        method, start, metadata = pending.pop(message["id"])
        record = {"method": method, "start_ms": (start - base) * 1000,
                  "end_ms": (now - base) * 1000, "ms": (now - start) * 1000,
                  **metadata}
        if "error" in message:
            record["error"] = message["error"]
            # Superseded views can report either cancellation or stale inputs.
            assert method != "workspace/executeCommand", message
            allowed = {-32800, -32801}
            if method.endswith("/diagnostic"):
                allowed.add(-32802)  # ServerCancelled: diagnostic inputs changed.
            assert message["error"]["code"] in allowed, message
        elif method == "textDocument/completion":
            result = message["result"]
            items = result.get("items", []) if isinstance(result, dict) else result
            item = next(item for item in items if item["label"] == "input")
            # An existing argument after the cursor must not receive another one.
            expected = {"input", "input{${1:file}}"} if metadata["phase"] in {
                "warmup", "immediate"} else {"input{${1:file}}"}
            assert item["textEdit"]["newText"] in expected, item
            record["candidate"] = item
        elif method == "workspace/executeCommand":
            assert message["result"]["status"] == "started", message
        records.append(record)
        results[message["id"]] = message

    def background(source, offset):
        cursor = {**document, "position": position(source, offset)}
        for method, params in [
            ("textDocument/linkedEditingRange", cursor),
            ("textDocument/semanticTokens/full", document),
            ("textDocument/diagnostic", document),
            ("textDocument/documentSymbol", document),
            ("textDocument/inlayHint", {**document, "range": {
                "start": {"line": 0, "character": 0},
                "end": position(source, len(source))}}),
        ]:
            send(method, params)

    try:
        server.request("initialize", {
            "processId": None, "rootUri": root.as_uri(),
            "capabilities": {"workspace": {"diagnostic": {"refreshSupport": True}},
                             "textDocument": {"diagnostic": {}, "completion": {
                                 "completionItem": {"snippetSupport": True}}}},
            "initializationOptions": {"distribution": {"enabled": False},
                                      "build": build,
                                      "diagnostics": {"chktex": {"enabled": False}}},
        })
        server.notify("initialized", {})
        server.notify("textDocument/didOpen", {"textDocument": {
            "uri": uri, "languageId": "latex", "version": 1, "text": original}})
        warm = (complete(original, command, "warmup") if command is not None else
                send("textDocument/documentSymbol", document))
        deadline = time.perf_counter() + args.timeout
        while warm not in results:
            assert time.perf_counter() < deadline, "warmup timed out"
            receive(.02)
        if args.prime_build and case != "baseline":
            send("workspace/executeCommand", {"command": "latex.build", "arguments": [uri]})
            deadline = time.perf_counter() + args.timeout
            while finished is None or pending:
                if time.perf_counter() >= deadline:
                    raise TimeoutError("priming build timed out")
                receive(.02)
            for name in ["probe-started", "probe-done"]:
                (main.parent / name).unlink(missing_ok=True)
            finished = None
        time.sleep(.5)
        records.clear()
        events.clear()
        base = time.perf_counter()
        base_wall = time.time()
        if case == "save":
            server.notify("textDocument/didSave", document)
        elif case != "baseline":
            send("workspace/executeCommand", {"command": "latex.build", "arguments": [uri]})
        last = complete(original, command, "immediate") if command is not None else None
        source, offset = original, command or 0
        next_pulse = base + .05
        # Type after the build/save has entered the stream. Never save these edits:
        # the compiler continues to read the original disk snapshot.
        prefixes = ["\\i", "\\in", "\\inp", "\\inpu", "\\input"]
        if case == "rapid":
            prefixes = ["% typing " + "x" * n for n in range(20)] + prefixes
        edit_index = 0
        final_typing = None
        next_edit = base + .02
        deadline = base + args.timeout
        while True:
            now = time.perf_counter()
            if edit_index < len(prefixes) and now >= next_edit:
                prefix = prefixes[edit_index]
                source = original[:insertion] + "\n" + prefix + "\n" + original[insertion:]
                offset = insertion + 1 + len(prefix)
                server.notify("textDocument/didChange", {"textDocument": {
                    "uri": uri, "version": edit_index + 2},
                    "contentChanges": [{"text": source}]})
                if case == "rapid":
                    background(source, offset)
                if last in pending:
                    server.notify("$/cancelRequest", {"id": last})
                # Prose is load generation, not a command completion context.
                if prefix.startswith("\\"):
                    last = complete(source, offset, "typing")
                    final_typing = last
                edit_index += 1
                next_edit += args.interval_ms / 1000
                next_pulse = now + .05
            if edit_index == len(prefixes) and last not in pending and now >= next_pulse:
                stop = (finished + .5) if finished else base + args.baseline_seconds
                if case != "baseline" and finished is None:
                    stop = deadline
                if now < stop:
                    last = complete(source, offset, "pulse")
                    next_pulse = now + .05
            done = edit_index == len(prefixes) and not pending
            if done and ((case == "baseline" and now >= base + args.baseline_seconds)
                         or (finished is not None and now >= finished + .5)):
                break
            if now >= deadline:
                raise TimeoutError(f"{case}: pending={pending}; events={events}")
            receive(.005)
        if case != "baseline":
            assert finished is not None, "missing buildFinished"
        completions = [r for r in records if r["method"] == "textDocument/completion"
                       and "error" not in r]
        assert completions, "no successful completion"
        assert final_typing in results and "error" not in results[final_typing], results
        markers = {name: (float((main.parent / name).read_text()) - base_wall) * 1000
                   for name in ["probe-started", "probe-done"] if (main.parent / name).exists()}
        return {"case": case, "compiler": args.compiler, "markers": markers,
                "document_sha256": hashlib.sha256(original.encode()).hexdigest(),
                "events": events, "records": records,
                "completion_ms": {"median": statistics.median(r["ms"] for r in completions),
                                  "max": max(r["ms"] for r in completions)},
                "final_typing_ms": next(r["ms"] for r in reversed(completions)
                                        if r["phase"] == "typing")}
    finally:
        # Drain requests before Server.close(), whose shutdown helper is serial.
        deadline = time.perf_counter() + args.timeout
        failed = sys.exc_info()[0] is not None
        try:
            if not failed:
                while pending and time.perf_counter() < deadline:
                    receive(.02)
            if failed or pending:
                server.process.kill()
                server.process.wait()
        except BaseException:
            if server.process.poll() is None:
                server.process.kill()
                server.process.wait()
            raise
        finally:
            server.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("project", type=Path)
    parser.add_argument("--server", type=Path, default=Path("target/release/texquill"))
    parser.add_argument("--document", default="main.tex")
    parser.add_argument("--needle", default="\\begin{document}")
    parser.add_argument("--compiler", choices=["controlled", "latexmk"], default="controlled")
    parser.add_argument("--cases", nargs="+", choices=["baseline", "start", "save", "rapid"],
                        default=["baseline", "start", "save", "rapid"])
    parser.add_argument("--rounds", type=int, default=2)
    parser.add_argument("--prime-build", action="store_true",
                        help="finish one build before measuring another in the same server")
    parser.add_argument("--sleep-seconds", type=float, default=2)
    parser.add_argument("--interval-ms", type=float, default=20)
    parser.add_argument("--baseline-seconds", type=float, default=2)
    parser.add_argument("--timeout", type=float, default=60)
    parser.add_argument("--output", type=Path, default=Path("target/benchmarks/build-completion.json"))
    args = parser.parse_args()
    if args.rounds < 1 or args.interval_ms <= 0 or args.sleep_seconds < 0:
        parser.error("rounds/interval must be positive; compiler sleep must be nonnegative")
    binary = args.server.resolve()
    report = {"server": str(binary), "sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
              "project": str(args.project.resolve()), "interval_ms": args.interval_ms,
              "prime_build": args.prime_build,
              "document": args.document, "compiler": args.compiler,
              "sleep_seconds": args.sleep_seconds, "needle": args.needle,
              "baseline_seconds": args.baseline_seconds, "timeout_seconds": args.timeout,
              "note": "LSP response timings, not Neovim menu latency. Cancelled requests excluded.",
              "samples": []}
    for iteration in range(args.rounds):
        for case in args.cases:
            with tempfile.TemporaryDirectory(prefix="texquill-build-completion-") as tmp:
                root = Path(tmp).resolve() / "project"
                shutil.copytree(args.project, root, ignore=shutil.ignore_patterns(
                    ".git", "target", "build", ".texquill", "probe-build"))
                result = replay(binary, root, case, args)
            report["samples"].append({"round": iteration, **result})
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(json.dumps(report, indent=2) + "\n")
            print(json.dumps({"round": iteration, "case": case,
                              "completion_ms": result["completion_ms"],
                              "final_typing_ms": result["final_typing_ms"]}), flush=True)


if __name__ == "__main__":
    main()
