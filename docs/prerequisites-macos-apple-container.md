# macOS Apple-container prerequisites

The default macOS backend requires **Apple's `container` CLI installed and its
system service running before `kalisto pull` or `kalisto up`**. Kalisto runs its
own k3s nodes on that runtime; Apple's experimental Kubernetes implementation is
not required.

## Requirements

- A Mac with Apple silicon and macOS 26 or newer.
- A compatible Apple `container` installation available on `PATH`. The validated
  runtime version is **1.7.1**.
- The Apple container system service running in your macOS login session.
- Network access for initial runtime/kernel setup and `kalisto pull` preparation.
  Complete both before measuring offline cluster startup.

Intel Macs and older macOS versions must select the
[optional Lima backend](https://cloudflavor.io/kalisto/docs/lima/) explicitly. These
Apple-container prerequisites do not apply to Lima or Linux Firecracker.

## Install and start Apple container

For a new Homebrew installation:

```bash
brew install container
container system start
container --version
container list
```

`container list` must succeed, even if no containers exist. If a compatible CLI
and running service are already available, skip installation/start instead of
reinstalling or restarting them. Homebrew users who manage the service with
`brew services` should also read the formula's recommended-kernel setup caveat.

See the [Homebrew formula](https://formulae.brew.sh/formula/container) and
[Apple's requirements and service setup](https://github.com/apple/container#requirements).

## Prepare Kalisto after the runtime is ready

Install the standalone Kalisto binary, then follow the
[quick start](https://cloudflavor.io/kalisto/docs/getting-started/). `kalisto pull` prepares the Linux
kernel/init, node image, Kubernetes images and managed tools for the selected
profile. `kalisto up` does not install the host engine or download missing startup
assets. Runtime installation, service setup and preparation are outside the
three-node startup measurement.

Fresh Apple-container nodes use containerd's OverlayFS snapshotter on the pinned
Kata kernel, sharing immutable image layers rather than copying Cilium's full
root filesystem for each init container. After upgrading from a native-snapshotter
build, run `kalisto pull` again to prepare the revised node-image profile before
creating a fresh cluster. Existing node disks are not converted in place.

## State directory symlinks

The user data directory may be symlinked to another volume. Apple-container
bind mounts use the canonical directory; Kalisto resolves host file aliases
before staging private manifests into that mount. A failed startup retains its
definition and node disks: resume it with `kalisto up --name <cluster>` and the
same profile and API port rather than deleting or recreating it.

## Existing services, upgrades and firewalls

Kalisto reuses a compatible installed CLI and shared running service without
replacing their executable, configuration, application root or data. It refuses
to take over an incompatible running daemon. Coordinate any engine upgrade or
service restart separately with users of existing clusters and containers.

A user-owned helper left running from another installation can be reused when it
shares the API service's application root, runs its expected packaged executable,
and belongs to a complete installation with the supported release CLI version.
Kalisto validates that helper independently; it does not re-register it or rewrite
its installation root. Managed services and the API daemon still require their
selected installation identity.

Upgrading the CLI or API daemon does not prove that an already-running image
helper was upgraded. An older helper, such as 0.10.0 beside a 1.7.1 API daemon,
is rejected with its version before newer-installation files are inspected.
Align shared services separately only after checking active workloads and
image-store compatibility; Kalisto never performs that shared-service upgrade.

Application firewalls such as LuLu may require explicit allow rules for
Homebrew's `container-core-images` and `container-runtime-linux` helpers after an
upgrade. Approve only the required helpers; do not disable the firewall.
