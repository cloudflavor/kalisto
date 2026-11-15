//! Native binary age envelopes. No plugins, subprocesses or secret CLI values.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use age::secrecy::{ExposeSecret, SecretString};
use zeroize::Zeroizing;

use super::format::{Scratch, invalid, io_error, private_file};
use crate::error::Result;

const MAX_SECRET_FILE: u64 = 64 * 1024;
const MAX_KEYS: usize = 64;
const AGE_MAGIC: &[u8] = b"age-encryption.org/";

/// Encryption material is deliberately neither Debug nor serializable.
pub enum ExportEncryption {
    Plaintext,
    Recipients(Vec<age::x25519::Recipient>),
    Passphrase(SecretString),
}
impl ExportEncryption {
    /// Read public recipients or a private passphrase file. Mutually exclusive.
    ///
    /// # Errors
    ///
    /// Rejects conflicting options, invalid or excessive recipients, and unreadable,
    /// insecure, or invalid passphrase files.
    pub fn from_files(recipients: &[String], passphrase_file: Option<&Path>) -> Result<Self> {
        if !recipients.is_empty() && passphrase_file.is_some() {
            return Err(invalid("choose age recipients OR a passphrase, not both"));
        }
        if let Some(path) = passphrase_file {
            return Self::with_passphrase(read_passphrase(path)?);
        }
        if recipients.is_empty() {
            return Ok(Self::Plaintext);
        }
        if recipients.len() > MAX_KEYS {
            return Err(invalid("too many age recipients"));
        }
        let keys = recipients
            .iter()
            .map(|value| {
                value.parse::<age::x25519::Recipient>().map_err(|_| {
                    invalid("invalid native age recipient; expected age1... public key")
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self::Recipients(keys))
    }

    /// # Errors
    ///
    /// Rejects passphrases shorter than 16 characters.
    pub fn with_passphrase(passphrase: SecretString) -> Result<Self> {
        if passphrase.expose_secret().chars().count() < 16 {
            return Err(invalid(
                "export passphrase must contain at least 16 characters; prefer randomly generated words",
            ));
        }
        Ok(Self::Passphrase(passphrase))
    }

    #[must_use]
    pub fn is_encrypted(&self) -> bool {
        !matches!(self, Self::Plaintext)
    }

    pub(super) fn encryptor(&self) -> Result<Option<age::Encryptor>> {
        match self {
            Self::Plaintext => Ok(None),
            Self::Recipients(keys) => {
                age::Encryptor::with_recipients(keys.iter().map(|key| key as &dyn age::Recipient))
                    .map(Some)
                    .map_err(|_| invalid("cannot initialize age recipient encryption"))
            }
            Self::Passphrase(passphrase) => {
                let mut recipient = age::scrypt::Recipient::new(passphrase.clone());
                // Fixed 256 MiB KDF, independent of export-machine calibration.
                recipient.set_work_factor(18);
                age::Encryptor::with_recipients(std::iter::once(&recipient as &dyn age::Recipient))
                    .map(Some)
                    .map_err(|_| invalid("cannot initialize age passphrase encryption"))
            }
        }
    }
}

/// Secret keys/passphrases never enter task JSON or command arguments.
pub enum ImportDecryption {
    None,
    Identities(Vec<age::x25519::Identity>),
    Passphrase(SecretString),
}
impl ImportDecryption {
    /// # Errors
    ///
    /// Rejects conflicting options, invalid or excessive identities, and unreadable,
    /// insecure, or invalid credential files.
    pub fn from_files(identity_files: &[PathBuf], passphrase_file: Option<&Path>) -> Result<Self> {
        if !identity_files.is_empty() && passphrase_file.is_some() {
            return Err(invalid(
                "choose identity files OR a passphrase file, not both",
            ));
        }
        if let Some(path) = passphrase_file {
            return Ok(Self::Passphrase(read_passphrase(path)?));
        }
        if identity_files.is_empty() {
            return Ok(Self::None);
        }
        if identity_files.len() > MAX_KEYS {
            return Err(invalid("too many identity files"));
        }
        let mut keys = Vec::new();
        for path in identity_files {
            let bytes = read_secret(path)?;
            let contents =
                std::str::from_utf8(&bytes).map_err(|_| invalid("identity file must be UTF-8"))?;
            for line in contents
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
            {
                if keys.len() >= MAX_KEYS {
                    return Err(invalid("too many age identities"));
                }
                keys.push(line.parse::<age::x25519::Identity>().map_err(|_| {
                    invalid(
                        "invalid identity file; only native AGE-SECRET-KEY-... keys are supported",
                    )
                })?);
            }
        }
        if keys.is_empty() {
            return Err(invalid("identity files contain no native age identities"));
        }
        Ok(Self::Identities(keys))
    }

    pub(super) fn reader(&self, input: &Path) -> Result<Box<dyn Read>> {
        if !std::fs::metadata(input).map_err(io_error)?.is_file() {
            return Err(invalid("archive input must be a regular file"));
        }
        let mut file = BufReader::new(File::open(input).map_err(io_error)?);
        let magic = file.fill_buf().map_err(io_error)?;
        if !magic.starts_with(AGE_MAGIC) {
            if !matches!(self, Self::None) || input.extension().is_some_and(|ext| ext == "age") {
                return Err(invalid(
                    "expected an age-encrypted archive; refusing plaintext downgrade",
                ));
            }
            return Ok(Box::new(file));
        }
        // age headers are public. Bound their parsing before invoking the library
        // so a malformed header cannot grow memory without the payload limit.
        let mut header = Vec::new();
        {
            let mut limited = (&mut file).take(MAX_SECRET_FILE + 1);
            loop {
                let start = header.len();
                if limited.read_until(b'\n', &mut header).map_err(io_error)? == 0
                    || header.len() as u64 > MAX_SECRET_FILE
                {
                    return Err(invalid("invalid or oversized age header"));
                }
                if header[start..].starts_with(b"--- ") {
                    break;
                }
            }
        }
        file.rewind().map_err(io_error)?;
        let decryptor = age::Decryptor::new_buffered(file)
            .map_err(|_| invalid("invalid or unsupported age archive header"))?;
        let reader = match self {
            Self::None => return Err(invalid("encrypted archive requires --identity or --passphrase/--passphrase-file")),
            Self::Identities(keys) => decryptor.decrypt(keys.iter().map(|key| key as &dyn age::Identity)),
            Self::Passphrase(passphrase) => {
                let mut identity = age::scrypt::Identity::new(passphrase.clone());
                // Bound untrusted archive KDF costs; age's default calibrated
                // work factor is accepted up to 2^20 (~1 GiB memory).
                identity.set_max_work_factor(20);
                decryptor.decrypt(std::iter::once(&identity as &dyn age::Identity))
            }
        }.map_err(|_| invalid("age decryption failed: wrong credentials, damaged header, or excessive passphrase work factor"))?;
        Ok(Box::new(reader))
    }
}

fn read_secret(path: &Path) -> Result<Zeroizing<Vec<u8>>> {
    if !std::fs::metadata(path).map_err(io_error)?.is_file() {
        return Err(invalid("secret input must be a regular file"));
    }
    let file = File::open(path).map_err(io_error)?;
    let metadata = file.metadata().map_err(io_error)?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(invalid(
            "secret input must be a private regular file (mode 0600 or 0400)",
        ));
    }
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(MAX_SECRET_FILE + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() as u64 > MAX_SECRET_FILE {
        return Err(invalid("secret input file too large"));
    }
    Ok(bytes)
}
fn read_passphrase(path: &Path) -> Result<SecretString> {
    let bytes = read_secret(path)?;
    let value = std::str::from_utf8(&bytes)
        .map_err(|_| invalid("passphrase file must be UTF-8"))?
        .trim_end_matches(['\r', '\n']);
    if value.is_empty() || value.contains(['\n', '\r']) {
        return Err(invalid("passphrase file must contain one nonempty line"));
    }
    Ok(SecretString::from(value.to_owned()))
}

/// Create a native age identity without external age-keygen; return public key only.
/// The secret destination must not exist, and is never included in normal output.
///
/// # Errors
///
/// Returns an error if private staging or key publication fails, including when
/// the destination already exists.
pub fn generate_identity(output: &Path) -> Result<String> {
    let identity = age::x25519::Identity::generate();
    let public = identity.to_public().to_string();
    let parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let scratch = Scratch::new(parent)?;
    let pending = scratch.0.join("identity.partial");
    let mut file = private_file(&pending)?;
    writeln!(
        file,
        "# public key: {public}\n{}",
        identity.to_string().expose_secret()
    )
    .map_err(io_error)?;
    file.sync_all().map_err(io_error)?;
    std::fs::hard_link(&pending, output).map_err(io_error)?;
    Ok(public)
}
