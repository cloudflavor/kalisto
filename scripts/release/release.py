#!/usr/bin/env python3
"""Package verified binaries and publish complete, immutable Forgejo releases."""

import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import re
import struct
import tarfile
import urllib.error
import urllib.parse
import urllib.request
import uuid

TARGETS = {
    "aarch64-apple-darwin": ("macos", "arm64"),
    "x86_64-apple-darwin": ("macos", "amd64"),
    "x86_64-unknown-linux-musl": ("linux", "amd64"),
    "aarch64-unknown-linux-musl": ("linux", "arm64"),
}
TAG = re.compile(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?(?:\+([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?\Z")
COMMIT = re.compile(r"[0-9a-f]{40}\Z")


def release_version(tag):
    match = TAG.fullmatch(tag)
    if not match or any(part.isdigit() and len(part) > 1 and part[0] == "0"
                        for part in (match.group(4) or "").split(".")):
        raise ValueError("release tag must be vMAJOR.MINOR.PATCH with optional valid SemVer suffix")
    return tag[1:]


def build_identity(ref, commit, run_id):
    if not COMMIT.fullmatch(commit):
        raise ValueError("invalid source commit")
    if ref.startswith("refs/tags/"):
        tag = ref.removeprefix("refs/tags/")
        return tag, release_version(tag)
    if not str(run_id).isdigit():
        raise ValueError("invalid CI run ID")
    return f"ci-{commit[:12]}", f"0.0.0-ci.{run_id}+g{commit[:12]}"


def asset_name(tag, target):
    if not TAG.fullmatch(tag) and not re.fullmatch(r"ci-[0-9a-f]{12}", tag):
        raise ValueError("invalid artifact label")
    system, arch = TARGETS[target]
    return f"kalisto-{tag}-{system}-{arch}.tar.gz"


def verify_binary(binary, target):
    """Reject mislabeled architectures; Linux artifacts must be static ELF64."""
    system, arch = TARGETS[target]
    if system == "linux":
        machine = 62 if arch == "amd64" else 183
        if len(binary) < 64 or binary[:6] != b"\x7fELF\x02\x01" or struct.unpack_from("<H", binary, 18)[0] != machine:
            raise ValueError("binary is not the expected Linux ELF64 architecture")
        offset = struct.unpack_from("<Q", binary, 32)[0]
        size, count = struct.unpack_from("<HH", binary, 54)
        if size < 4 or offset + size * count > len(binary):
            raise ValueError("invalid ELF program headers")
        for index in range(count):
            if struct.unpack_from("<I", binary, offset + size * index)[0] == 3:
                raise ValueError("Linux release must be static (unexpected ELF interpreter)")
    else:
        cpu = 0x0100000C if arch == "arm64" else 0x01000007
        if len(binary) < 32 or binary[:4] != b"\xcf\xfa\xed\xfe" or struct.unpack_from("<I", binary, 4)[0] != cpu:
            raise ValueError("binary is not the expected macOS Mach-O64 architecture")


def package(binary_path, output, tag, version, commit, target):
    if not COMMIT.fullmatch(commit):
        raise ValueError("invalid source commit")
    binary = Path(binary_path).read_bytes()
    verify_binary(binary, target)
    output = Path(output)
    output.mkdir(parents=True, exist_ok=True)
    path = output / asset_name(tag, target)
    metadata = json.dumps({"tag": tag, "version": version, "commit": commit,
                           "target": target}, sort_keys=True).encode() + b"\n"
    with path.open("xb") as raw, gzip.GzipFile(fileobj=raw, mode="wb", filename="", mtime=0) as compressed:
        with tarfile.open(fileobj=compressed, mode="w|", format=tarfile.USTAR_FORMAT) as archive:
            for name, content, mode in [("kalisto", binary, 0o755), ("BUILD-INFO.json", metadata, 0o644)]:
                header = tarfile.TarInfo(name)
                header.size, header.mode, header.mtime = len(content), mode, 0
                archive.addfile(header, io.BytesIO(content))
    return path


def collect_artifacts(root, tag, commit):
    """Check complete target inventory and provenance before any release API call."""
    expected = {asset_name(tag, target): target for target in TARGETS}
    found = {}
    for path in Path(root).rglob("*.tar.gz"):
        if path.name not in expected or path.name in found or path.is_symlink():
            raise ValueError("unexpected or duplicate release artifact")
        target = expected[path.name]
        with tarfile.open(path, "r:gz") as archive:
            entries = archive.getmembers()
            if {entry.name for entry in entries} != {"kalisto", "BUILD-INFO.json"} or len(entries) != 2 or any(not entry.isfile() for entry in entries):
                raise ValueError("invalid release archive inventory")
            metadata_entry = archive.getmember("BUILD-INFO.json")
            if metadata_entry.size > 4096:
                raise ValueError("release metadata too large")
            metadata = json.load(archive.extractfile(metadata_entry))
            if metadata.get("tag") != tag or metadata.get("commit") != commit or metadata.get("target") != target:
                raise ValueError("release artifact provenance mismatch")
            if TAG.fullmatch(tag) and metadata.get("version") != release_version(tag):
                raise ValueError("release version mismatch")
            binary_entry = archive.getmember("kalisto")
            if binary_entry.mode != 0o755 or binary_entry.size > 256 * 1024 * 1024:
                raise ValueError("invalid executable mode or size")
            verify_binary(archive.extractfile(binary_entry).read(), target)
        found[path.name] = path
    if set(found) != set(expected):
        raise ValueError("all four macOS/Linux amd64/arm64 artifacts are required")
    return found


class ApiError(RuntimeError):
    def __init__(self, code):
        self.code = code
        super().__init__(f"Forgejo release API returned HTTP {code}")


class SameOriginRedirect(urllib.request.HTTPRedirectHandler):
    def __init__(self, origin):
        super().__init__()
        self.origin = origin

    def redirect_request(self, request, response, code, message, headers, newurl):
        parsed = urllib.parse.urlsplit(newurl)
        if f"{parsed.scheme}://{parsed.netloc}" != self.origin or parsed.username or parsed.password:
            raise ValueError("refusing to forward release credentials to another origin")
        return super().redirect_request(request, response, code, message, headers, newurl)


class Forgejo:
    def __init__(self, token, server, repository):
        parsed = urllib.parse.urlsplit(server)
        if (parsed.scheme != "https" or not parsed.hostname or parsed.username or parsed.password
                or parsed.path not in ("", "/") or parsed.query or parsed.fragment):
            raise ValueError("release server must be an explicitly configured HTTPS origin")
        if (not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository)
                or any(part in (".", "..") for part in repository.split("/"))):
            raise ValueError("release repository must be explicitly configured as owner/name")
        if not token:
            raise ValueError("release token is missing")
        self.origin = f"{parsed.scheme}://{parsed.netloc}"
        self.base = f"{self.origin}/api/v1/repos/{repository}"
        self.token = token

    def request(self, method, path, payload=None, content_type="application/json", raw=False):
        data = json.dumps(payload).encode() if isinstance(payload, dict) else payload
        request = urllib.request.Request(self.base + path, data=data, method=method,
            headers={"Authorization": f"token {self.token}", "Content-Type": content_type})
        try:
            with urllib.request.build_opener(SameOriginRedirect(self.origin)).open(request, timeout=120) as response:
                content = response.read()
        except urllib.error.HTTPError as error:
            raise ApiError(error.code) from None
        return content if raw else json.loads(content)

    def upload(self, release_id, name, content):
        boundary = f"kalisto-{uuid.uuid4().hex}"
        body = (f"--{boundary}\r\nContent-Disposition: form-data; name=\"attachment\"; filename=\"{name}\"\r\n"
                f"Content-Type: application/octet-stream\r\n\r\n").encode() + content + f"\r\n--{boundary}--\r\n".encode()
        return self.request("POST", f"/releases/{release_id}/assets?name={urllib.parse.quote(name)}", body,
                            f"multipart/form-data; boundary={boundary}")

    def download(self, asset):
        url = asset["browser_download_url"]
        parsed = urllib.parse.urlsplit(url)
        if f"{parsed.scheme}://{parsed.netloc}" != self.origin or parsed.username or parsed.password:
            raise ValueError("release asset URL is outside the configured Forgejo origin")
        if not 0 <= asset["size"] <= 256 * 1024 * 1024:
            raise ValueError("release asset size exceeds limit")
        request = urllib.request.Request(url, headers={"Authorization": f"token {self.token}"})
        with urllib.request.build_opener(SameOriginRedirect(self.origin)).open(request, timeout=120) as response:
            data = response.read(asset["size"] + 1)
        if len(data) != asset["size"]:
            raise ValueError("release asset size mismatch")
        return data

    def tag_commit(self, tag):
        refs = self.request("GET", f"/git/refs/tags/{urllib.parse.quote(tag, safe='')}")
        matching = [ref for ref in refs if ref["ref"] == f"refs/tags/{tag}"]
        if len(matching) != 1:
            raise ValueError("release tag is missing or ambiguous")
        obj = matching[0]["object"]
        for _ in range(8):
            if obj["type"] == "commit":
                return obj["sha"]
            if obj["type"] != "tag":
                break
            obj = self.request("GET", f"/git/tags/{obj['sha']}")["object"]
        raise ValueError("tag does not resolve to a commit")


def publish(api, root, tag, commit):
    release_version(tag)
    files = collect_artifacts(root, tag, commit)
    if api.tag_commit(tag) != commit:
        raise ValueError("tag moved or points to a different source commit")
    payloads = {name: path.read_bytes() for name, path in sorted(files.items())}
    payloads["SHA256SUMS"] = "".join(f"{hashlib.sha256(data).hexdigest()}  {name}\n"
        for name, data in payloads.items()).encode()
    try:
        release = api.request("GET", f"/releases/tags/{urllib.parse.quote(tag, safe='')}")
    except ApiError as error:
        if error.code != 404:
            raise
        release = api.request("POST", "/releases", {
            "tag_name": tag, "target_commitish": commit, "name": tag, "draft": True,
            "prerelease": TAG.fullmatch(tag).group(4) is not None,
            "body": f"Kalisto {tag}, source `{commit}`.\n\nmacOS and static Linux binaries for amd64 and arm64.\n"
                    "Verify downloads against SHA256SUMS. macOS binaries are unsigned/not notarized.\n",
        })
    if not release["draft"]:
        raise ValueError("release is already published; refusing to replace published assets")
    if release["target_commitish"] != commit:
        raise ValueError("existing draft targets a different source; refusing to modify it")
    release_id = release["id"]
    existing = {item["name"]: item for item in api.request("GET", f"/releases/{release_id}/assets")}
    if set(existing) - set(payloads):
        raise ValueError("draft contains unexpected assets; refusing to publish")
    for name, data in payloads.items():
        if name in existing:
            remote = api.download(existing[name])
            if hashlib.sha256(remote).digest() != hashlib.sha256(data).digest():
                raise ValueError("draft asset differs; refusing to overwrite it")
        else:
            api.upload(release_id, name, data)
    uploaded = api.request("GET", f"/releases/{release_id}/assets")
    if len(uploaded) != len(payloads) or {item["name"] for item in uploaded} != set(payloads):
        raise ValueError("release asset inventory is incomplete")
    for asset in uploaded:
        if hashlib.sha256(api.download(asset)).digest() != hashlib.sha256(payloads[asset["name"]]).digest():
            raise ValueError("uploaded release asset failed SHA-256 readback; release left draft")
    # Recheck immediately before publication to catch force-moved tags.
    if api.tag_commit(tag) != commit:
        raise ValueError("tag moved during upload; release left draft")
    return api.request("PATCH", f"/releases/{release_id}", {"draft": False})


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    identity = sub.add_parser("identity")
    identity.add_argument("--ref", required=True)
    identity.add_argument("--commit", required=True)
    identity.add_argument("--run-id", required=True)
    pack = sub.add_parser("package")
    for arg in ["binary", "output", "tag", "version", "commit", "target"]:
        pack.add_argument(f"--{arg}", required=True)
    for command in ["verify", "publish"]:
        operation = sub.add_parser(command)
        for arg in ["root", "tag", "commit"]:
            operation.add_argument(f"--{arg}", required=True)
    args = parser.parse_args()
    if args.command == "identity":
        tag, version = build_identity(args.ref, args.commit, args.run_id)
        print(f"BUILD_TAG={tag}\nKALISTO_BUILD_VERSION={version}")
    elif args.command == "package":
        print(package(args.binary, args.output, args.tag, args.version, args.commit, args.target))
    elif args.command == "verify":
        print("\n".join(sorted(collect_artifacts(args.root, args.tag, args.commit))))
    else:
        if os.environ.get("GITHUB_EVENT_NAME") != "push" or os.environ.get("GITHUB_REF") != f"refs/tags/{args.tag}":
            raise ValueError("publication is allowed only for a pushed release tag")
        api = Forgejo(os.environ.get("GITHUB_TOKEN"), os.environ.get("GITHUB_SERVER_URL", ""),
                      os.environ.get("GITHUB_REPOSITORY", ""))
        print(publish(api, args.root, args.tag, args.commit)["html_url"])


if __name__ == "__main__":
    try:
        main()
    except (ValueError, ApiError, OSError) as error:
        raise SystemExit(str(error))
