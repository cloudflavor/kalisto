#!/usr/bin/env python3
"""Package verified binaries and publish complete, immutable GitHub releases."""

import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import re
import struct
import subprocess
import tarfile
import urllib.error
import urllib.parse
import urllib.request
import base64

TARGETS = {
    "aarch64-apple-darwin": ("macos", "arm64"),
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
        cpu = 0x0100000C
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
        raise ValueError("macOS arm64, Linux amd64 and Linux arm64 artifacts are required")
    return found


class ApiError(RuntimeError):
    def __init__(self, code):
        self.code = code
        super().__init__(f"GitHub release API returned HTTP {code}")


def github_url(url, host):
    parsed = urllib.parse.urlsplit(url)
    if (parsed.scheme != "https" or parsed.netloc != host or parsed.username
            or parsed.password or parsed.fragment):
        raise ValueError("refusing an untrusted GitHub URL")
    return parsed


class GitHubRedirect(urllib.request.HTTPRedirectHandler):
    def __init__(self, asset=False):
        super().__init__()
        self.asset = asset

    def redirect_request(self, request, response, code, message, headers, newurl):
        parsed = urllib.parse.urlsplit(newurl)
        origin = urllib.parse.urlsplit(request.full_url)
        # These are GitHub's release-asset storage endpoints, not a wildcard
        # allowance for user-controlled githubusercontent.com content.
        storage = {"release-assets.githubusercontent.com", "objects.githubusercontent.com"}
        if (parsed.scheme != "https" or parsed.username or parsed.password
                or parsed.fragment or parsed.netloc != parsed.hostname):
            raise ValueError("refusing an untrusted GitHub redirect")
        same_origin = parsed.netloc == origin.netloc
        if not same_origin and not (self.asset and parsed.netloc in storage and parsed.query):
            raise ValueError("refusing a redirect outside GitHub asset storage")
        redirected = super().redirect_request(request, response, code, message, headers, newurl)
        if redirected is not None and not same_origin:
            redirected.remove_header("Authorization")
        return redirected


class GitHub:
    def __init__(self, token, repository):
        if (not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository)
                or any(part in (".", "..") for part in repository.split("/"))):
            raise ValueError("KALISTO_GITHUB_REPOSITORY must be explicitly configured as owner/name")
        if not token:
            raise ValueError("KALISTO_GITHUB_TOKEN is missing")
        self.repository = repository
        self.base = f"https://api.github.com/repos/{repository}"
        self.token = token

    def headers(self, accept="application/vnd.github+json"):
        return {"Authorization": f"Bearer {self.token}", "Accept": accept,
                "X-GitHub-Api-Version": "2022-11-28"}

    def request(self, method, path, payload=None):
        return self.send(method, self.base + path, payload)

    def send(self, method, url, payload=None, content_type="application/json"):
        host = urllib.parse.urlsplit(url).hostname
        if host not in ("api.github.com", "uploads.github.com"):
            raise ValueError("refusing to send GitHub credentials outside its API endpoints")
        github_url(url, host)
        data = json.dumps(payload).encode() if isinstance(payload, dict) else payload
        request = urllib.request.Request(url, data=data, method=method,
            headers=dict(self.headers(), **{"Content-Type": content_type}))
        try:
            with urllib.request.build_opener(GitHubRedirect()).open(request, timeout=120) as response:
                content = response.read()
        except urllib.error.HTTPError as error:
            raise ApiError(error.code) from None
        return json.loads(content)

    def upload(self, published, name, content):
        url = published["upload_url"].split("{", 1)[0]
        parsed = github_url(url, "uploads.github.com")
        if parsed.path != f"/repos/{self.repository}/releases/{published['id']}/assets" or parsed.query:
            raise ValueError("unexpected GitHub release upload endpoint")
        return self.send("POST", url + "?name=" + urllib.parse.quote(name, safe=""),
                         content, "application/octet-stream")

    def download(self, asset):
        url = asset["url"]
        parsed = github_url(url, "api.github.com")
        if not re.fullmatch(re.escape(f"/repos/{self.repository}/releases/assets/") + r"[0-9]+", parsed.path) or parsed.query:
            raise ValueError("unexpected GitHub asset API endpoint")
        if not isinstance(asset["size"], int) or not 0 <= asset["size"] <= 256 * 1024 * 1024:
            raise ValueError("release asset size exceeds limit")
        request = urllib.request.Request(url, headers=self.headers("application/octet-stream"))
        with urllib.request.build_opener(GitHubRedirect(asset=True)).open(request, timeout=120) as response:
            data = response.read(asset["size"] + 1)
        if len(data) != asset["size"]:
            raise ValueError("release asset size mismatch")
        return data

    def tag_object(self, tag):
        ref = self.request("GET", f"/git/ref/tags/{urllib.parse.quote(tag, safe='')}")
        if ref["ref"] != f"refs/tags/{tag}":
            raise ValueError("GitHub returned a different release tag")
        return {key: ref["object"][key] for key in ("type", "sha")}

    def tag_commit(self, tag):
        obj = self.tag_object(tag)
        for _ in range(8):
            if obj["type"] == "commit" and COMMIT.fullmatch(obj["sha"]):
                return obj["sha"]
            if obj["type"] != "tag" or not COMMIT.fullmatch(obj["sha"]):
                break
            obj = self.request("GET", f"/git/tags/{obj['sha']}")["object"]
        raise ValueError("tag does not resolve to a commit")

    def verify_source(self, tag, commit, expected_tag):
        if self.tag_object(tag) != expected_tag or self.tag_commit(tag) != commit:
            raise ValueError("GitHub tag moved or points to a different source commit")
        source = self.request("GET", f"/commits/{commit}")
        comparison = self.request("GET", f"/compare/{commit}...main")
        if source["sha"] != commit or comparison["status"] not in ("ahead", "identical"):
            raise ValueError("GitHub release source is not on main ancestry")


