#!/usr/bin/env python3
"""Stage a published Kalisto release for the website without replacing cached files."""

import argparse
import hashlib
import os
from pathlib import Path
import shutil
import tempfile
import urllib.parse

from release import ApiError, Forgejo, asset_name, collect_artifacts, release_version, TARGETS


def stage(api, root, tag, commit):
    release_version(tag)
    if api.tag_commit(tag) != commit:
        raise ValueError("release tag points to a different source commit")
    published = api.request("GET", f"/releases/tags/{urllib.parse.quote(tag, safe='')}")
    if published["draft"] or published["tag_name"] != tag or published["target_commitish"] != commit:
        raise ValueError("only a published release from the requested source can be staged")
    assets = api.request("GET", f"/releases/{published['id']}/assets")
    expected = {asset_name(tag, target) for target in TARGETS}
    if len(assets) != len(expected) + 1 or {asset["name"] for asset in assets} != expected | {"SHA256SUMS"}:
        raise ValueError("published release inventory is incomplete or ambiguous")
    root = Path(root)
    root.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".kalisto-stage-", dir=root.parent) as temporary:
        temporary = Path(temporary)
        for asset in assets:
            (temporary / asset["name"]).write_bytes(api.download(asset))
        files = collect_artifacts(temporary, tag, commit)
        checksums = "".join(f"{hashlib.sha256(files[name].read_bytes()).hexdigest()}  {name}\n"
                            for name in sorted(files)).encode()
        if (temporary / "SHA256SUMS").read_bytes() != checksums:
            raise ValueError("published release checksums do not match its archives")
        if api.tag_commit(tag) != commit:
            raise ValueError("release tag moved during staging")
        if root.is_symlink():
            raise ValueError("refusing to stage into a symlinked release cache")
        if root.exists():
            if not root.is_dir() or {path.name for path in root.iterdir()} != expected | {"SHA256SUMS"}:
                raise ValueError("existing release cache has unexpected files; refusing to replace it")
            for name in expected | {"SHA256SUMS"}:
                path = root / name
                if path.is_symlink() or not path.is_file() or path.read_bytes() != (temporary / name).read_bytes():
                    raise ValueError("existing release cache differs; refusing to replace it")
            return root
        root.mkdir()
        try:
            # Publish the checksum inventory last; the website verifies every file.
            for name in sorted(expected) + ["SHA256SUMS"]:
                os.rename(temporary / name, root / name)
        except BaseException:
            shutil.rmtree(root)
            raise
    return root


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for argument in ("root", "tag"):
        parser.add_argument(f"--{argument}", required=True)
    parser.add_argument("--commit", help="Expected release source commit; defaults to resolving the tag")
    args = parser.parse_args()
    api = Forgejo(os.environ.get("GITHUB_TOKEN"), os.environ.get("GITHUB_SERVER_URL", ""),
                  os.environ.get("GITHUB_REPOSITORY", ""))
    commit = args.commit or api.tag_commit(args.tag)
    print(stage(api, args.root, args.tag, commit))


if __name__ == "__main__":
    try:
        main()
    except (ValueError, ApiError, OSError) as error:
        raise SystemExit(str(error))
