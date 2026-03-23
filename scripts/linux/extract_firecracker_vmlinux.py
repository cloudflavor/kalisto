#!/usr/bin/env python3
import pathlib
import shutil
import subprocess
import sys
import tempfile


MAGICS = {
    "gzip": (b"\x1f\x8b\x08", ["gzip", "-cd"]),
    "xz": (b"\xfd7zXZ\x00", ["xz", "-cd"]),
    "zstd": (b"\x28\xb5\x2f\xfd", ["zstd", "-cdq"]),
    "lz4": (b"\x04\x22\x4d\x18", ["lz4", "-cd"]),
    "bzip2": (b"BZh", ["bzip2", "-cd"]),
}


def main() -> int:
    if len(sys.argv) != 3:
        print(
            "usage: extract_firecracker_vmlinux.py <kernel-image> <output-vmlinux>",
            file=sys.stderr,
        )
        return 1

    source = pathlib.Path(sys.argv[1])
    destination = pathlib.Path(sys.argv[2])
    data = source.read_bytes()

    if data.startswith(b"\x7fELF"):
        destination.write_bytes(data)
        return 0

    candidates = []
    for compression, (magic, command) in MAGICS.items():
        start = 0
        while True:
            offset = data.find(magic, start)
            if offset < 0:
                break
            candidates.append((offset, compression, command))
            start = offset + 1

    candidates.sort(key=lambda candidate: candidate[0])
    for offset, compression, command in candidates:
        if shutil.which(command[0]) is None:
            continue
        with tempfile.TemporaryDirectory(prefix="kalisto-vmlinux-") as temp_dir:
            payload = pathlib.Path(temp_dir) / "payload.bin"
            output = pathlib.Path(temp_dir) / "vmlinux"
            payload.write_bytes(data[offset:])
            try:
                with output.open("wb") as handle:
                    subprocess.run(
                        command + [str(payload)],
                        check=True,
                        stdout=handle,
                        stderr=subprocess.DEVNULL,
                    )
            except subprocess.CalledProcessError:
                continue
            extracted = output.read_bytes()
            if extracted.startswith(b"\x7fELF"):
                destination.write_bytes(extracted)
                return 0

    print(
        f"unable to extract an uncompressed ELF kernel from {source}",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