class SourceRepository:
    """Synchronize only reviewed source with ordinary, protection-aware Git pushes."""
    def git(self, *args, env=None):
        result = subprocess.run(["git", *args], env=env, stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, text=True)
        if result.returncode:
            # Git diagnostics can include credentials; never relay them to CI logs.
            raise ValueError("source Git operation failed; check ancestry, permissions and branch protections")
        return result.stdout.strip()

    def verify_source_refs(self, tag, expected):
        source = os.environ.get("GITHUB_SERVER_URL", "")
        repository = os.environ.get("GITHUB_REPOSITORY", "")
        token = os.environ.get("GITHUB_TOKEN", "")
        parsed = urllib.parse.urlsplit(source)
        if (parsed.scheme != "https" or not parsed.hostname or parsed.username
                or parsed.password or parsed.path not in ("", "/") or parsed.query
                or parsed.fragment or not token or parsed.hostname == "github.com"
                or not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository)
                or any(part in (".", "..") for part in repository.split("/"))):
            raise ValueError("source checkout credentials and HTTPS repository must be explicit")
        credentials = base64.b64encode(f"x-access-token:{token}".encode()).decode()
        origin = f"{parsed.scheme}://{parsed.netloc}"
        env = dict(os.environ, GIT_TERMINAL_PROMPT="0", GIT_CONFIG_COUNT="3",
                   GIT_CONFIG_KEY_0=f"http.{origin}/.extraheader",
                   GIT_CONFIG_VALUE_0=f"Authorization: Basic {credentials}",
                   GIT_CONFIG_KEY_1="credential.helper", GIT_CONFIG_VALUE_1="",
                   GIT_CONFIG_KEY_2="http.followRedirects", GIT_CONFIG_VALUE_2="false")
        refs = self.git("ls-remote", "--refs", f"{origin}/{repository}.git",
                        f"refs/tags/{tag}", "refs/heads/main", env=env)
        refs = dict(line.split()[::-1] for line in refs.splitlines())
        if refs.get(f"refs/tags/{tag}") != expected["sha"]:
            raise ValueError("source release tag changed or disappeared")
        if refs.get("refs/heads/main") != self.git("rev-parse", "refs/remotes/origin/main"):
            raise ValueError("source main changed; rerun against a fresh reviewed source snapshot")

    def verify(self, api, tag, commit, expected):
        self.verify_source_refs(tag, expected)
        api.verify_source(tag, commit, expected)

    def prepare(self, api, tag, commit):
        if not COMMIT.fullmatch(commit):
            raise ValueError("invalid source commit")
        tag_ref = f"refs/tags/{tag}"
        if self.git("rev-parse", "HEAD") != commit or self.git("rev-parse", f"{tag_ref}^{{commit}}") != commit:
            raise ValueError("local release tag does not match the built source")
        main = self.git("rev-parse", "refs/remotes/origin/main")
        self.git("merge-base", "--is-ancestor", commit, main)
        tag_sha = self.git("rev-parse", tag_ref)
        tag_type = self.git("cat-file", "-t", tag_sha)
        expected = {"type": tag_type, "sha": tag_sha}
        self.verify_source_refs(tag, expected)
        credentials = base64.b64encode(f"x-access-token:{api.token}".encode()).decode()
        env = dict(os.environ, GIT_TERMINAL_PROMPT="0", GIT_CONFIG_COUNT="3",
                   GIT_CONFIG_KEY_0="http.https://github.com/.extraheader",
                   GIT_CONFIG_VALUE_0=f"Authorization: Basic {credentials}",
                   GIT_CONFIG_KEY_1="credential.helper", GIT_CONFIG_VALUE_1="",
                   GIT_CONFIG_KEY_2="http.followRedirects", GIT_CONFIG_VALUE_2="false")
        remote = f"https://github.com/{api.repository}.git"
        refs = {}
        for line in self.git("ls-remote", "--refs", remote, "refs/heads/main", tag_ref, env=env).splitlines():
            sha, ref = line.split()
            refs[ref] = sha
        if tag_ref in refs and refs[tag_ref] != tag_sha:
            raise ValueError("GitHub already has a different tag; refusing to overwrite it")
        update_main = "refs/heads/main" not in refs
        if not update_main:
            self.git("fetch", "--no-tags", remote, "refs/heads/main:refs/remotes/github/main", env=env)
            github_main = refs["refs/heads/main"]
            common = self.git("merge-base", github_main, commit)
            if common not in (github_main, commit):
                raise ValueError("GitHub main diverged; refusing to overwrite it")
            update_main = common == github_main and github_main != commit
        updates = []
        if update_main:
            # Do not publish source changes merged after this release was tagged.
            updates.append(f"{commit}:refs/heads/main")
        if tag_ref not in refs:
            updates.append(f"{tag_sha}:{tag_ref}")
        if updates:
            # Atomic and non-force: a racing divergent main/tag or protection
            # rejection fails without partially publishing source refs.
            self.git("push", "--atomic", remote, *updates, env=env)
        self.verify(api, tag, commit, expected)
        return expected


