use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tar::Archive;

const OWNER: &str = "debba";
const REPOSITORY: &str = "tuxcleaner";
const MAX_ARCHIVE_BYTES: u64 = 200 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    assets: Vec<GitHubAsset>,
}

#[derive(Debug, Clone, Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateInfo {
    pub current_version: String,
    pub available_version: String,
    pub target: String,
    pub update_available: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateResult {
    pub previous_version: String,
    pub installed_version: String,
    pub target: String,
    pub updated: bool,
}

pub fn check(requested_version: Option<&str>) -> Result<UpdateInfo> {
    let release = fetch_release(requested_version)?;
    info_from_release(&release)
}

pub fn install(requested_version: Option<&str>) -> Result<UpdateResult> {
    let release = fetch_release(requested_version)?;
    let info = info_from_release(&release)?;
    if requested_version.is_none() && !info.update_available {
        return Ok(UpdateResult {
            previous_version: info.current_version.clone(),
            installed_version: info.current_version,
            target: info.target,
            updated: false,
        });
    }

    let current_executable = ensure_update_target_writable()?;

    let archive_name = format!("tuxcleaner-{}.tar.gz", info.target);
    let checksum_name = format!("{archive_name}.sha256");
    let archive_url = asset_url(&release, &archive_name)?;
    let checksum_url = asset_url(&release, &checksum_name)?;
    let archive = download(archive_url, MAX_ARCHIVE_BYTES)?;
    let checksum = download(checksum_url, 4096)?;
    verify_checksum(&archive, &checksum)?;

    let temporary = tempfile::tempdir().context("failed to create update directory")?;
    extract_archive(&archive, temporary.path())?;
    let new_binary = temporary.path().join("tuxcleaner");
    if !new_binary.is_file() {
        bail!("release archive does not contain the tuxcleaner binary");
    }
    self_replace::self_replace(&new_binary).with_context(|| {
        format!(
            "failed to replace {}; the install directory may have become read-only or lost write permission",
            current_executable.display()
        )
    })?;

    Ok(UpdateResult {
        previous_version: info.current_version,
        installed_version: info.available_version,
        target: info.target,
        updated: true,
    })
}

fn ensure_update_target_writable() -> Result<PathBuf> {
    let executable = std::env::current_exe()
        .context("failed to determine the current executable before updating")?;
    verify_update_target_writable(&executable)?;
    Ok(executable)
}

fn verify_update_target_writable(executable: &Path) -> Result<()> {
    let directory = executable.parent().with_context(|| {
        format!(
            "cannot determine the install directory for {}",
            executable.display()
        )
    })?;
    tempfile::Builder::new()
        .prefix(".tuxcleaner-update-check-")
        .tempfile_in(directory)
        .with_context(|| {
            format!(
                "cannot update {} because its install directory {} is not writable; run TuxCleaner outside a read-only sandbox or reinstall it into a writable directory",
                executable.display(),
                directory.display()
            )
        })?;
    Ok(())
}

fn fetch_release(requested_version: Option<&str>) -> Result<GitHubRelease> {
    let api_base = std::env::var("TUXCLEANER_GITHUB_API")
        .unwrap_or_else(|_| format!("https://api.github.com/repos/{OWNER}/{REPOSITORY}"));
    let endpoint = match requested_version {
        Some(version) => format!("{api_base}/releases/tags/{}", normalize_tag(version)?),
        None => format!("{api_base}/releases/latest"),
    };
    let mut response = ureq::get(&endpoint)
        .header("Accept", "application/vnd.github+json")
        .header(
            "User-Agent",
            concat!("tuxcleaner/", env!("CARGO_PKG_VERSION")),
        )
        .call()
        .with_context(|| format!("failed to query GitHub release metadata at {endpoint}"))?;
    response
        .body_mut()
        .with_config()
        .limit(2 * 1024 * 1024)
        .read_json()
        .context("failed to parse GitHub release metadata")
}

fn info_from_release(release: &GitHubRelease) -> Result<UpdateInfo> {
    let current = Version::parse(env!("CARGO_PKG_VERSION"))?;
    let available = Version::parse(release.tag_name.trim_start_matches('v'))
        .with_context(|| format!("invalid release tag: {}", release.tag_name))?;
    Ok(UpdateInfo {
        current_version: current.to_string(),
        available_version: available.to_string(),
        target: release_target()?,
        update_available: available > current,
    })
}

