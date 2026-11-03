#!/usr/bin/env python3
"""Exercise completion with the user's actual Neovim config, in project copies."""

import argparse
import fcntl
import hashlib
import json
import os
import pty
import struct
import subprocess
import tempfile
import termios
import threading
from pathlib import Path

from completion_matrix import CASES, fixture


def run(args, mode, *, suite_cases=CASES, script=None, project_fixture=fixture):
    with tempfile.TemporaryDirectory(prefix="texquill-completion-ui-") as tmp:
        root = Path(tmp) / "project"
        sources = project_fixture(args.project, root)
        cases = [c for c in suite_cases if not args.cases or c["name"] in args.cases]
        manifest = Path(tmp) / "manifest.json"
        manifest.write_text(
            json.dumps(
                {
                    "root": str(root),
                    "sources": sources,
                    "cases": cases,
                    "mode": mode,
                    "churn_buffers": args.churn_buffers,
                }
            )
        )
        output = args.output / (mode + ".json")
        output.unlink(missing_ok=True)
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 45, 140, 0, 0))
        env = os.environ.copy()
        env.update(
            NVIM_SMOKE_TEST="1",
            NVIM_TEXQUILL=str(args.server.resolve()),
            MATRIX_MANIFEST=str(manifest),
            MATRIX_OUTPUT=str(output.resolve()),
            XDG_STATE_HOME=str(Path(tmp) / "state"),
            XDG_CACHE_HOME=str(Path(tmp) / "cache"),
            TERM="xterm-256color",
        )
        if args.no_watch:
            env["MATRIX_NO_WATCH"] = "1"
        if args.no_chktex:
            env["MATRIX_NO_CHKTEX"] = "1"
        if args.raw_backslash:
            env["MATRIX_RAW_BACKSLASH"] = "1"
        script = (script or Path(__file__).with_suffix(".lua")).resolve()
        process = subprocess.Popen(
            [
                args.nvim,
                "-n",
                "-i",
                "NONE",
                str(root / "main.tex"),
                "-c",
                "luafile " + str(script),
            ],
            stdin=slave,
            stdout=slave,
            stderr=slave,
            env=env,
            start_new_session=True,
        )
        os.close(slave)

        def drain():
            with (args.output / (mode + ".log")).open("wb") as log:
                try:
                    while True:
                        chunk = os.read(master, 65536)
                        if not chunk:
                            break
                        log.write(chunk)
                except OSError:
                    pass

        reader = threading.Thread(target=drain, daemon=True)
        reader.start()
        try:
            process.wait(timeout=260)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
            raise
        finally:
            reader.join(timeout=2)
            os.close(master)
        result = json.loads(output.read_text())
        result.update(
            server_sha256=hashlib.sha256(args.server.read_bytes()).hexdigest(),
            raw_backslash=args.raw_backslash,
            churn_buffers=args.churn_buffers,
            no_watch=args.no_watch,
            no_chktex=args.no_chktex,
        )
        output.write_text(json.dumps(result, indent=2) + "\n")
        assert len(result["samples"]) == len(cases), result
        for sample in result["samples"]:
            print(json.dumps(dict(mode=mode, **sample)), flush=True)
        assert not result.get("error"), result.get("error")
        return all(r["ok"] and not r.get("stale") for r in result["samples"])


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("project", type=Path)
    p.add_argument("--server", type=Path, default=Path("target/release/texquill"))
    p.add_argument("--nvim", default="nvim")
    p.add_argument(
        "--modes",
        nargs="+",
        choices=["warm", "rapid", "build", "save"],
        default=["warm", "rapid", "build", "save"],
    )
    p.add_argument("--cases", nargs="+", choices=[c["name"] for c in CASES])
    p.add_argument(
        "--no-watch",
        action="store_true",
        help="cancel file watchers in the disposable client for diagnosis",
    )
    p.add_argument(
        "--no-chktex",
        action="store_true",
        help="disable ChkTeX in the disposable client for diagnosis",
    )
    p.add_argument(
        "--churn-buffers",
        action="store_true",
        help="replace even unchanged buffer contents between cases, exercising cache invalidation",
    )
    p.add_argument(
        "--raw-backslash",
        action="store_true",
        help="remove only the backslash Insert mapping inside the disposable editor, for owner isolation",
    )
    p.add_argument(
        "--output", type=Path, default=Path("target/performance/completion-matrix-ui")
    )
    args = p.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    ok = True
    for mode in args.modes:
        ok = run(args, mode) and ok
    if not ok:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
