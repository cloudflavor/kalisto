//! Read-only loopback transport for prepared OCI archives and layouts.
//!
//! Apple's image-load service extracts archives into its own temporary directory.
//! Serving the existing external-volume archive avoids that extra copy without
//! changing the user's running image helper or its temporary-directory settings.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::error::{Error, Result};

const REPOSITORY: &str = "kalisto-node";
const MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

#[derive(Clone, Copy)]
struct Blob {
    offset: u64,
    size: u64,
}

enum ImageSource {
    Archive(File),
    Layout(std::path::PathBuf),
}

impl ImageSource {
    fn open(path: &Path) -> Result<(Self, BTreeMap<String, Blob>)> {
        let mut blobs = BTreeMap::new();
        if path.is_dir() {
            let root = path.join("blobs/sha256");
            for entry in std::fs::read_dir(&root).map_err(io_error)? {
                let entry = entry.map_err(io_error)?;
                let hash = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| invalid("non-UTF-8 OCI blob name"))?;
                if !valid_hash(&hash) || !entry.file_type().map_err(io_error)?.is_file() {
                    return Err(invalid("invalid OCI layout blob"));
                }
                blobs.insert(
                    hash,
                    Blob {
                        offset: 0,
                        size: entry.metadata().map_err(io_error)?.len(),
                    },
                );
            }
            return Ok((Self::Layout(root), blobs));
        }
        let file = File::open(path).map_err(io_error)?;
        let length = file.metadata().map_err(io_error)?.len();
        let mut archive = tar::Archive::new(file);
        for entry in archive.entries_with_seek().map_err(io_error)? {
            let entry = entry.map_err(io_error)?;
            let path = entry.path().map_err(io_error)?;
            let Some(hash) = path
                .to_str()
                .and_then(|path| path.strip_prefix("blobs/sha256/"))
            else {
                continue;
            };
            if !valid_hash(hash) || !entry.header().entry_type().is_file() {
                return Err(invalid("invalid OCI archive blob"));
            }
            let blob = Blob {
                offset: entry.raw_file_position(),
                size: entry.size(),
            };
            if blob
                .offset
                .checked_add(blob.size)
                .is_none_or(|end| end > length)
                || blobs.insert(hash.to_owned(), blob).is_some()
            {
                return Err(invalid("truncated or duplicate OCI archive blob"));
            }
        }
        Ok((Self::Archive(archive.into_inner()), blobs))
    }

    fn copy_blob(&mut self, hash: &str, blob: Blob, stream: &mut TcpStream) -> io::Result<u64> {
        match self {
            Self::Archive(file) => {
                file.seek(SeekFrom::Start(blob.offset))?;
                io::copy(&mut file.take(blob.size), stream)
            }
            Self::Layout(root) => {
                let file = File::open(root.join(hash))?;
                io::copy(&mut file.take(blob.size), stream)
            }
        }
    }
}

pub(super) struct ArchiveRegistry {
    reference: String,
    stopping: Arc<AtomicBool>,
    worker: Option<JoinHandle<io::Result<()>>>,
}

impl ArchiveRegistry {
    pub(super) fn start(path: &Path, reference: &str) -> Result<Self> {
        let digest = reference
            .rsplit_once('@')
            .map(|(_, digest)| digest)
            .ok_or_else(|| invalid("node image reference must contain a digest"))?;
        let hash = digest
            .strip_prefix("sha256:")
            .filter(|hash| valid_hash(hash))
            .ok_or_else(|| invalid("invalid node image digest"))?;
        let (mut source, blobs) = ImageSource::open(path)?;
        if !blobs.contains_key(hash) {
            return Err(invalid(
                "node image manifest is absent from its OCI archive",
            ));
        }
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).map_err(io_error)?;
        let address = listener.local_addr().map_err(io_error)?;
        listener.set_nonblocking(true).map_err(io_error)?;
        let stopping = Arc::new(AtomicBool::new(false));
        let worker_stopping = Arc::clone(&stopping);
        let worker = thread::Builder::new()
            .name("kalisto-oci-import".into())
            .spawn(move || {
                while !worker_stopping.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            // Darwin inherits O_NONBLOCK from the listener.
                            // Response writes must wait for client backpressure.
                            stream.set_nonblocking(false)?;
                            stream.set_read_timeout(Some(Duration::from_secs(2)))?;
                            stream.set_write_timeout(Some(Duration::from_secs(30)))?;
                            if let Err(error) = serve(&mut stream, &mut source, &blobs) {
                                tracing::debug!(%error, "OCI import connection closed");
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => return Err(error),
                    }
                }
                Ok(())
            })
            .map_err(io_error)?;
        Ok(Self {
            reference: format!("{address}/{REPOSITORY}@{digest}"),
            stopping,
            worker: Some(worker),
        })
    }

    pub(super) fn reference(&self) -> &str {
        &self.reference
    }
}

impl Drop for ArchiveRegistry {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            match worker.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::warn!(%error, "OCI import listener failed"),
                Err(_) => tracing::warn!("OCI import listener panicked"),
            }
        }
    }
}

