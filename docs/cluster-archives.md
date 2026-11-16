# Cold cluster archives

`kalisto export` / `kalisto import` preserve VM-local cluster state, including
installed workloads and data. These are cold disk archives, not hot snapshots,
process migration, or sanitized public templates.

## Export

```bash
kalisto export --name dev --output /secure/dev.kalisto
# Authorize a clean OS shutdown first if nodes are running:
kalisto export --name dev --output /secure/dev.kalisto --stop
```

Without `--stop`, every node must already be stopped. Export never force-stops a VM
and never deletes source data. It leaves the source stopped. Quiesce applications
externally when application-consistent backups matter: stopped disks alone do not
provide that guarantee, especially after a previous forced shutdown.

The archive includes:

- Every node's main VM disk and optional VZ EFI variable store.
- Kubernetes datastore resources, Helm releases, CRDs, container images/cache and
  persistent data **inside those disks**.
- Selected private runtime files: kubeconfigs, node tokens and per-cluster Cilium
  TLS manifests.
- The matching boot-asset profile and the sanitized base image itself.

It does not capture RAM, live processes, connections, external databases or cloud
services. Custom provisioning, additional VM disks, external host mounts and unknown
host-runtime files are refused rather than silently omitted. Back up external data
separately. PIDs and logs are deliberately excluded.

## Native age encryption

