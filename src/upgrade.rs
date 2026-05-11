//! Self-update for the `rdc` binary.
//!
//! Two surfaces:
//!
//! 1. `rdc upgrade` — explicit command. Resolves the target version
//!    (latest GitHub release or `--version`), downloads the matching
//!    platform tarball, runs a pre-flight `--version` against the new
//!    binary, swaps it in via atomic rename, and keeps the previous
//!    binary at `<install_dir>/rdc.bak` for one-shot rollback.
//!
//! 2. Once-daily passive nudge — every command reads
//!    `$XDG_CACHE_HOME/rdc/update.json` (or `~/.cache/rdc/update.json`).
//!    If the cached "latest version" is newer than the running binary,
//!    a one-line stderr note appears at command start. If the cache is
//!    older than 24h or missing, a background fetch refreshes it for
//!    the next invocation. All failures are silent — the nudge never
//!    blocks a command and never errors.
//!
//! ## Compatibility policy
//!
//! - **Backward compat (new binary reading old artifacts):** the
//!   latest rdc always reads anything produced by any previous release.
//!   Lockfile versions migrate forward; project config and overlay
//!   tolerate missing fields via serde defaults.
//! - **Forward compat (older binary reading artifacts from a newer
//!   release):** not promised. But artifacts the older binary doesn't
//!   understand must produce a clear error pointing at `rdc upgrade`,
//!   never silent corruption. The lockfile version check enforces this.
//! - **Downgrades** via `rdc upgrade --version <older>` are supported
//!   as an emergency escape hatch only; users may have to re-pull.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// User-Agent header GitHub's API requires.
const USER_AGENT: &str = concat!("rdc/", env!("CARGO_PKG_VERSION"));

/// GitHub repo that hosts releases.
const REPO: &str = "mrtnzlml/rossum-deployment-manager-experiment";

/// How long the version-check cache stays warm before a background
/// refresh fires.
const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Timeout for the GitHub API version check used by the once-daily
/// passive nudge. Kept tight so the nudge never makes a command feel
/// slow even on a flaky connection.
const NUDGE_TIMEOUT: Duration = Duration::from_secs(2);

/// Timeout for the explicit `rdc upgrade` flow. Allows for slow API
/// responses and (more importantly) a real tarball download.
const UPGRADE_TIMEOUT: Duration = Duration::from_secs(60);

/// Cached "what's the latest release" result. Lives at
/// `$XDG_CACHE_HOME/rdc/update.json` (or `~/.cache/rdc/update.json`).
/// Schema is versioned via `serde(default)` so newer rdc additions
/// don't break older binaries reading the same cache.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct UpdateCache {
    /// Unix epoch seconds at last successful check.
    checked_at: u64,
    /// Tag name from the GitHub release (without the leading `v`).
    latest: String,
}

/// Parsed semver-lite. Only major.minor.patch is honored; pre-release
/// and build metadata are dropped during parse. Sufficient for our
/// version pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim().trim_start_matches('v');
        // Drop pre-release/build metadata.
        let core = s.split(['-', '+']).next().unwrap_or(s);
        let mut parts = core.split('.');
        let major = parts
            .next()
            .ok_or_else(|| anyhow!("version '{s}' missing major"))?
            .parse::<u32>()
            .with_context(|| format!("parsing major in '{s}'"))?;
        let minor = parts
            .next()
            .ok_or_else(|| anyhow!("version '{s}' missing minor"))?
            .parse::<u32>()
            .with_context(|| format!("parsing minor in '{s}'"))?;
        let patch = parts
            .next()
            .ok_or_else(|| anyhow!("version '{s}' missing patch"))?
            .parse::<u32>()
            .with_context(|| format!("parsing patch in '{s}'"))?;
        Ok(Self { major, minor, patch })
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Version of the running binary.
pub fn current() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION"))
        .expect("CARGO_PKG_VERSION should always be parseable")
}

