#!/usr/bin/env python3
"""Compare two release servers on an isolated copy of a self-contained project."""

import argparse
from contextlib import ExitStack
import hashlib
import json
from pathlib import Path
import shutil
import statistics
import tempfile
import time

from editing import Server, position


def comparable(value):
    if isinstance(value, dict):
        return {key: comparable(item) for key, item in value.items() if key != "resultId"}
    if isinstance(value, list):
        return [comparable(item) for item in value]
    return value


def replay(args, root, binaries, run):
    uri = (root / args.document).as_uri()
    source = (root / args.document).read_text()
    at = source.index(args.needle) + args.offset
    cursor = position(source, at)
    document = {"textDocument": {"uri": uri}}
    queries = [
        ("hover", "textDocument/hover", {**document, "position": cursor}),
        ("definition", "textDocument/definition", {**document, "position": cursor}),
        ("completion", "textDocument/completion", {**document, "position": cursor}),
        ("references", "textDocument/references", {
            **document, "position": cursor, "context": {"includeDeclaration": True},
        }),
        ("links", "textDocument/documentLink", document),
        ("tokens", "textDocument/semanticTokens/full", document),
        ("outline", "textDocument/documentSymbol", document),
        ("diagnostic", "textDocument/diagnostic", document),
    ]
    samples = []
    with ExitStack() as stack:
        servers = {}
        order = list(binaries)
        if run % 2:
            order.reverse()
        for name in order:
            server = Server(binaries[name], root, args.timeout)
            stack.callback(server.close)
            server.request("initialize", {
                "processId": None, "rootUri": root.as_uri(),
                "capabilities": {"textDocument": {"diagnostic": {}}},
                "initializationOptions": {
                    "distribution": {"enabled": False}, "build": {"onSave": False},
                    "diagnostics": {"chktex": {"enabled": False}},
                },
            })
            server.notify("initialized", {})
            server.notify("textDocument/didOpen", {"textDocument": {
                "uri": uri, "languageId": "latex", "version": 1, "text": source,
            }})
            # Flush startup discovery before comparing response contents/times.
            server.request("textDocument/hover", {**document, "position": cursor})
            servers[name] = server

        def pair(label, method, params, cycle, changes=()):
            order = list(servers)
            if cycle % 2:
                order.reverse()
            results = {}
            for name in order:
                start = time.perf_counter()
                for version in changes:
                    servers[name].notify("textDocument/didChange", {
                        "textDocument": {"uri": uri, "version": version},
                        "contentChanges": [{"range": {
                            "start": {"line": 0, "character": 0},
                            "end": {"line": 0, "character": 0},
                        }, "text": "% latency probe\n"}],
                    })
                result = servers[name].request(method, params)
                elapsed = (time.perf_counter() - start) * 1000
                results[name] = comparable(result)
                if cycle >= args.warmup:
                    samples.append({"run": run, "cycle": cycle - args.warmup,
                                    "server": name, "query": label, "ms": elapsed})
            assert results["before"] == results["after"], f"{label}: response changed"

        for cycle in range(args.warmup + args.iterations):
            for label, method, params in queries:
                pair(label, method, params, cycle)
        version = 1
        inserted = 0
        for cycle in range(args.warmup + args.iterations):
            version += 1
            inserted += 1
            pair("edit+hover", "textDocument/hover", {
                **document, "position": {**cursor, "line": cursor["line"] + inserted},
            }, cycle, [version])
        inserted += args.burst
        pair(f"burst{args.burst}+hover", "textDocument/hover", {
            **document, "position": {**cursor, "line": cursor["line"] + inserted},
        }, args.warmup, range(version + 1, version + args.burst + 1))
    return samples


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("project", type=Path)
    parser.add_argument("--before", type=Path, required=True)
    parser.add_argument("--after", type=Path, required=True)
    parser.add_argument("--document", default="main.tex")
    parser.add_argument("--needle", default=r"\maketitle")
    parser.add_argument("--offset", type=int, default=4)
    parser.add_argument("--iterations", type=int, default=5)
    parser.add_argument("--warmup", type=int, default=1)
    parser.add_argument("--runs", type=int, default=2)
    parser.add_argument("--burst", type=int, default=4)
    parser.add_argument("--timeout", type=float, default=120)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if min(args.iterations, args.runs, args.burst) < 1 or args.warmup < 0 or args.timeout <= 0:
        parser.error("counts/timeout must be positive; warmup must be nonnegative")
    binaries = {name: getattr(args, name).resolve(strict=True) for name in ["before", "after"]}
    samples = []
    with tempfile.TemporaryDirectory(prefix="texquill-project-") as directory:
        root = Path(directory).resolve() / "project"
        shutil.copytree(args.project.resolve(strict=True), root, symlinks=True,
                        ignore=shutil.ignore_patterns(".git", "build", "target", "node_modules"))
        for run in range(args.runs):
            samples.extend(replay(args, root, binaries, run))
            print(f"run {run + 1}/{args.runs}: matching responses", flush=True)
    summary = {}
    for query in dict.fromkeys(sample["query"] for sample in samples):
        summary[query] = {
            name: round(statistics.median(sample["ms"] for sample in samples
                                          if sample["query"] == query and sample["server"] == name), 3)
            for name in binaries
        }
    report = {"binaries": {name: {"path": str(path), "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
                           for name, path in binaries.items()},
              "runs": args.runs, "iterations": args.iterations, "warmup": args.warmup,
              "summary_ms": summary, "samples": samples}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    main()
