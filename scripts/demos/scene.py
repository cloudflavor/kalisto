#!/usr/bin/env python3
"""Scripted prompts, real CLI output; invoked only by the native CI recorder."""
import json
import os
import shlex
import signal
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path


class SceneError(Exception):
    pass


def run(args, *, visible=True):
    if visible:
        print("$ " + shlex.join(args), flush=True)
    result = subprocess.run(args, stdout=None if visible else subprocess.PIPE,
                            stderr=None if visible else subprocess.PIPE, check=False)
    if result.returncode:
        raise SystemExit(result.returncode if result.returncode > 0 else 128 - result.returncode)
    return result.stdout


def kubectl(*args, visible=True):
    return run(["kalisto", "kubectl", "--name", os.environ["DEMO_CLUSTER"], "--", *args],
               visible=visible)


def inspect_nodes(count):
    kubectl("wait", "--for=condition=Ready", "nodes", "--all", "--timeout=180s")
    nodes = json.loads(kubectl("get", "nodes", "-o", "json", visible=False))["items"]
    if len(nodes) != count or not all(any(c["type"] == "Ready" and c["status"] == "True"
                                         for c in n["status"]["conditions"]) for n in nodes):
        raise SceneError("Node count or readiness assertion failed")
    kubectl("get", "nodes")


def inspect_app(replicas):
    kubectl("rollout", "status", "deployment/kalisto-http", "--timeout=180s")
    pods = json.loads(kubectl("get", "pods", "-l", "app=kalisto-http", "-o", "json",
                             visible=False))["items"]
    ready = [p for p in pods if not p["metadata"].get("deletionTimestamp") and
             any(c["type"] == "Ready" and c["status"] == "True"
                 for c in p.get("status", {}).get("conditions", []))]
    if len(ready) != replicas or len({p["spec"]["nodeName"] for p in ready}) != replicas:
        raise SceneError("Workload replicas must be Ready on distinct workers")
    kubectl("get", "pods", "-l", "app=kalisto-http", "-o", "wide")
    kubectl("get", "service", "kalisto-http")


def http_response():
    # Keep the managed wrapper and its kubectl child in one owned process group.
    with socket.socket() as reservation:
        reservation.bind(("127.0.0.1", 0))
        port = reservation.getsockname()[1]
    command = ["kalisto", "kubectl", "--name", os.environ["DEMO_CLUSTER"], "--",
               "port-forward", "--address=127.0.0.1", "service/kalisto-http", f"{port}:8080"]
    print("$ " + shlex.join(command) + " &", flush=True)
    log = Path(os.environ["DEMO_PRIVATE"]) / (sys.argv[1] + "-port-forward.log")
    owner = Path(os.environ["DEMO_PRIVATE"]) / (sys.argv[1] + "-process.json")
    with log.open("wb") as output:
        child = subprocess.Popen(command, stdout=output, stderr=subprocess.STDOUT,
                                 start_new_session=True)
        try:
            started = subprocess.run(["/bin/ps", "-p", str(child.pid), "-o", "lstart="],
                                     stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
            if started.returncode:
                raise SceneError("Cannot establish owned port-forward process identity")
            owner.write_text(json.dumps({"pid": child.pid, "started": started.stdout.decode().strip()}))
            url = f"http://127.0.0.1:{port}/"
            deadline = time.monotonic() + 60
            while True:
                if child.poll() is not None:
                    raise SceneError("Owned port-forward exited before HTTP readiness")
                try:
                    with urllib.request.urlopen(url, timeout=2) as response:
                        response.read()
                    break
                except (urllib.error.URLError, TimeoutError, ConnectionError):
                    if time.monotonic() >= deadline:
                        raise SceneError("HTTP readiness timed out") from None
                    time.sleep(0.25)
            args = ["curl", "--fail", "--silent", "--show-error", "--max-time", "10", url]
            print("$ " + shlex.join(args), flush=True)
            response = subprocess.run(args, stdout=subprocess.PIPE, check=False)
            sys.stdout.buffer.write(response.stdout)
            sys.stdout.flush()
            if response.returncode:
                raise SystemExit(response.returncode)
            if response.stdout != b"Hello from Kalisto. This workload survives a cluster restart.\n":
                raise SceneError("The real HTTP response did not match the fixture")
        finally:
            try:
                os.killpg(child.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                child.wait(timeout=10)
            except subprocess.TimeoutExpired:
                os.killpg(child.pid, signal.SIGKILL)
                child.wait()
            owner.unlink(missing_ok=True)


def main():
    name = os.environ["DEMO_CLUSTER"]
    profile = json.loads(os.environ["DEMO_PROFILE_JSON"])
    scene = sys.argv[1]
    print("# Scripted execution; every command runs against this job's owned cluster.", flush=True)
    if scene == "start":
        print("# Images and tools were prepared with kalisto pull before recording.", flush=True)
        run(["kalisto", "up", *profile])
        kubectl("get", "--raw=/readyz")
        print(flush=True)
        inspect_nodes(3)
        for rollout in json.loads(Path("rollouts.json").read_text()):
            kubectl("-n", rollout["namespace"], "rollout", "status", rollout["resource"],
                    "--timeout=180s")
    elif scene == "deploy":
        run(["kalisto", "apply", "--name", name, "--manifest", "app.yaml"])
        inspect_app(2)
        http_response()
    elif scene == "resume":
        kubectl("create", "configmap", "demo-state", "--from-literal=value=saved-before-stop")
        before = json.loads(kubectl("get", "configmap", "demo-state", "-o", "json", visible=False))
        run(["kalisto", "down", "--name", name])
        run(["kalisto", "status", "--name", name])
        stopped = json.loads(run(["kalisto", "--json", "status", "--name", name], visible=False))
        if len(stopped["nodes"]) != 3 or any("Stopped" not in n["detail"].split()
                                               for n in stopped["nodes"]):
            raise SceneError("All owned nodes must actually stop before resuming")
        run(["kalisto", "up", *profile])
        inspect_nodes(3)
        after = json.loads(kubectl("get", "configmap", "demo-state", "-o", "json", visible=False))
        if before["metadata"]["uid"] != after["metadata"]["uid"] or after["data"] != before["data"]:
            raise SceneError("Saved ConfigMap identity and contents did not persist")
        kubectl("get", "configmap", "demo-state", "-o", "jsonpath={.data.value}")
        print(flush=True)
        inspect_app(2)
        http_response()
    elif scene == "scale":
        inspect_nodes(3)
        run(["kalisto", "scale", "--name", name, "--workers", "3"])
        inspect_nodes(4)
        kubectl("scale", "deployment/kalisto-http", "--replicas=3")
        inspect_app(3)
        http_response()
    else:
        raise SceneError("Unknown scene")
    # Written only after every real command and assertion succeeds. asciinema's
    # --return independently propagates this process's original failure status.
    (Path(os.environ["DEMO_PRIVATE"]) / (scene + ".success")).touch(exist_ok=False)


if __name__ == "__main__":
    try:
        main()
    except SceneError as error:
        print(str(error), file=sys.stderr)
        sys.exit(1)
    except (OSError, ValueError, KeyError):
        # Never put a traceback containing runner paths into the terminal.
        print("Scene infrastructure or response parsing failed; inspect private CI evidence.", file=sys.stderr)
        sys.exit(1)