/// Where the running binary lives, and what we can safely do with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallLocation {
    /// Path is under `~/.cargo/bin/` (or `$CARGO_HOME/bin`). Replacing
    /// it would break cargo's bookkeeping — instruct the user to run
    /// `cargo install` instead.
    Cargo(PathBuf),
    /// Path is writable and not managed by cargo. Safe to self-replace.
    Replaceable(PathBuf),
    /// Path is read-only or otherwise non-replaceable. Print manual
    /// instructions and exit.
    Unknown(PathBuf),
}

/// Classify the running binary's location.
pub fn classify_install() -> Result<InstallLocation> {
    let exe = std::env::current_exe().context("locating the running rdc binary")?;
    let canonical =
        std::fs::canonicalize(&exe).with_context(|| format!("canonicalizing {}", exe.display()))?;

    if is_under_cargo_bin(&canonical) {
        return Ok(InstallLocation::Cargo(canonical));
    }

    if is_writable_in_place(&canonical) {
        Ok(InstallLocation::Replaceable(canonical))
    } else {
        Ok(InstallLocation::Unknown(canonical))
    }
}

fn is_under_cargo_bin(path: &Path) -> bool {
    // CARGO_HOME first, then fall back to ~/.cargo.
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| {
                let mut p = PathBuf::from(h);
                p.push(".cargo");
                p
            })
        });
    let Some(cargo_home) = cargo_home else { return false };
    let cargo_bin = cargo_home.join("bin");
    path.starts_with(&cargo_bin)
}

fn is_writable_in_place(path: &Path) -> bool {
    // The check that actually matters is "can we replace this file?" —
    // which on Unix means "can we write into its parent directory?"
    // because rename(parent/tmp, parent/exe) requires the parent
    // writable, not the file itself.
    let Some(parent) = path.parent() else { return false };
    let meta = match std::fs::metadata(parent) {
        Ok(m) => m,
        Err(_) => return false,
    };
    !meta.permissions().readonly()
}

/// Asset name for the current platform, matching what the release
/// workflow uploads. Returns an error on platforms we don't build for.
pub fn platform_asset_name() -> Result<String> {
    let os = match std::env::consts::OS {
        "macos" => "apple-darwin",
        "linux" => "unknown-linux-gnu",
        other => anyhow::bail!("unsupported OS '{other}' — build from source instead"),
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => anyhow::bail!("unsupported arch '{other}' — build from source instead"),
    };
    if os == "unknown-linux-gnu" && arch == "aarch64" {
        anyhow::bail!("linux aarch64 isn't pre-built; build from source instead");
    }
    Ok(format!("rdc-{arch}-{os}.tar.gz"))
}

/// Minimal subset of the GitHub Releases JSON.
#[derive(Debug, Deserialize)]
struct ReleaseInfo {
    tag_name: String,
}

fn http_client(timeout: Duration) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(timeout)
        .build()
        .context("building HTTP client")
}

async fn fetch_latest_version_with_timeout(timeout: Duration) -> Result<Version> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let info: ReleaseInfo = http_client(timeout)?
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("non-2xx from {url}"))?
        .json()
        .await
        .context("parsing GitHub release JSON")?;
    Version::parse(&info.tag_name)
}

/// Fetch the latest release tag from GitHub. Used by the explicit
/// `rdc upgrade` flow; tolerates slower responses than the nudge.
pub async fn fetch_latest_version() -> Result<Version> {
    fetch_latest_version_with_timeout(UPGRADE_TIMEOUT).await
}

/// Fetch a specific version's tarball asset URL.
fn asset_download_url(version: &Version, asset: &str) -> String {
    format!("https://github.com/{REPO}/releases/download/v{version}/{asset}")
}

/// Cache file location: `$XDG_CACHE_HOME/rdc/update.json` if set,
/// `$HOME/.cache/rdc/update.json` otherwise. Returns None if we can't
/// figure out a home directory.
fn cache_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| {
                let mut p = PathBuf::from(h);
                p.push(".cache");
                p
            })
        })?;
    Some(base.join("rdc").join("update.json"))
}

