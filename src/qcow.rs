//! Read-only normalization of the pinned QCOW2 seed into a sparse raw disk.
//!
//! No QEMU executable is needed on the user's Mac. Supported subset: QCOW2 v2/v3,
//! standard L2 entries, no backing/encryption/incompatible features, raw DEFLATE.
//! Format: https://www.qemu.org/docs/master/interop/qcow2.html

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use flate2::read::DeflateDecoder;

use crate::error::{Error, Result};

const OFFSET: u64 = 0x00ff_ffff_ffff_fe00;
const COMPRESSED: u64 = 1 << 62;
const MAX_DISK: u64 = 16 * 1024 * 1024 * 1024;

pub fn to_raw(source: &Path, destination: &Path) -> Result<()> {
    let mut input = File::open(source).map_err(io_error)?;
    let source_size = input.metadata().map_err(io_error)?.len();
    let mut header = [0u8; 104];
    input.read_exact(&mut header).map_err(io_error)?;
    if &header[..4] != b"QFI\xfb" || !matches!(u32_at(&header, 4), 2 | 3) {
        return Err(invalid("unsupported node seed format"));
    }
    let bits = u32_at(&header, 20);
    let size = u64_at(&header, 24);
    let entries = u32_at(&header, 36) as u64;
    if !(9..=21).contains(&bits)
        || size == 0
        || size > MAX_DISK
        || u64_at(&header, 8) != 0
        || u32_at(&header, 32) != 0
        || (u32_at(&header, 4) == 3 && u64_at(&header, 72) != 0)
    {
        return Err(invalid("unsupported QCOW2 seed features or size"));
    }
    let cluster = 1u64 << bits;
    let per_l2 = cluster / 8;
    let clusters = size.div_ceil(cluster);
    let needed_l1 = clusters.div_ceil(per_l2);
    if entries < needed_l1 || entries > MAX_DISK / 512 {
        return Err(invalid("invalid QCOW2 L1 size"));
    }
    let l1_offset = u64_at(&header, 40);
    let mut l1 = vec![0; (needed_l1 * 8) as usize];
    read_at(&mut input, source_size, l1_offset, &mut l1)?;
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(io_error)?;
    let result = (|| {
        output.set_len(size).map_err(io_error)?;
        let mut l2 = vec![0; cluster as usize];
        let mut data = vec![0; cluster as usize];
        for first in 0..needed_l1 {
            let table = u64_at(&l1, (first * 8) as usize) & OFFSET;
            if table == 0 {
                continue;
            }
            read_at(&mut input, source_size, table, &mut l2)?;
            for second in 0..per_l2 {
                let index = first * per_l2 + second;
                if index >= clusters {
                    break;
                }
                let descriptor = u64_at(&l2, (second * 8) as usize);
                if descriptor == 0 {
                    continue;
                }
                if descriptor & COMPRESSED != 0 {
                    let shift = 62 - (bits - 8);
                    let offset = descriptor & ((1u64 << shift) - 1);
                    let sectors = ((descriptor >> shift) & ((1u64 << (bits - 8)) - 1)) + 1;
                    let length = sectors * 512 - (offset & 511);
                    let available = source_size
                        .checked_sub(offset)
                        .ok_or_else(|| invalid("QCOW2 compressed offset exceeds input"))?;
                    let mut compressed = vec![0; length.min(available) as usize];
                    read_at(&mut input, source_size, offset, &mut compressed)?;
                    DeflateDecoder::new(compressed.as_slice())
                        .read_exact(&mut data)
                        .map_err(io_error)?;
                } else {
                    let offset = descriptor & OFFSET;
                    if descriptor & 1 != 0 || offset == 0 {
                        continue;
                    }
                    read_at(&mut input, source_size, offset, &mut data)?;
                }
                let count = cluster.min(size - index * cluster) as usize;
                if data[..count].iter().any(|byte| *byte != 0) {
                    output
                        .seek(SeekFrom::Start(index * cluster))
                        .map_err(io_error)?;
                    output.write_all(&data[..count]).map_err(io_error)?;
                }
            }
        }
        output.sync_all().map_err(io_error)
    })();
    drop(output);
    if result.is_err() {
        let _ = fs::remove_file(destination);
    }
    result
}

fn read_at(file: &mut File, size: u64, offset: u64, bytes: &mut [u8]) -> Result<()> {
    if offset
        .checked_add(bytes.len() as u64)
        .is_none_or(|end| end > size)
    {
        return Err(invalid("QCOW2 extent exceeds input"));
    }
    file.seek(SeekFrom::Start(offset)).map_err(io_error)?;
    file.read_exact(bytes).map_err(io_error)
}

fn u32_at(data: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(
        data[offset..offset + 4]
            .try_into()
            .expect("fixed-width QCOW2 field"),
    )
}
fn u64_at(data: &[u8], offset: usize) -> u64 {
    u64::from_be_bytes(
        data[offset..offset + 8]
            .try_into()
            .expect("fixed-width QCOW2 field"),
    )
}
fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidArgs(message.into())
}
fn io_error(source: std::io::Error) -> Error {
    Error::CommandIo {
        program: "QCOW2 seed normalization".into(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flate2::{Compression, write::DeflateEncoder};

    use super::*;

    fn fixture(compressed: bool) -> Vec<u8> {
        let mut bytes = vec![0; 2048];
        bytes[..4].copy_from_slice(b"QFI\xfb");
        bytes[4..8].copy_from_slice(&2u32.to_be_bytes());
        bytes[20..24].copy_from_slice(&9u32.to_be_bytes());
        bytes[24..32].copy_from_slice(&1024u64.to_be_bytes());
        bytes[36..40].copy_from_slice(&1u32.to_be_bytes());
        bytes[40..48].copy_from_slice(&512u64.to_be_bytes());
        bytes[512..520].copy_from_slice(&1024u64.to_be_bytes());
        let descriptor = if compressed {
            let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(&vec![b'A'; 512]).unwrap();
            let data = encoder.finish().unwrap();
            // Compressed clusters may begin in the middle of a sector.
            bytes[1543..1543 + data.len()].copy_from_slice(&data);
            COMPRESSED | 1543
        } else {
            bytes[1536..2048].fill(b'A');
            (1u64 << 63) | 1536
        };
        bytes[1024..1032].copy_from_slice(&descriptor.to_be_bytes());
        bytes
    }

    #[test]
    fn normalizes_standard_and_unaligned_compressed_clusters_and_sparse_holes() {
        let root = std::env::temp_dir().join(format!("kalisto-qcow-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        for compressed in [false, true] {
            let source = root.join(format!("{compressed}.qcow2"));
            let destination = root.join(format!("{compressed}.raw"));
            fs::write(&source, fixture(compressed)).unwrap();
            to_raw(&source, &destination).unwrap();
            let result = fs::read(destination).unwrap();
            assert_eq!(&result[..512], vec![b'A'; 512]);
            assert_eq!(&result[512..], vec![0; 512]);
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn invalid_extent_is_rejected_before_reading() {
        let root =
            std::env::temp_dir().join(format!("kalisto-qcow-invalid-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let source = root.join("seed.qcow2");
        let destination = root.join("disk.raw");
        let mut bytes = fixture(false);
        bytes[40..48].copy_from_slice(&u64::MAX.to_be_bytes());
        fs::write(&source, bytes).unwrap();
        assert!(to_raw(&source, &destination).is_err());
        assert!(!destination.exists());
        fs::remove_dir_all(root).unwrap();
    }
}