fn release_target() -> Result<String> {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => bail!("self-update is not available for CPU architecture {other}"),
    };
    let libc = if cfg!(target_env = "musl") {
        "musl"
    } else {
        "gnu"
    };
    Ok(format!("{arch}-unknown-linux-{libc}"))
}

fn normalize_tag(version: &str) -> Result<String> {
    let value = version.trim().trim_start_matches('v');
    let parsed = Version::parse(value).with_context(|| format!("invalid version: {version}"))?;
    Ok(format!("v{parsed}"))
}

fn asset_url<'a>(release: &'a GitHubRelease, name: &str) -> Result<&'a str> {
    release
        .assets
        .iter()
        .find(|asset| asset.name == name)
        .map(|asset| asset.browser_download_url.as_str())
        .with_context(|| format!("release {} is missing asset {name}", release.tag_name))
}

fn download(url: &str, limit: u64) -> Result<Vec<u8>> {
    let mut response = ureq::get(url)
        .header(
            "User-Agent",
            concat!("tuxcleaner/", env!("CARGO_PKG_VERSION")),
        )
        .call()
        .with_context(|| format!("failed to download {url}"))?;
    response
        .body_mut()
        .with_config()
        .limit(limit)
        .read_to_vec()
        .with_context(|| format!("failed to read {url}"))
}

fn verify_checksum(archive: &[u8], checksum_file: &[u8]) -> Result<()> {
    let expected = std::str::from_utf8(checksum_file)?
        .split_whitespace()
        .next()
        .context("checksum asset is empty")?;
    if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("checksum asset does not contain a valid SHA-256 digest");
    }
    let actual = format!("{:x}", Sha256::digest(archive));
    if !expected.eq_ignore_ascii_case(&actual) {
        bail!("SHA-256 checksum verification failed");
    }
    Ok(())
}

fn extract_archive(bytes: &[u8], destination: &Path) -> Result<()> {
    let decoder = GzDecoder::new(Cursor::new(bytes));
    let mut archive = Archive::new(decoder);
    for entry in archive
        .entries()
        .context("failed to read release archive")?
    {
        let mut entry = entry.context("failed to read release archive entry")?;
        let path = entry.path().context("invalid release archive path")?;
        if path.as_ref() == Path::new("tuxcleaner") {
            let output = destination.join("tuxcleaner");
            let mut file = fs::File::create(&output)?;
            std::io::copy(&mut entry, &mut file)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&output, fs::Permissions::from_mode(0o755))?;
            }
            return Ok(());
        }
    }
    bail!("release archive does not contain tuxcleaner")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_release_versions() {
        assert_eq!(normalize_tag("0.3.0").unwrap(), "v0.3.0");
        assert_eq!(normalize_tag("v1.0.0-beta.1").unwrap(), "v1.0.0-beta.1");
        assert!(normalize_tag("latest").is_err());
    }

    #[test]
    fn verifies_sha256_assets() {
        let archive = b"release bytes";
        let checksum = format!("{:x}  archive.tar.gz\n", Sha256::digest(archive));
        verify_checksum(archive, checksum.as_bytes()).unwrap();
        assert!(verify_checksum(b"changed", checksum.as_bytes()).is_err());
    }

    #[test]
    fn finds_assets_by_exact_name() {
        let release = GitHubRelease {
            tag_name: "v1.0.0".into(),
            assets: vec![GitHubAsset {
                name: "wanted.tar.gz".into(),
                browser_download_url: "https://example.invalid/wanted".into(),
            }],
        };
        assert_eq!(
            asset_url(&release, "wanted.tar.gz").unwrap(),
            "https://example.invalid/wanted"
        );
        assert!(asset_url(&release, "other.tar.gz").is_err());
    }

    #[test]
    fn writable_update_target_preflight_accepts_the_test_binary() {
        let executable = ensure_update_target_writable().unwrap();
        assert!(executable.is_file());
    }

    #[cfg(unix)]
    #[test]
    fn update_target_preflight_explains_a_non_writable_directory() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("bin");
        let executable = directory.join("tuxcleaner");
        fs::create_dir(&directory).unwrap();
        fs::write(&executable, b"binary").unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o555)).unwrap();

        let error = verify_update_target_writable(&executable).unwrap_err();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();

        let message = format!("{error:#}");
        assert!(message.contains("is not writable"));
        assert!(message.contains("read-only sandbox"));
    }
}