def publish(api, root, tag, commit, source):
    release_version(tag)
    files = collect_artifacts(root, tag, commit)
    expected_tag = source.prepare(api, tag, commit)
    payloads = {name: path.read_bytes() for name, path in sorted(files.items())}
    payloads["SHA256SUMS"] = "".join(f"{hashlib.sha256(data).hexdigest()}  {name}\n"
        for name, data in payloads.items()).encode()
    # The tag lookup is for published releases. Authenticated list requests also
    # expose drafts, including resumable drafts beyond the first page.
    release = None
    page = 1
    while True:
        releases = api.request("GET", f"/releases?per_page=100&page={page}")
        matching = [item for item in releases if item["tag_name"] == tag]
        if len(matching) > 1:
            raise ValueError("ambiguous GitHub release tag")
        if matching:
            release = matching[0]
            break
        if len(releases) < 100:
            break
        page += 1
    if release is None:
        release = api.request("POST", "/releases", {
            "tag_name": tag, "target_commitish": commit, "name": tag, "draft": True,
            "prerelease": TAG.fullmatch(tag).group(4) is not None,
            "body": f"Kalisto {tag}, source `{commit}`.\n\nmacOS arm64 and static Linux amd64/arm64 binaries.\n"
                    "Verify downloads against SHA256SUMS. macOS binaries are unsigned/not notarized.\n",
        })
    if not release["draft"]:
        raise ValueError("release is already published; refusing to replace published assets")
    if release["tag_name"] != tag or release["target_commitish"] != commit:
        raise ValueError("existing draft targets a different source or tag; refusing to modify it")
    release_id = release["id"]
    assets = api.request("GET", f"/releases/{release_id}/assets?per_page=100")
    existing = {item["name"]: item for item in assets}
    if len(existing) != len(assets):
        raise ValueError("draft has duplicate assets; refusing to publish")
    if set(existing) - set(payloads):
        raise ValueError("draft contains unexpected assets; refusing to publish")
    for name, data in payloads.items():
        if name in existing:
            remote = api.download(existing[name])
            if hashlib.sha256(remote).digest() != hashlib.sha256(data).digest():
                raise ValueError("draft asset differs; refusing to overwrite it")
        else:
            api.upload(release, name, data)
    uploaded = api.request("GET", f"/releases/{release_id}/assets?per_page=100")
    if len(uploaded) != len(payloads) or {item["name"] for item in uploaded} != set(payloads):
        raise ValueError("release asset inventory is incomplete")
    for asset in uploaded:
        if hashlib.sha256(api.download(asset)).digest() != hashlib.sha256(payloads[asset["name"]]).digest():
            raise ValueError("uploaded release asset failed SHA-256 readback; release left draft")
    # Recheck source refs and exact GitHub tag object immediately before publication.
    source.verify(api, tag, commit, expected_tag)
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
        api = GitHub(os.environ.get("KALISTO_GITHUB_TOKEN"),
                     os.environ.get("KALISTO_GITHUB_REPOSITORY", ""))
        print(publish(api, args.root, args.tag, args.commit, SourceRepository())["html_url"])


if __name__ == "__main__":
    try:
        main()
    except (ValueError, ApiError, OSError) as error:
        raise SystemExit(str(error))
