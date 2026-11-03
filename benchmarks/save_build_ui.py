#!/usr/bin/env python3
"""Verify edit/save compilation in a project copy using the real Neovim config."""
import argparse
import shutil
from pathlib import Path

from completion_matrix_ui import run


def fixture(project, root):
    shutil.copytree(project, root, ignore=shutil.ignore_patterns(".git", "build", "target"))
    names = ["main.tex", "Tex/1_Introduction.tex"]
    return {name: (root / name).read_text() for name in names if (root / name).is_file()}


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("project", type=Path)
    p.add_argument("--server", type=Path, default=Path("target/release/texquill"))
    p.add_argument("--nvim", default="nvim")
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--cases", nargs="+")
    args = p.parse_args()
    args.churn_buffers = args.no_watch = args.no_chktex = args.raw_backslash = False
    args.output.mkdir(parents=True, exist_ok=True)
    names = ["main_changed", "main_changed_again", "unchanged_save", "two_fast_saves_main"]
    if (args.project / "Tex/1_Introduction.tex").is_file():
        names += ["child_changed", "child_then_main"]
    names += ["queued_saved_then_unsaved"]
    if args.cases and set(args.cases) - set(names):
        p.error("case is unavailable for this project layout")
    assert run(
        args,
        "save",
        suite_cases=[{"name": name} for name in names],
        script=Path(__file__).with_suffix(".lua"),
        project_fixture=fixture,
    )


if __name__ == "__main__":
    main()
