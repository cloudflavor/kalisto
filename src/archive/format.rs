//! Versioned, checksummed cold archives. Never unpack untrusted tar paths.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use flate2::Compression;
use flate2::bufread::GzDecoder;
use flate2::write::GzEncoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::crypto::{ExportEncryption, ImportDecryption};
use crate::error::{Error, Result};

pub const DEFAULT_MAX_BYTES: u64 = 1024 * 1024 * 1024 * 1024;
const MAX_ENTRIES: usize = 512;
const MAX_MANIFEST: u64 = 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Payload {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema: u32,
    pub architecture: String,
    pub lima_version: String,
    pub contains_secrets: bool,
    pub state: serde_json::Value,
    pub files: Vec<Payload>,
}

/// Task-owned staging only. Existing directories are never adopted or removed.
pub struct Scratch(pub PathBuf);
impl Scratch {
    pub fn new(parent: &Path) -> Result<Self> {
        fs::create_dir_all(parent).map_err(io_error)?;
        for attempt in 0..16 {
            let path = parent.join(format!(
                ".kalisto-archive-{}-{}-{attempt}",
                std::process::id(),
                nonce()?
            ));
            match fs::create_dir(&path) {
                Ok(()) => {
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
                        .map_err(io_error)?;
                    return Ok(Self(path));
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(io_error(error)),
            }
        }
        Err(invalid("cannot allocate private archive staging"))
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub fn private_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(io_error)
}

pub fn valid_path(path: &str) -> bool {
    if path.len() > 99 || path.contains('\\') || path.contains('\0') {
        return false;
    }
    let parts = path.split('/').collect::<Vec<_>>();
    if parts
        .iter()
        .any(|part| part.is_empty() || matches!(*part, "." | ".."))
    {
        return false;
    }
    match parts.as_slice() {
        ["base-image"] => true,
        ["assets", file] => safe_basename(file),
        ["nodes", index, "disk" | "vz-efi"] => valid_index(index),
        ["runtime", index, file] => {
            valid_index(index)
                && (matches!(*file, "kubeconfig.yaml" | "node-token")
                    || (file.starts_with("cilium-")
                        && file.ends_with(".json")
                        && safe_basename(file)))
        }
        _ => false,
    }
}
fn safe_basename(name: &str) -> bool {
    !name.is_empty()
        && !matches!(name, "." | "..")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
}
fn valid_index(index: &str) -> bool {
    index
        .parse::<usize>()
        .is_ok_and(|n| n < 64 && n.to_string() == index)
}

#[cfg(test)]
pub fn write(output: &Path, manifest: Manifest, sources: &[(String, PathBuf)]) -> Result<()> {
    write_protected(output, manifest, sources, &ExportEncryption::Plaintext)
}

pub fn write_protected(
    output: &Path,
    manifest: Manifest,
    sources: &[(String, PathBuf)],
    protection: &ExportEncryption,
) -> Result<()> {
    if fs::symlink_metadata(output).is_ok() {
        return Err(invalid("archive output already exists; refusing overwrite"));
    }
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let scratch = Scratch::new(parent)?;
    let temporary = scratch.0.join("archive.partial");
    let mut output_file = HashWriter {
        file: private_file(&temporary)?,
        hash: Sha256::new(),
    };
    match protection.encryptor()? {
        Some(encryptor) => {
            let mut writer = encryptor.wrap_output(&mut output_file).map_err(io_error)?;
            write_contents(&mut writer, manifest, sources)?;
            writer.finish().map_err(io_error)?;
        }
        None => write_contents(&mut output_file, manifest, sources)?,
    }
    output_file.file.sync_all().map_err(io_error)?;
    let expected = output_file.hash.finalize();
    // Verify stored ciphertext without adding an undeclared verification recipient
    // or ever materializing a plaintext archive. Secret keys are not needed here.
    let mut check = HashReader {
        file: File::open(&temporary).map_err(io_error)?,
        hash: Sha256::new(),
    };
    io::copy(&mut check, &mut io::sink()).map_err(io_error)?;
    if check.hash.finalize() != expected {
        return Err(invalid("archive storage integrity readback failed"));
    }
    fs::hard_link(&temporary, output).map_err(io_error)?;
    Ok(())
}

fn write_contents(
    writer: impl Write,
    mut manifest: Manifest,
    sources: &[(String, PathBuf)],
) -> Result<()> {
    let encoder = GzEncoder::new(writer, Compression::fast());
    let mut tar = tar::Builder::new(encoder);
    tar.sparse(false);
    let mut before = Vec::new();
    let mut records = BTreeMap::new();
    for (name, source) in sources {
        if !valid_path(name) || records.contains_key(name) || sources.len() >= MAX_ENTRIES {
            return Err(invalid("invalid or duplicate archive payload name"));
        }
        let metadata = fs::symlink_metadata(source).map_err(io_error)?;
        if !metadata.is_file() {
            return Err(invalid(format!(
                "archive source must be a regular file: {}",
                source.display()
            )));
        }
        tracing::info!(
            payload = name,
            bytes = metadata.len(),
            "archiving cluster data"
        );
        let mut reader = HashReader {
            file: File::open(source).map_err(io_error)?,
            hash: Sha256::new(),
        };
        let mut header = header(metadata.len());
        tar.append_data(&mut header, name, &mut reader)
            .map_err(io_error)?;
        records.insert(
            name.clone(),
            Payload {
                path: name.clone(),
                bytes: metadata.len(),
                sha256: format!("{:x}", reader.hash.finalize()),
            },
        );
        before.push((
            source,
            metadata.len(),
            metadata.modified().map_err(io_error)?,
        ));
    }
    for (path, size, modified) in before {
        let metadata = fs::symlink_metadata(path).map_err(io_error)?;
        if !metadata.is_file()
            || metadata.len() != size
            || metadata.modified().map_err(io_error)? != modified
        {
            return Err(invalid(
                "cluster data changed during export; archive was not published",
            ));
        }
    }
    manifest.files = records.into_values().collect();
    let bytes = serde_json::to_vec(&manifest).map_err(|error| invalid(error.to_string()))?;
    if bytes.len() as u64 > MAX_MANIFEST {
        return Err(invalid("archive manifest too large"));
    }
    tar.append_data(
        &mut header(bytes.len() as u64),
        "manifest.json",
        bytes.as_slice(),
    )
    .map_err(io_error)?;
    let encoder = tar.into_inner().map_err(io_error)?;
    encoder.finish().map_err(io_error)?;
    Ok(())
}

#[cfg(test)]
pub fn read(input: &Path, stage: &Path, max_bytes: u64) -> Result<Manifest> {
    read_protected(input, stage, max_bytes, &ImportDecryption::None)
}

pub fn read_protected(
    input: &Path,
    stage: &Path,
    max_bytes: u64,
    credentials: &ImportDecryption,
) -> Result<Manifest> {
    read_impl(credentials.reader(input)?, Some(stage), max_bytes)
}

pub fn verify(input: &Path, max_bytes: u64) -> Result<Manifest> {
    read_impl(ImportDecryption::None.reader(input)?, None, max_bytes)
}

fn read_impl(reader: impl Read, stage: Option<&Path>, max_bytes: u64) -> Result<Manifest> {
    let decoder = GzDecoder::new(BufReader::new(reader));
    let mut tar = tar::Archive::new(decoder);
    let mut actual = BTreeMap::new();
    let mut manifest = None;
    let mut total = 0u64;
    for entry in tar.entries().map_err(io_error)?.raw(true) {
        let mut entry = entry.map_err(io_error)?;
        if manifest.is_some() {
            return Err(invalid("archive manifest must be the final entry"));
        }
        if !entry.header().entry_type().is_file() {
            return Err(invalid(
                "archive links, special files and extension headers are forbidden",
            ));
        }
        let raw = entry.path_bytes();
        let name = std::str::from_utf8(raw.as_ref())
            .map_err(|_| invalid("archive paths must be UTF-8"))?
            .to_owned();
        let size = entry.size();
        if name == "manifest.json" {
            if size > MAX_MANIFEST {
                return Err(invalid("archive manifest too large"));
            }
            let mut data = Vec::new();
            entry.read_to_end(&mut data).map_err(io_error)?;
            manifest = Some(
                serde_json::from_slice::<Manifest>(&data)
                    .map_err(|error| invalid(format!("invalid archive manifest: {error}")))?,
            );
            continue;
        }
        if !valid_path(&name) || actual.contains_key(&name) || actual.len() >= MAX_ENTRIES {
            return Err(invalid("unsafe, unsupported or duplicate archive path"));
        }
        total = total
            .checked_add(size)
            .filter(|bytes| *bytes <= max_bytes)
            .ok_or_else(|| invalid("archive exceeds uncompressed size limit"))?;
        tracing::info!(payload = name, bytes = size, "verifying archive payload");
        let mut file = if let Some(stage) = stage {
            let destination = stage.join(&name);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent).map_err(io_error)?;
            }
            let file = private_file(&destination)?;
            file.set_len(size).map_err(io_error)?;
            Some(file)
        } else {
            None
        };
        let mut hash = Sha256::new();
        let mut buffer = vec![0; 1024 * 1024];
        let mut copied = 0u64;
        loop {
            let count = entry.read(&mut buffer).map_err(io_error)?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
            // Preserve zero-filled disk regions as holes instead of allocating
            // many GiB of empty space on the receiving filesystem.
            if let Some(file) = &mut file {
                if buffer[..count].iter().any(|byte| *byte != 0) {
                    file.write_all(&buffer[..count]).map_err(io_error)?;
                } else {
                    file.seek(SeekFrom::Current(count as i64))
                        .map_err(io_error)?;
                }
            }
            copied += count as u64;
        }
        if copied != size {
            return Err(invalid("truncated archive payload"));
        }
        if let Some(file) = file {
            file.sync_all().map_err(io_error)?;
        }
        actual.insert(
            name.clone(),
            Payload {
                path: name,
                bytes: size,
                sha256: format!("{:x}", hash.finalize()),
            },
        );
    }
    // Drain the gzip member to verify its CRC, while accepting only bounded tar
    // padding. Reject concatenated members and bytes following the gzip member.
    let mut decoder = tar.into_inner();
    let mut padding = Vec::new();
    decoder
        .by_ref()
        .take(65537)
        .read_to_end(&mut padding)
        .map_err(io_error)?;
    if padding.len() > 65536 || padding.iter().any(|byte| *byte != 0) {
        return Err(invalid("unexpected data after tar end"));
    }
    if decoder.into_inner().read(&mut [0; 1]).map_err(io_error)? != 0 {
        return Err(invalid("trailing gzip members/data are forbidden"));
    }
    let manifest = manifest.ok_or_else(|| invalid("archive manifest missing"))?;
    if manifest.schema != 1 || !manifest.contains_secrets {
        return Err(invalid("unsupported archive schema"));
    }
    let mut expected = BTreeMap::new();
    for record in &manifest.files {
        if !valid_path(&record.path)
            || expected
                .insert(record.path.clone(), record.clone())
                .is_some()
        {
            return Err(invalid("invalid archive manifest inventory"));
        }
    }
    if expected != actual {
        return Err(invalid("archive checksum/inventory mismatch"));
    }
    Ok(manifest)
}

fn header(size: u64) -> tar::Header {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_mode(0o600);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_size(size);
    header.set_cksum();
    header
}
struct HashWriter {
    file: File,
    hash: Sha256,
}
impl Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let count = self.file.write(bytes)?;
        self.hash.update(&bytes[..count]);
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}
struct HashReader {
    file: File,
    hash: Sha256,
}
impl Read for HashReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.file.read(buffer)?;
        self.hash.update(&buffer[..count]);
        Ok(count)
    }
}
pub fn nonce() -> Result<u128> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|time| time.as_nanos())
        .map_err(|error| invalid(error.to_string()))
}
pub fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidArgs(message.into())
}
pub fn io_error(source: io::Error) -> Error {
    Error::CommandIo {
        program: "cluster archive".into(),
        source,
    }
}
