#!/usr/bin/env python3
"""Native CI-only showcase capture, ownership checks and narrow publication gate."""
import bisect
import datetime
import getpass
import hashlib
import ipaddress
import json
import math
import os
import platform
import re
import shutil
import signal
import socket
import subprocess
import sys
import uuid
from pathlib import Path
from urllib.parse import urlsplit

SCENES = ("start", "deploy", "resume", "scale")
HERE = Path(__file__).resolve().parent


class CaptureError(Exception):
    pass


def require(condition, message):
    if not condition:
        raise CaptureError(message)


def workspace():
    root = Path(os.environ["KALISTO_MACOS_BUILD_ROOT"]).resolve(strict=True)
    work = Path(os.environ["KALISTO_SHOWCASE_WORKSPACE"]).resolve(strict=True)
    require(work.parent == root and work.name.startswith("kalisto-showcase."),
            "Refusing a workspace outside the owned external root")
    require((work / ".showcase-owner").read_text().strip() == os.environ["GITHUB_RUN_ID"],
            "Workspace ownership does not match this CI run")
    return work


def environment(work):
    # No CI credentials, proxy credentials, shell startup hooks, personal
    # kubeconfig, container overrides or personal asciinema config reach scenes.
    cargo = shutil.which("cargo", path=os.environ.get("PATH", ""))
    require(cargo is not None, "The native runner's existing Rust toolchain is required")
    keys = ("HOME", "CARGO_HOME", "RUSTUP_HOME", "CARGO_TARGET_DIR", "SCCACHE_DIR")
    env = {k: os.environ[k] for k in keys if k in os.environ}
    env.update(PATH=f"{work}/bin:{Path(cargo).parent}:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin",
               TMPDIR=str(work / "tmp"), XDG_DATA_HOME=str(work / "data"),
               XDG_CONFIG_HOME=str(work / "config"), XDG_CACHE_HOME=str(work / "cache"),
               ASCIINEMA_CONFIG_HOME=str(work / "config/asciinema"),
               SHELL="/bin/sh", TERM="xterm-256color", LC_ALL="C", NO_COLOR="1",
               RUST_LOG="info", DEMO_PRIVATE=str(work / "private"))
    return env


def execute(args, env, *, cwd=None, log=None):
    result = subprocess.run(args, env=env, cwd=cwd, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, check=False)
    if log is not None:
        log.write_bytes(result.stdout + result.stderr)
    if result.returncode:
        # Never interpolate subprocess output, argv or machine paths into CI logs.
        raise SystemExit(result.returncode if result.returncode > 0 else 128 - result.returncode)
    return result.stdout.decode("utf-8")