No GPG, `age` executable, plugins or host crypto tools are required. Kalisto embeds
[age 0.11.5](https://docs.rs/age/0.11.5/age/), using the standard binary age format
around the gzip archive. Native X25519 age keys are supported; SSH/GPG/plugin keys,
armored files and encrypted identity-file wrappers are not supported in this version.

```bash
kalisto archive-keygen --output /secure/dev.agekey
# Replace age1... with the public recipient printed by keygen:
kalisto export --name dev --output /secure/dev.kalisto.age --recipient age1... --stop
kalisto import /secure/dev.kalisto.age --identity /secure/dev.agekey
```

`archive-keygen` creates a mode-0600 private key file atomically without overwriting,
and prints only the public recipient. Repeat `--recipient` to encrypt for multiple
recovery keys; any one recipient can decrypt. Repeat `--identity` for multiple key
files. Keep recovery keys separately from archives. Lost keys mean lost access.

Passphrase alternative:

```bash
kalisto export --name dev --output /secure/dev.kalisto.age --passphrase --stop
kalisto import /secure/dev.kalisto.age --passphrase
```

The terminal prompt never echoes input; export asks for confirmation. At least 16
characters are required for export, but length alone does not imply strength: prefer
randomly generated words. Native passphrase exports use scrypt with a 256 MiB memory
cost; imports cap accepted scrypt cost at roughly 1 GiB. Recipient keys avoid this KDF.

For noninteractive use, pass `--passphrase-file /private/path` instead. Secret input
files must be private regular files (0600/0400), UTF-8, and at most 64 KiB. Passphrase
files contain one nonempty line, optionally newline-terminated. Recipients/identities
and passphrases are mutually exclusive. Raw passphrases/private keys are never CLI
argument values. Secret buffers are zeroized on drop; do not put secrets in filenames.

No encryption flags preserves legacy **unencrypted** export behavior. A `.age` output
suffix without encryption is refused. Import detects encryption by header, not suffix;
if credentials are supplied or input ends in `.age`, plaintext is rejected to prevent
silent downgrade. No fallback follows a decryption/authentication failure.

Tar/gzip streams directly into age without a plaintext archive intermediate. Export
reads back the stored ciphertext with SHA-256 before atomic publication, without
adding an undeclared recipient or retaining an extra recovery key. Import authenticates
the full age stream, gzip CRC and payload inventory before publishing cluster/VM
resources. Decrypted VM disks still stage privately on the target; encryption does
not encrypt live VM storage. Use secure host storage for staging, swap and runtime.

## Import

```bash
kalisto import /secure/dev.kalisto
# Optional API port change on the target:
kalisto import /secure/dev.kalisto --api-port 16444
# After successful import, explicitly boot the restored cluster:
kalisto up --name dev
```

The two `import` lines are alternatives, not consecutive steps. Import preserves the
original cluster/node identities; renaming is not supported. It requires compatible
CPU architecture and the pinned Lima version, on macOS native VZ. It refuses existing
cluster directories, Lima instances and archive output paths. It never deletes an
existing source cluster to make room.

Use another compatible Mac, or remove the original only through an explicitly
authorized cleanup after a safe backup. Do not run original/restored copies against
shared external services without considering their duplicated identities.

Import verifies payload checksums before publishing definitions. It restores sparse
disk regions without allocating their empty logical space, rebuilds host paths and
narrow mounts, and regenerates host-local VZ identifiers. Archived host configuration,
provision scripts, SSH private keys, sockets and cloud-init media are not executed
or restored. Lima regenerates host integration at the next boot. `up` refreshes the
host kubeconfig endpoint and waits for current guest boot identities, not just old
Kubernetes Node Ready conditions stored in the snapshot.

Import leaves nodes **stopped**. Boot assets are bundled, but verified managed host
tools may download on an unprepared target. The default uncompressed payload limit
is 1 TiB; `--max-bytes` changes it. Export/import time is outside the prepared-boot
benchmark, and restored-cluster startup has no separate sub-minute guarantee.

## Secrets, integrity and interruption

**Archives contain cluster credentials, Kubernetes Secrets and application data.
Encrypt before transport and keep recovery keys separate. Unencrypted exports remain
available when no encryption flags are supplied. Never commit real archives or keys.**
New archives/keys use mode `0600`, staging/destination roots `0700`. `*.kalisto`,
`*.kalisto.age` and `*.agekey` are gitignored; other output names still need care.

Only import trusted archives. Import itself never starts guest code; subsequent
`up` runs the archived operating systems. SHA-256 payload checksums and gzip CRC
validation detect corruption, not a malicious author who can replace both payloads
and checksums. Age adds authenticated encryption, not a sender signature: anyone
with the public recipient can create a new encrypted archive. Export verifies stored
bytes on readback; legacy plaintext export also checks tar/gzip on readback.

Tar paths/types, duplicates, file count, sizes and the complete inventory are checked.
Links, device files and extension headers are rejected. No existing file is overwritten.
Per-cluster locks exclude normal Kalisto lifecycle mutations during the operation;
do not manipulate these VMs directly through Lima or deploy changes concurrently.

Normal import failures clean only destinations created by that import. A hard-killed
or interrupted import can leave a private incomplete-import marker and owned partial
directories. The marker blocks automatic cluster creation/overwrite. Inspect these
resources before cleanup; never blindly delete unrelated data or locks.

## MCP and validation

`clusters.export` accepts `name`, `output`, `stop`, optional `recipients` or
`passphraseFile`; `clusters.import` accepts `input`, `apiPort`, `maxBytes`, optional
`identityFiles` or `passphraseFile`. Files are local to the MCP host, not the client.
Never send inline secret keys/passphrases: unsupported fields and malformed recipients
are rejected before task JSON/manifests are persisted. MCP never prompts on a terminal.
Both use the same core implementation as the CLI and require
task-augmented calls. Archive tasks are not cancellable while copying/publishing;
poll to a terminal result instead of killing a restore transaction.

Host-safe unit checks cover format round trips, corrupt/truncated input, links,
traversal, duplicates, limits, collisions, credential/data preservation using fake
VM disks, and operation locks. Encryption tests add standard-age interoperability,
multiple recipients, randomized ciphertext, wrong keys/passphrases, tampering,
truncation, trailing data, private key generation, secret-file limits, plaintext
downgrade refusal and no publication after authentication failure. CLI/MCP argument
coverage excludes inline secrets. These new checks have not run locally; CI is pending.
They do not establish native VM boot compatibility or constitute a security audit.
The native round-trip test is opt-in:

```bash
KALISTO_LIVE_CLUSTER_ARCHIVE_TESTS=1 \
KALISTO_ARCHIVE_TEST_ROOT=/absolute/path/to/owned-ci-storage \
  cargo test --release --test macos_cluster_archive_live -- --nocapture
```

It requires a macOS CI runner with `kubectl`, uses disposable named VMs, and checks
Kubernetes objects and worker-local data after encrypted export/destroy/import/boot.
Sensitive archives and generated recovery keys are deleted, not published as CI artifacts. On the development Mac, test
storage and caches must use the mounted external build volume. Respect the user's
execution policy; do not run this live test without explicit authorization.

Native restore evidence is pending; no native archive CI success is claimed.
See the canonical [archive guide](https://cloudflavor.io/kalisto/docs/archives/)
and [AI agent guidance](https://cloudflavor.io/kalisto/docs/agents/).