fn serve(
    stream: &mut TcpStream,
    source: &mut ImageSource,
    blobs: &BTreeMap<String, Blob>,
) -> io::Result<()> {
    let mut header = [0u8; 8192];
    let mut length: usize = 0;
    loop {
        let start = length.saturating_sub(3);
        let count = stream.read(&mut header[length..])?;
        if count == 0 {
            return Ok(());
        }
        length += count;
        if header[start..length]
            .windows(4)
            .any(|part| part == b"\r\n\r\n")
        {
            break;
        }
        if length == header.len() {
            return empty_response(stream, "431 Request Header Fields Too Large");
        }
    }
    let Ok(header) = std::str::from_utf8(&header[..length]) else {
        return empty_response(stream, "400 Bad Request");
    };
    let mut request = header.lines().next().unwrap_or("").split_whitespace();
    let (Some(method), Some(path), Some(version)) =
        (request.next(), request.next(), request.next())
    else {
        return empty_response(stream, "400 Bad Request");
    };
    if request.next().is_some() || !matches!(version, "HTTP/1.0" | "HTTP/1.1") {
        return empty_response(stream, "400 Bad Request");
    }
    if !matches!(method, "GET" | "HEAD") {
        return empty_response(stream, "405 Method Not Allowed");
    }
    if matches!(path, "/v2" | "/v2/") {
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Type: application/json\r\nDocker-Distribution-API-Version: registry/2.0\r\nConnection: close\r\n\r\n"
        )?;
        if method == "GET" {
            stream.write_all(b"{}")?;
        }
        return Ok(());
    }
    let (hash, media_type) =
        if let Some(hash) = path.strip_prefix("/v2/kalisto-node/manifests/sha256:") {
            (hash, MANIFEST_TYPE)
        } else if let Some(hash) = path.strip_prefix("/v2/kalisto-node/blobs/sha256:") {
            (hash, "application/octet-stream")
        } else {
            return empty_response(stream, "404 Not Found");
        };
    let Some(blob) = blobs.get(hash) else {
        return empty_response(stream, "404 Not Found");
    };
    let range = header.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("range").then(|| value.trim())
    });
    tracing::debug!(
        method,
        path,
        range,
        bytes = blob.size,
        "Serving prepared OCI content"
    );
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: {media_type}\r\nDocker-Distribution-API-Version: registry/2.0\r\nDocker-Content-Digest: sha256:{hash}\r\nConnection: close\r\n\r\n",
        blob.size
    )?;
    if method == "GET" {
        let transferred = source.copy_blob(hash, *blob, stream)?;
        tracing::debug!(
            path,
            transferred,
            expected = blob.size,
            "Sent prepared OCI content"
        );
        if transferred != blob.size {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
    }
    Ok(())
}

fn empty_response(stream: &mut TcpStream, status: &str) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
}

pub(super) fn import_image(
    runner: &dyn crate::process::CommandRunner,
    path: &Path,
    reference: &str,
) -> Result<()> {
    let registry = ArchiveRegistry::start(path, reference)?;
    super::runtime::run(
        runner,
        &[
            "image".into(),
            "pull".into(),
            "--scheme".into(),
            "http".into(),
            "--platform".into(),
            "linux/arm64".into(),
            registry.reference().into(),
        ],
    )?;
    super::runtime::run(
        runner,
        &[
            "image".into(),
            "tag".into(),
            registry.reference().into(),
            reference.into(),
        ],
    )?;
    super::runtime::run(
        runner,
        &["image".into(), "inspect".into(), reference.into()],
    )?;
    Ok(())
}

/// Keep network acquisition in the same managed downloader as the boot assets.
/// The borrowed Apple helper receives only local, digest-verified content.
pub(super) fn prepare_remote_image(
    runner: &dyn crate::process::CommandRunner,
    reference: &str,
) -> Result<String> {
    let output = runner.run(
        "crane",
        &[
            "digest".into(),
            "--platform".into(),
            "linux/arm64".into(),
            reference.into(),
        ],
    )?;
    let digest = output.stdout.trim();
    let hash = digest
        .strip_prefix("sha256:")
        .filter(|hash| valid_hash(hash))
        .ok_or_else(|| invalid("registry returned an invalid image digest"))?;
    let repository = reference.split('@').next().unwrap_or(reference);
    let repository = match repository.rfind(':') {
        Some(colon) if repository.rfind('/').is_some_and(|slash| colon > slash) => {
            &repository[..colon]
        }
        _ => repository,
    };
    let pinned = format!("{repository}@{digest}");
    let cache = super::runtime::root()?.join("oci-images");
    std::fs::create_dir_all(&cache).map_err(io_error)?;
    let destination = cache.join(hash);
    if !destination.exists() {
        let temporary = cache.join(format!(".{hash}.prepare-{}", std::process::id()));
        std::fs::create_dir(&temporary).map_err(io_error)?;
        let guard = ImageDownload(temporary);
        runner.run(
            "crane",
            &[
                "pull".into(),
                "--format=oci".into(),
                "--annotate-ref".into(),
                "--platform".into(),
                "linux/arm64".into(),
                pinned.clone(),
                guard.0.display().to_string(),
            ],
        )?;
        std::fs::rename(&guard.0, &destination).map_err(io_error)?;
    }
    import_image(runner, &destination, &pinned)?;
    Ok(pinned)
}

struct ImageDownload(std::path::PathBuf);

impl Drop for ImageDownload {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn invalid(message: &str) -> Error {
    Error::InvalidArgs(message.into())
}

fn io_error(source: io::Error) -> Error {
    Error::CommandIo {
        program: "Apple node image import".into(),
        source,
    }
}