def engine_snapshot(env):
    executable = shutil.which("container", path=env["PATH"])
    require(executable is not None, "Installed Homebrew container is required")
    executable = Path(executable).resolve(strict=True)
    require("/Cellar/container/" in str(executable), "Only the installed Homebrew container is permitted")
    version = execute([str(executable), "--version"], env)
    require(version.startswith("container CLI version 1.7.1 (build: release,"),
            "The existing Homebrew container must be compatible release 1.7.1")
    status = json.loads(execute([str(executable), "system", "status", "--format", "json"], env))
    require(status["status"] == "running" and status["server"]["version"] == "1.7.1"
            and status["server"]["build"] == "release", "A compatible daemon must already be running")
    app = Path(status["paths"]["appRoot"]).resolve(strict=True)
    install = Path(status["paths"]["installRoot"]).resolve(strict=True)
    require(executable.is_relative_to(install), "Homebrew executable and daemon installation differ")
    session = execute(["/bin/launchctl", "managername"], env).strip()
    require(session in ("Aqua", "Background"), "Unsupported native launchd session")
    domain = "gui" if session == "Aqua" else "user"
    services = {}
    for label in ("com.apple.container.apiserver", "com.apple.container.container-core-images"):
        output = execute(["/bin/launchctl", "print", f"{domain}/{os.getuid()}/{label}"], env)
        values = {}
        keys = {"program", "CONTAINER_APP_ROOT", "CONTAINER_INSTALL_ROOT", "TMPDIR"}
        if label.endswith(".apiserver"):
            keys.update(("pid", "runs"))
        for line in output.splitlines():
            match = re.match(r"\s*([^=]+?)\s*(?:=>|=)\s*(.*)$", line)
            if match and match[1].strip() in keys:
                values[match[1].strip()] = match[2].strip()
        require(values.get("CONTAINER_APP_ROOT") == str(app)
                and values.get("CONTAINER_INSTALL_ROOT") == str(install),
                "Daemon roots do not match the running installation")
        services[label] = values
    api = services["com.apple.container.apiserver"]
    require(all(k in api for k in ("pid", "runs", "program")), "Daemon identity is incomplete")
    native = Path(api["program"]).with_name("container").resolve(strict=True)
    require(native.is_relative_to(install), "Native CLI escapes the borrowed installation")
    properties = json.loads(execute([str(executable), "system", "property", "list", "--format", "json"], env))
    config = app / "config.toml"
    return {"paths": status["paths"], "server": status["server"], "services": services,
            "properties": properties, "native": str(native),
            "config_sha256": hashlib.sha256(config.read_bytes()).hexdigest() if config.exists() else None}


def verify_borrowed(work, env, before):
    require(engine_snapshot(env) == before, "Borrowed daemon identity or configuration changed")
    selection_path = work / "data/kalisto/apple-container/selection.json"
    if selection_path.exists():
        selected = json.loads(selection_path.read_text())
        require(selected["managed"] is False and selected["executable"] == before["native"]
                and selected["app_root"] == before["paths"]["appRoot"]
                and selected["install_root"] == before["paths"]["installRoot"],
                "Kalisto did not borrow the existing daemon unchanged")
    private_runtime = work / "data/kalisto/artifacts/runtime-v1/apple-container-1.7.1-arm64"
    require(not private_runtime.exists(), "A private Apple runtime must never be installed")


def collision_check(name, env):
    prefix = "kalisto-" + name
    for args in (["container", "list", "--all", "--format", "json"],
                 ["container", "network", "list", "--format", "json"]):
        resources = json.loads(execute(args, env))
        require(isinstance(resources, list) and prefix not in json.dumps(resources),
                "Owned cluster name collides with existing runtime resources")


