use std::fs;
use std::os::unix::fs::PermissionsExt;

use age::secrecy::SecretString;
use serde_json::json;

use super::DEFAULT_MAX_BYTES;
use super::crypto::{ExportEncryption, ImportDecryption, generate_identity};
use super::format::{self, Manifest, Scratch, private_file};

fn temporary() -> Scratch {
    Scratch::new(&std::env::temp_dir()).unwrap()
}
fn manifest() -> Manifest {
    Manifest {
        schema: 1,
        architecture: std::env::consts::ARCH.into(),
        lima_version: crate::runtime::LIMA_VERSION.into(),
        contains_secrets: true,
        state: json!({}),
        files: Vec::new(),
    }
}
fn recipient_protection(key: &age::x25519::Identity) -> ExportEncryption {
    ExportEncryption::Recipients(vec![key.to_public()])
}

#[test]
fn recipient_archive_is_standard_age_wrapping_gzip_and_preserves_payload() {
    let root = temporary();
    let key = age::x25519::Identity::generate();
    let source = root.0.join("source");
    fs::write(&source, b"private database and token").unwrap();
    let path = root.0.join("private.kalisto.age");
    format::write_protected(
        &path,
        manifest(),
        &[("nodes/0/disk".into(), source)],
        &recipient_protection(&key),
    )
    .unwrap();
    let ciphertext = fs::read(&path).unwrap();
    assert!(ciphertext.starts_with(b"age-encryption.org/v1\n"));
    // Interoperate with the upstream age API, without any Kalisto envelope.
    let compressed = age::decrypt(&key, &ciphertext).unwrap();
    assert!(compressed.starts_with(&[0x1f, 0x8b]));
    let destination = temporary();
    format::read_protected(
        &path,
        &destination.0,
        DEFAULT_MAX_BYTES,
        &ImportDecryption::Identities(vec![key]),
    )
    .unwrap();
    assert_eq!(
        fs::read(destination.0.join("nodes/0/disk")).unwrap(),
        b"private database and token"
    );
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(
        !fs::read_dir(&root.0)
            .unwrap()
            .any(|entry| entry.unwrap().file_type().unwrap().is_dir())
    );
}

#[test]
fn every_declared_recipient_can_decrypt_and_ciphertexts_are_randomized() {
    let root = temporary();
    let first = age::x25519::Identity::generate();
    let second = age::x25519::Identity::generate();
    let protection = ExportEncryption::Recipients(vec![first.to_public(), second.to_public()]);
    let a = root.0.join("a.age");
    let b = root.0.join("b.age");
    format::write_protected(&a, manifest(), &[], &protection).unwrap();
    format::write_protected(&b, manifest(), &[], &protection).unwrap();
    assert_ne!(fs::read(&a).unwrap(), fs::read(&b).unwrap());
    for key in [first, second] {
        format::read_protected(
            &a,
            &temporary().0,
            DEFAULT_MAX_BYTES,
            &ImportDecryption::Identities(vec![key]),
        )
        .unwrap();
    }
}

#[test]
fn wrong_key_missing_credentials_damage_truncation_and_appended_bytes_fail_closed() {
    let root = temporary();
    let key = age::x25519::Identity::generate();
    let path = root.0.join("bundle.age");
    format::write_protected(&path, manifest(), &[], &recipient_protection(&key)).unwrap();
    assert!(
        format::read_protected(
            &path,
            &temporary().0,
            DEFAULT_MAX_BYTES,
            &ImportDecryption::None
        )
        .is_err()
    );
    assert!(
        format::read_protected(
            &path,
            &temporary().0,
            DEFAULT_MAX_BYTES,
            &ImportDecryption::Identities(vec![age::x25519::Identity::generate()])
        )
        .is_err()
    );
    let original = fs::read(&path).unwrap();
    let credentials = ImportDecryption::Identities(vec![key]);
    for variant in 0..3 {
        let mut bytes = original.clone();
        match variant {
            0 => {
                let n = bytes.len() - 1;
                bytes[n] ^= 0x80;
            }
            1 => {
                bytes.pop();
            }
            _ => bytes.extend_from_slice(b"unexpected ciphertext"),
        }
        fs::write(&path, bytes).unwrap();
        assert!(
            format::read_protected(&path, &temporary().0, DEFAULT_MAX_BYTES, &credentials).is_err(),
            "variant {variant}"
        );
    }
}

