#!/usr/bin/env python3
"""Compile the production renderer independently of Unix-only cluster backends."""

import json
from pathlib import Path
import shutil
import sys
import tomllib


def toml_value(value):
    if isinstance(value, dict):
        return "{ " + ", ".join(
            f"{key} = {toml_value(item)}" for key, item in value.items()
        ) + " }"
    return json.dumps(value)


def main():
    source = Path(__file__).resolve().parents[2]
    destination = Path(sys.argv[1]).resolve()
    destination.mkdir(parents=True, exist_ok=True)
    manifest = tomllib.loads((source / "Cargo.toml").read_text())
    package = manifest["package"]
    lines = [
        "[package]",
        'name = "kalisto-progress-check"',
        'version = "0.0.0"',
        f'edition = {toml_value(package["edition"])}',
        f'rust-version = {toml_value(package["rust-version"])}',
        "[dependencies]",
    ]
    for name in ["indicatif", "tracing", "tracing-indicatif", "tracing-subscriber"]:
        lines.append(f'{name} = {toml_value(manifest["dependencies"][name])}')
    lines.extend([
        "[dev-dependencies]",
        'portable-pty = "0.9.0"',
        f'serde_json = {toml_value(manifest["dependencies"]["serde_json"])}',
        "[[bin]]",
        'name = "progress-smoke"',
        f'path = {toml_value(str(source / "scripts/ci/progress_smoke.rs"))}',
        "[[test]]",
        'name = "terminal-progress"',
        f'path = {toml_value(str(source / "scripts/ci/progress_terminal.rs"))}',
    ])
    (destination / "Cargo.toml").write_text("\n".join(lines) + "\n")
    shutil.copyfile(source / "Cargo.lock", destination / "Cargo.lock")
    print(destination / "Cargo.toml")


if __name__ == "__main__":
    main()
