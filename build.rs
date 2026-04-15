use std::env;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
    println!("cargo:rerun-if-changed=.git/refs/tags");
    println!("cargo:rerun-if-changed=.git/packed-refs");
    println!("cargo:rerun-if-env-changed=KALISTO_BUILD_VERSION");

    let package_version = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_owned());
    let build_version = env::var("KALISTO_BUILD_VERSION")
        .unwrap_or_else(|_| derive_build_version(&package_version));

    println!("cargo:rustc-env=KALISTO_BUILD_VERSION={build_version}");
}

fn derive_build_version(package_version: &str) -> String {
    let short_sha =
        git_output(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "nogit".to_owned());
    let dirty_suffix = if is_dirty_tree() { ".dirty" } else { "" };

    if let Some((release_tag, release_version)) = nearest_release_tag() {
        let commits_since_tag = git_commit_distance(&release_tag).unwrap_or(0);
        if commits_since_tag == 0 && dirty_suffix.is_empty() {
            return release_version;
        }

        let next_patch = increment_patch_version(&release_version);
        return format!("{next_patch}-dev.{commits_since_tag}+g{short_sha}{dirty_suffix}");
    }

    let commit_count =
        git_output(&["rev-list", "--count", "HEAD"]).unwrap_or_else(|| "0".to_owned());
    format!("{package_version}-dev.{commit_count}+g{short_sha}{dirty_suffix}")
}

fn nearest_release_tag() -> Option<(String, String)> {
    let tag = git_output(&[
        "describe",
        "--tags",
        "--abbrev=0",
        "--match",
        "v[0-9]*.[0-9]*.[0-9]*",
        "HEAD",
    ])?;
    let version = parse_release_version(&tag)?;
    Some((tag, version))
}

fn parse_release_version(tag: &str) -> Option<String> {
    let version = tag.strip_prefix('v')?;
    let mut parts = version.split('.');
    let major = parts.next()?;
    let minor = parts.next()?;
    let patch = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    if !all_ascii_digits(major) || !all_ascii_digits(minor) || !all_ascii_digits(patch) {
        return None;
    }

    Some(format!("{major}.{minor}.{patch}"))
}

fn increment_patch_version(version: &str) -> String {
    let mut parts = version.split('.');
    let major = parts.next().unwrap_or("0");
    let minor = parts.next().unwrap_or("0");
    let patch = parts.next().unwrap_or("0").parse::<u64>().unwrap_or(0);
    format!("{major}.{minor}.{}", patch + 1)
}

fn all_ascii_digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn git_commit_distance(tag: &str) -> Option<u64> {
    let range = format!("{tag}..HEAD");
    let count = git_output(&["rev-list", "--count", &range])?;
    count.parse::<u64>().ok()
}

fn is_dirty_tree() -> bool {
    let output = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output();
    let Ok(output) = output else {
        return false;
    };
    if !output.status.success() {
        return false;
    }

    match String::from_utf8(output.stdout) {
        Ok(value) => !value.trim().is_empty(),
        Err(_) => false,
    }
}

fn git_output(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }

    let value = String::from_utf8(output.stdout).ok()?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    Some(trimmed.to_owned())
}