#[test]
fn passphrase_archive_round_trips_and_wrong_passphrase_fails() {
    let root = temporary();
    let path = root.0.join("passphrase.age");
    let secret = SecretString::from("six random words should replace this example".to_owned());
    let protection = ExportEncryption::with_passphrase(secret.clone()).unwrap();
    format::write_protected(&path, manifest(), &[], &protection).unwrap();
    format::read_protected(
        &path,
        &temporary().0,
        DEFAULT_MAX_BYTES,
        &ImportDecryption::Passphrase(secret),
    )
    .unwrap();
    assert!(
        format::read_protected(
            &path,
            &temporary().0,
            DEFAULT_MAX_BYTES,
            &ImportDecryption::Passphrase(SecretString::from("different secret words".to_owned()))
        )
        .is_err()
    );
    assert!(ExportEncryption::with_passphrase(SecretString::from("short".to_owned())).is_err());
}

#[test]
fn decryption_credentials_or_age_suffix_never_silently_accept_plaintext() {
    let root = temporary();
    let path = root.0.join("plain.kalisto");
    format::write(&path, manifest(), &[]).unwrap();
    let key = age::x25519::Identity::generate();
    assert!(
        format::read_protected(
            &path,
            &temporary().0,
            DEFAULT_MAX_BYTES,
            &ImportDecryption::Identities(vec![key])
        )
        .is_err()
    );
    let disguised = root.0.join("disguised.age");
    fs::copy(&path, &disguised).unwrap();
    assert!(
        format::read_protected(
            &disguised,
            &temporary().0,
            DEFAULT_MAX_BYTES,
            &ImportDecryption::None
        )
        .is_err()
    );
}

#[test]
fn native_key_generation_is_private_no_clobber_and_never_returns_secret() {
    let root = temporary();
    let path = root.0.join("backup.agekey");
    let public = generate_identity(&path).unwrap();
    assert!(public.starts_with("age1"));
    assert!(!public.contains("SECRET"));
    let original = fs::read(&path).unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(generate_identity(&path).is_err());
    assert_eq!(fs::read(&path).unwrap(), original);
    assert!(
        matches!(ImportDecryption::from_files(&[path],None).unwrap(),ImportDecryption::Identities(keys) if keys.len()==1)
    );
}

#[test]
fn secret_files_are_size_bounded_private_and_not_echoed_on_parse_error() {
    let root = temporary();
    let path = root.0.join("bad.agekey");
    let secret = b"sensitive-invalid-identity";
    fs::write(&path, secret).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(ImportDecryption::from_files(std::slice::from_ref(&path), None).is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let error = ImportDecryption::from_files(std::slice::from_ref(&path), None)
        .err()
        .unwrap()
        .to_string();
    assert!(!error.contains(std::str::from_utf8(secret).unwrap()));
    fs::write(&path, vec![b'x'; 65537]).unwrap();
    assert!(ImportDecryption::from_files(&[path], None).is_err());
}

#[test]
fn private_passphrase_files_and_recipient_conflicts_are_handled_without_inline_secrets() {
    use std::io::Write;
    let root = temporary();
    let file = root.0.join("passphrase");
    let mut output = private_file(&file).unwrap();
    writeln!(output, "randomly-generated-words-for-backup").unwrap();
    assert!(
        ExportEncryption::from_files(&[], Some(&file))
            .unwrap()
            .is_encrypted()
    );
    assert!(matches!(
        ImportDecryption::from_files(&[], Some(&file)).unwrap(),
        ImportDecryption::Passphrase(_)
    ));
    let key = age::x25519::Identity::generate();
    assert!(ExportEncryption::from_files(&[key.to_public().to_string()], Some(&file)).is_err());
    assert!(ImportDecryption::from_files(std::slice::from_ref(&file), Some(&file)).is_err());
}

#[test]
fn encrypted_payload_limit_and_oversized_headers_are_rejected() {
    let root = temporary();
    let path = root.0.join("oversized.age");
    let source = root.0.join("source");
    fs::write(&source, b"oversized payload").unwrap();
    let key = age::x25519::Identity::generate();
    format::write_protected(
        &path,
        manifest(),
        &[("base-image".into(), source)],
        &recipient_protection(&key),
    )
    .unwrap();
    let credentials = ImportDecryption::Identities(vec![key]);
    assert!(format::read_protected(&path, &temporary().0, 1, &credentials).is_err());
    let mut header = b"age-encryption.org/v1\n".to_vec();
    header.extend(vec![b'x'; 65537]);
    fs::write(&path, header).unwrap();
    assert!(credentials.reader(&path).is_err());
}

#[test]
fn upstream_age_ciphertext_is_accepted_as_a_native_archive() {
    let root = temporary();
    let plain = root.0.join("plain");
    format::write(&plain, manifest(), &[]).unwrap();
    let key = age::x25519::Identity::generate();
    let encrypted = age::encrypt(&key.to_public(), &fs::read(plain).unwrap()).unwrap();
    let path = root.0.join("upstream.age");
    fs::write(&path, encrypted).unwrap();
    format::read_protected(
        &path,
        &temporary().0,
        DEFAULT_MAX_BYTES,
        &ImportDecryption::Identities(vec![key]),
    )
    .unwrap();
}