fn now_unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn load_cache() -> Option<UpdateCache> {
    let path = cache_path()?;
    let raw = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn save_cache(cache: &UpdateCache) {
    let Some(path) = cache_path() else { return };
    if let Some(dir) = path.parent() {
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
    }
    let Ok(raw) = serde_json::to_string(cache) else { return };
    let _ = std::fs::write(path, raw);
}

/// If `latest` from cache is greater than `current`, return it for
/// callers that want to nudge the user. Otherwise None. An empty
/// `latest` field (e.g. after a failed first fetch) returns None.
pub fn cached_upgrade_available() -> Option<Version> {
    let cache = load_cache()?;
    if cache.latest.is_empty() {
        return None;
    }
    let latest = Version::parse(&cache.latest).ok()?;
    if latest > current() {
        Some(latest)
    } else {
        None
    }
}

/// Refresh the cache if it's older than CACHE_TTL or missing. Uses
/// the tight NUDGE_TIMEOUT so a slow or unreachable API can't make
/// a command feel slow. All errors are swallowed — this is purely
/// best-effort.
///
/// On a failed fetch we still write `checked_at = now` so a network
/// outage doesn't make every subsequent command re-attempt the call.
/// The previous `latest` value (if any) is preserved for the nudge.
pub async fn refresh_cache_if_stale() {
    let existing = load_cache();
    let stale = match &existing {
        Some(c) => now_unix_seconds().saturating_sub(c.checked_at) >= CACHE_TTL.as_secs(),
        None => true,
    };
    if !stale {
        return;
    }
    let fetched = fetch_latest_version_with_timeout(NUDGE_TIMEOUT).await.ok();
    let latest = fetched
        .map(|v| v.to_string())
        .or_else(|| existing.as_ref().map(|c| c.latest.clone()))
        .unwrap_or_default();
    save_cache(&UpdateCache {
        checked_at: now_unix_seconds(),
        latest,
    });
}

/// Emit a one-line nudge to stderr if the cache says we're behind.
/// Called once per command, just before the dispatch.
pub fn emit_nudge_if_available() {
    if let Some(latest) = cached_upgrade_available() {
        eprintln!("note: rdc v{latest} is available — run `rdc upgrade` to install");
    }
}

/// Top-level `rdc upgrade` flow. `target` is either an explicit version
/// or the latest fetched fresh.
pub async fn run_upgrade(target: Option<Version>, check_only: bool) -> Result<()> {
    let current_v = current();

    let latest = match target {
        Some(v) => v,
        None => fetch_latest_version().await?,
    };

    if check_only {
        if latest > current_v {
            println!("rdc v{current_v} → v{latest} available");
        } else if latest == current_v {
            println!("rdc v{current_v} is the latest");
        } else {
            println!("rdc v{current_v} is ahead of the latest release v{latest}");
        }
        return Ok(());
    }

    if latest == current_v {
        println!("rdc v{current_v} is already the latest");
        // Refresh the cache so the nudge stops appearing next time.
        save_cache(&UpdateCache {
            checked_at: now_unix_seconds(),
            latest: latest.to_string(),
        });
        return Ok(());
    }

    let install = classify_install()?;
    let target_path = match &install {
        InstallLocation::Cargo(path) => {
            eprintln!(
                "rdc was installed via cargo (binary at {}).\n\
                 To upgrade, run:\n\n  \
                 cargo install --git https://github.com/{REPO} --force\n",
                path.display()
            );
            anyhow::bail!("cannot self-replace a cargo-installed binary");
        }
        InstallLocation::Unknown(path) => {
            let asset = platform_asset_name().unwrap_or_else(|_| "<your-platform>.tar.gz".into());
            eprintln!(
                "rdc binary at {} is not in a writable directory.\n\
                 To upgrade manually:\n\n  \
                 curl -fsSL {} -o /tmp/{asset}\n  \
                 tar xzf /tmp/{asset} -C /tmp\n  \
                 mv /tmp/rdc {}\n",
                path.display(),
                asset_download_url(&latest, &asset),
                path.display(),
            );
            anyhow::bail!("install location not writable");
        }
        InstallLocation::Replaceable(path) => path.clone(),
    };

    let asset_name = platform_asset_name()?;
    let url = asset_download_url(&latest, &asset_name);

    println!("downloading rdc v{latest} ({asset_name})…");
    let bytes = http_client(UPGRADE_TIMEOUT)?
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("non-2xx from {url}"))?
        .bytes()
        .await
        .context("reading tarball body")?;

    let tmp = tempfile::tempdir().context("creating temp dir for upgrade")?;
    extract_rdc_binary(bytes.as_ref(), tmp.path())
        .context("extracting rdc from tarball")?;

    let staged = tmp.path().join("rdc");
    if !staged.exists() {
        anyhow::bail!("tarball did not contain an 'rdc' binary at the root");
    }

    // Pre-flight: run the staged binary's --version and confirm it
    // reports the version we expected. Catches a corrupt download or
    // the wrong platform tarball before we touch the user's binary.
    let actual = std::process::Command::new(&staged)
        .arg("--version")
        .output()
        .context("running pre-flight `--version` on the staged binary")?;
    if !actual.status.success() {
        anyhow::bail!(
            "staged binary failed --version (status {:?}, stderr: {})",
            actual.status,
            String::from_utf8_lossy(&actual.stderr).trim()
        );
    }
    let stdout = String::from_utf8_lossy(&actual.stdout);
    let parsed_actual = stdout
        .split_whitespace()
        .find_map(|tok| Version::parse(tok).ok())
        .ok_or_else(|| {
            anyhow!("could not parse a version from staged binary stdout: {stdout:?}")
        })?;
    if parsed_actual != latest {
        anyhow::bail!(
            "staged binary reports v{parsed_actual} but we expected v{latest}"
        );
    }

    // Self-replace pattern (Unix-only; Windows isn't a supported target):
    //
    // The atomic rename(staged, target) below is correct only when both
    // paths are on the same filesystem. The staged binary lives under
    // tempfile::tempdir() which is often /tmp — a different fs from the
    // install dir. So we first COPY the staged binary into a sibling of
    // target (same dir → same fs), then atomically rename it over
    // target. The kernel keeps the old binary's inode alive for the
    // running process, so this self-update completes without breaking
    // the in-flight upgrade.
    //
    // We also use COPY (not rename) for the backup so target_path is
    // never absent from the directory — a parallel shell that runs
    // `rdc` during the upgrade always sees a valid binary, either the
    // old one (before swap) or the new one (after).
    let target_dir = target_path.parent().ok_or_else(|| {
        anyhow!("target path {} has no parent directory", target_path.display())
    })?;

    // 1. Stage the new binary in the target's directory.
    let staging_path = target_dir.join(format!(".rdc.new.{}", std::process::id()));
    std::fs::copy(&staged, &staging_path).with_context(|| {
        format!(
            "staging new binary at {} (from {})",
            staging_path.display(),
            staged.display()
        )
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&staging_path)
            .with_context(|| format!("stat {}", staging_path.display()))?
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&staging_path, perms)
            .with_context(|| format!("chmod {}", staging_path.display()))?;
    }

    // 2. Copy the current binary aside as a backup BEFORE the swap.
    //    Target stays in place — this is just a sibling duplicate.
    let backup_path = target_path.with_file_name("rdc.bak");
    let _ = std::fs::remove_file(&backup_path); // overwrite previous backup
    if let Err(e) = std::fs::copy(&target_path, &backup_path) {
        let _ = std::fs::remove_file(&staging_path);
        return Err(anyhow::Error::new(e)).with_context(|| {
            format!(
                "backing up {} to {}",
                target_path.display(),
                backup_path.display()
            )
        });
    }

    // 3. Atomic swap: rename(staging, target) replaces target's
    //    directory entry. Old inode stays alive for the running rdc;
    //    every subsequent `rdc` invocation gets the new binary.
    if let Err(e) = std::fs::rename(&staging_path, &target_path) {
        let _ = std::fs::remove_file(&staging_path);
        // The backup is still good; target_path is still the old binary.
        return Err(anyhow::Error::new(e)).with_context(|| {
            format!(
                "swapping in new binary at {} (target unchanged; backup at {})",
                target_path.display(),
                backup_path.display()
            )
        });
    }

    // Refresh the nudge cache so we don't keep telling the user about
    // the upgrade they just did.
    save_cache(&UpdateCache {
        checked_at: now_unix_seconds(),
        latest: latest.to_string(),
    });

    println!(
        "upgraded rdc v{current_v} → v{latest}\nprevious binary at {} (delete when ready)",
        backup_path.display()
    );
    Ok(())
}