def cleanup(work, env):
    private = work / "private"
    # A killed scene may leave its port-forward group behind. Guard PID reuse by
    # matching the recorded kernel start time before signalling only that group.
    for path in private.glob("*-process.json"):
        owned = json.loads(path.read_text())
        result = subprocess.run(["/bin/ps", "-p", str(owned["pid"]), "-o", "lstart="],
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
        if result.returncode == 0 and result.stdout.decode().strip() == owned["started"]:
            try:
                os.killpg(owned["pid"], signal.SIGTERM)
            except ProcessLookupError:
                pass
    owner = private / "cluster.json"
    if owner.exists():
        name = json.loads(owner.read_text())["name"]
        require(re.fullmatch(r"showcase-[0-9a-f]{16}", name), "Invalid owned cluster name")
        # Never destroy by discovery or with --all. The marker is written only
        # after collision checks, before the first mutating product command.
        state = work / "data/kalisto/clusters" / name
        if state.exists():
            execute([str(work / "bin/kalisto"), "destroy", "--name", name], env,
                    cwd=work / "demo", log=private / "cleanup.log")
        collision_check(name, env)
        owner.unlink()
    before_file = private / "engine-before.json"
    if before_file.exists():
        verify_borrowed(work, env, json.loads(before_file.read_text()))


def redact_events(events, replacements):
    # Match across PTY chunk boundaries, retaining every event and its timestamp.
    text = "".join(event[2] for event in events)
    starts, offset = [], 0
    for event in events:
        starts.append(offset)
        offset += len(event[2])
    pattern = re.compile("|".join(re.escape(old) for old in sorted(replacements, key=len, reverse=True)))
    matches = list(pattern.finditer(text))
    for match in reversed(matches):
        first = bisect.bisect_right(starts, match.start()) - 1
        last = bisect.bisect_right(starts, match.end() - 1) - 1
        begin, end = match.start() - starts[first], match.end() - starts[last]
        replacement = replacements[match[0]]
        if first == last:
            events[first][2] = events[first][2][:begin] + replacement + events[first][2][end:]
        else:
            events[first][2] = events[first][2][:begin] + replacement
            for index in range(first + 1, last):
                events[index][2] = ""
            events[last][2] = events[last][2][end:]
    return events


def privacy_gate(text, forbidden):
    plain = re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", text)
    for value in forbidden:
        if value and re.search(r"(?<![\w-])" + re.escape(value) + r"(?![\w-])", plain, re.I):
            raise CaptureError("Publication gate rejected private machine identity")
    require(not re.search(r"/(?:Users|Volumes|home)/|/private/var/folders/|\.arpa\b|\.internal\b|"
                          r"\b(?:authorization|bearer|client-key-data|client-certificate-data)\b|"
                          r"(?:password|token|secret)\s*[:=]", plain, re.I),
            "Publication gate rejected private paths, infrastructure or credentials")
    for address in re.findall(r"(?<![\d.])(?:\d{1,3}\.){3}\d{1,3}(?![\d.])", plain):
        ip = ipaddress.ip_address(address)
        require(ip.is_loopback or ip in ipaddress.ip_network("10.42.0.0/16")
                or ip in ipaddress.ip_network("10.43.0.0/16"),
                "Publication gate rejected a non-pod/service infrastructure address")
    for url in re.findall(r"https?://[^\s\x1b<>\"']+", plain):
        parsed = urlsplit(url)
        require(parsed.hostname in {"127.0.0.1", "localhost", "github.com", "raw.githubusercontent.com",
                                    "docker.io", "registry-1.docker.io", "registry.k8s.io", "quay.io"}
                and parsed.username is None and parsed.password is None and not parsed.query,
                "Publication gate rejected a private or credential-bearing URL")
    require(not re.search(r"\b(?:[A-Za-z0-9-]+\.)+(?:local|lan|corp|home)\b", plain, re.I),
            "Publication gate rejected a private hostname")


def publish_cast(raw, target, scene, replacements, forbidden):
    lines = raw.read_text().splitlines()
    header = json.loads(lines[0])
    events = [json.loads(line) for line in lines[1:]]
    require(header["version"] == 2 and header["width"] == 110 and header["height"] == 30,
            "Recorder must produce fixed 110x30 asciicast v2")
    previous = 0
    for event in events:
        require(len(event) == 3 and event[1] == "o" and isinstance(event[2], str)
                and isinstance(event[0], (int, float)) and math.isfinite(event[0])
                and event[0] >= previous, "Recorder emitted invalid timing or non-output events")
        previous = event[0]
    require(events and previous > 0, "Recording must contain actual timed output")
    events = redact_events(events, replacements)
    # Drop incidental recorder metadata rather than publishing its environment.
    public_header = {"version": 2, "width": 110, "height": 30,
                     "timestamp": header["timestamp"], "env": {"TERM": "xterm-256color", "SHELL": "/bin/sh"},
                     "command": f"python3 scene.py {scene}", "title": f"Kalisto: {scene}"}
    content = "\n".join(json.dumps(row, ensure_ascii=False) for row in [public_header, *events]) + "\n"
    privacy_gate("".join(event[2] for event in events), forbidden)
    privacy_gate(json.dumps(public_header), forbidden)
    target.write_text(content)
    return {"id": scene, "file": target.name, "sha256": hashlib.sha256(target.read_bytes()).hexdigest(),
            "duration_seconds": previous, "columns": 110, "rows": 30}


def record(work, env):
    require(platform.system() == "Darwin" and platform.machine() == "arm64",
            "Showcase capture requires native macOS arm64 CI")
    require(os.environ.get("GITHUB_ACTIONS") == "true" and os.environ.get("GITHUB_EVENT_NAME") == "workflow_dispatch",
            "Recording is allowed only in an explicitly dispatched native CI job")
    private, demo, public = (work / name for name in ("private", "demo", "public"))
    for directory in (private, demo, public, work / "bin", work / "tmp"):
        directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    require(not any(public.iterdir()), "Refusing to reuse public capture output")
    asciinema = shutil.which("asciinema", path=env["PATH"])
    require(asciinema is not None, "The already-installed asciinema recorder is required")
    recorder_version = execute([asciinema, "--version"], env).strip()
    require(re.fullmatch(r"asciinema 3\.2\.1", recorder_version), "Expected installed asciinema 3.2.1; do not replace it")
    before = engine_snapshot(env)
    (private / "engine-before.json").write_text(json.dumps(before))
    print("Building the exact dispatched source on external storage.", flush=True)
    execute(["cargo", "build", "--locked", "--release", "--bin", "kalisto"], env,
            cwd=work, log=private / "build.log")
    shutil.copy2(Path(env["CARGO_TARGET_DIR"]) / "release/kalisto", work / "bin/kalisto")
    cli_version = execute(["kalisto", "--version"], env).strip()
    require(re.fullmatch(r"kalisto [0-9][0-9A-Za-z.+-]*", cli_version), "Unexpected public CLI version format")
    commit = execute(["/usr/bin/git", "rev-parse", "HEAD"], env, cwd=work).strip()
    require(re.fullmatch(r"[0-9a-f]{40}", commit) and commit == os.environ["GITHUB_SHA"],
            "Source revision differs from the dispatched full commit")
    name = "showcase-" + uuid.uuid4().hex[:16]
    collision_check(name, env)
    require(not (work / "data/kalisto/clusters" / name).exists(), "Cluster state must be fresh")
    (private / "cluster.json").write_text(json.dumps({"name": name}))
    env["DEMO_CLUSTER"] = name
    with socket.socket() as reservation:
        reservation.bind(("127.0.0.1", 0))
        env["DEMO_API_PORT"] = str(reservation.getsockname()[1])
        profile = ["--name", name, "--backend", "apple-container", "--workers", "2", "--addons", "cni/cilium",
                   "--api-port", env["DEMO_API_PORT"]]
        print("Preparing images and managed tools outside recorded scenes.", flush=True)
        prepared = execute(["kalisto", "pull", *profile], env, cwd=demo, log=private / "pull.log")
        require(not (work / "data/kalisto/clusters" / name).exists(), "Preparation must not create cluster state")
        collision_check(name, env)
        verify_borrowed(work, env, before)
        image = re.search(r"^probe image: (docker\.io/library/busybox@sha256:[0-9a-f]{64})$", prepared, re.M)
        require(image is not None, "Preparation must report its actual digest-pinned BusyBox image")
        roots = re.findall(r"^prepared assets: (.+)$", prepared, re.M)
        require(len(roots) == 1, "Preparation must identify exactly one real asset manifest")
        asset_root = Path(roots[0]).resolve(strict=True)
        require(asset_root.is_relative_to(work / "data/kalisto/artifacts"), "Assets must stay in the isolated external workspace")
        manifest = json.loads((asset_root / "manifest.json").read_text())
        require(manifest["probe_image"] == image[1] and manifest["rollouts"], "Prepared fixture or Cilium rollouts mismatch")
        (demo / "rollouts.json").write_text(json.dumps(manifest["rollouts"]))
        fixture = (HERE / "app.yaml").read_text().replace("docker.io/library/busybox:1.36.1", image[1])
        (demo / "app.yaml").write_text(fixture)
        shutil.copy2(HERE / "scene.py", demo / "scene.py")
    for scene in SCENES:
        print(f"Recording genuine {scene} scene.", flush=True)
        execute([asciinema, "rec", "--output-format", "asciicast-v2", "--window-size", "110x30",
                 "--return", "--headless", "--quiet", "--capture-env", "TERM,SHELL",
                 "--command", f"python3 scene.py {scene}", str(private / (scene + ".cast"))],
                env, cwd=demo, log=private / (scene + "-recorder.log"))
        require((private / (scene + ".success")).is_file(), "A scene did not complete every real assertion")
        verify_borrowed(work, env, before)
    # Destruction and daemon checks are part of success, before any distributable output.
    cleanup(work, env)
    replacements = {str(work): "<demo-workspace>",
                    str(Path(os.environ["KALISTO_MACOS_BUILD_ROOT"]).resolve()): "<build-storage>",
                    str(Path(env["HOME"]).resolve()): "<runner-home>"}
    username = getpass.getuser()
    hostname = socket.gethostname()
    # Machine identity remaining outside those exact path-prefix replacements
    # fails closed and requires private review, never further output rewriting.
    forbidden = [username, hostname, hostname.split(".")[0], os.environ.get("GITHUB_SERVER_URL", "")]
    for key in ("GITHUB_SERVER_URL", "GITHUB_API_URL", "ACTIONS_RUNTIME_URL"):
        value = os.environ.get(key, "")
        if value:
            forbidden.append(urlsplit(value).hostname or value)
    recordings = []
    try:
        for scene in SCENES:
            recordings.append(publish_cast(private / (scene + ".cast"), public / (scene + ".cast"),
                                           scene, replacements, forbidden))
        (public / "app.yaml").write_text(fixture)
        provenance = {"schema_version": 1, "source_commit": commit, "backend": "apple-container",
                      "platform": "macos-arm64", "cli_version": cli_version, "asciinema_version": recorder_version,
                      "recorded_at": datetime.datetime.now(datetime.timezone.utc).isoformat().replace("+00:00", "Z"),
                      "preparation_included": False, "timing": "original",
                      "execution": "scripted prompts with real commands and assertions",
                      "fixture_image": image[1], "fixture_sha256": hashlib.sha256(fixture.encode()).hexdigest(),
                      "redactions": ["Exact owned workspace prefix -> <demo-workspace>; exact external build-root prefix -> <build-storage>; exact runner HOME prefix -> <runner-home>. No other output substitutions.",
                                     "Header rebuilt with original version/dimensions/timestamp and fixed public title, relative command and TERM/SHELL only; incidental environment and idle-limit metadata omitted.",
                                     "All output events retained in original order with unchanged timestamps, including events emptied by a path replacement spanning PTY chunks."],
                      "recordings": recordings}
        privacy_gate(json.dumps(provenance), forbidden)
        (public / "provenance.json").write_text(json.dumps(provenance, indent=2) + "\n")
    except BaseException:
        # Never leave a partially passing inventory available to the upload step.
        for path in public.iterdir():
            path.unlink()
        raise
    (private / "publishable").touch()
    print("All four genuine scenes passed; six publication candidates are ready for human review.", flush=True)


def main():
    os.umask(0o077)
    work = workspace()
    env = environment(work)
    if sys.argv[1:] == ["cleanup"]:
        cleanup(work, env)
        return
    require(not sys.argv[1:], "Unknown recorder command")
    try:
        record(work, env)
    except BaseException:
        try:
            cleanup(work, env)
        except BaseException:
            print("Owned cleanup needs private runner inspection; shared daemon was not stopped.", file=sys.stderr)
        raise


if __name__ == "__main__":
    try:
        main()
    except CaptureError as error:
        print(str(error), file=sys.stderr)
        sys.exit(1)
    except (OSError, ValueError, KeyError):
        print("Recorder infrastructure failed; private evidence remains on external storage.", file=sys.stderr)
        sys.exit(1)