/// Walk a `.tar.gz` byte slice and extract `rdc` to `dest_dir`.
/// The tarball produced by `.github/workflows/release.yaml` has the
/// binary at the root; we accept any entry named `rdc` regardless of
/// path prefix for robustness.
fn extract_rdc_binary(gz_bytes: &[u8], dest_dir: &Path) -> Result<()> {
    let gz = flate2::read::GzDecoder::new(gz_bytes);
    let mut archive = tar::Archive::new(gz);
    for entry in archive.entries().context("reading tarball entries")? {
        let mut entry = entry.context("reading tarball entry")?;
        let entry_path = entry.path().context("reading tarball entry path")?.into_owned();
        let file_name = match entry_path.file_name() {
            Some(n) => n.to_owned(),
            None => continue,
        };
        if file_name == "rdc" {
            let dest = dest_dir.join("rdc");
            entry
                .unpack(&dest)
                .with_context(|| format!("unpacking rdc to {}", dest.display()))?;
            return Ok(());
        }
    }
    anyhow::bail!("no 'rdc' binary inside tarball")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parse_basic() {
        let v = Version::parse("0.1.2").unwrap();
        assert_eq!(v.major, 0);
        assert_eq!(v.minor, 1);
        assert_eq!(v.patch, 2);
    }

    #[test]
    fn version_parse_with_v_prefix() {
        let v = Version::parse("v1.2.3").unwrap();
        assert_eq!(v, Version { major: 1, minor: 2, patch: 3 });
    }

    #[test]
    fn version_parse_strips_pre_release() {
        let v = Version::parse("v1.2.3-rc.1").unwrap();
        assert_eq!(v, Version { major: 1, minor: 2, patch: 3 });
    }

    #[test]
    fn version_parse_strips_build_metadata() {
        let v = Version::parse("1.2.3+commit.abc").unwrap();
        assert_eq!(v, Version { major: 1, minor: 2, patch: 3 });
    }

    #[test]
    fn version_ord() {
        assert!(Version::parse("0.0.1").unwrap() < Version::parse("0.0.2").unwrap());
        assert!(Version::parse("0.1.0").unwrap() < Version::parse("0.10.0").unwrap());
        assert!(Version::parse("1.0.0").unwrap() > Version::parse("0.99.99").unwrap());
    }

    #[test]
    fn version_parse_rejects_garbage() {
        assert!(Version::parse("not a version").is_err());
        assert!(Version::parse("1.2").is_err());
        assert!(Version::parse("").is_err());
    }

    #[test]
    fn platform_asset_name_is_one_of_known() {
        // We can only assert on the current platform; just check format.
        if let Ok(name) = platform_asset_name() {
            assert!(name.starts_with("rdc-"));
            assert!(name.ends_with(".tar.gz"));
        }
    }

    #[test]
    fn cargo_bin_detection_matches_expected_prefix() {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap();
        let p = home.join(".cargo").join("bin").join("rdc");
        assert!(is_under_cargo_bin(&p));
        let p2 = home.join(".local").join("bin").join("rdc");
        assert!(!is_under_cargo_bin(&p2));
    }
}
