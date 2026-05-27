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
const REPO: &str = "mrtnzlml/rdc";

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
    // CARGO_HOME first, then fall back to <home>/.cargo. `home_dir`
    // honours HOME on Unix and USERPROFILE on Windows.
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".cargo")));
    let Some(cargo_home) = cargo_home else { return false };
    path.starts_with(cargo_home.join("bin"))
}

/// User home directory: `$HOME` on Unix, `%USERPROFILE%` on Windows
/// (falls back to `$HOME` first on both — some Windows shells set it).
fn home_dir() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("HOME") {
        return Some(PathBuf::from(h));
    }
    #[cfg(windows)]
    {
        if let Some(h) = std::env::var_os("USERPROFILE") {
            return Some(PathBuf::from(h));
        }
    }
    None
}

fn is_writable_in_place(path: &Path) -> bool {
    // The check that actually matters is "can we replace this file?" —
    // which on Unix means "can we write into its parent directory?"
    // because rename(parent/tmp, parent/exe) requires the parent
    // writable, not the file itself. We probe by creating and deleting
    // a tiny test file. This is more reliable than reading permission
    // bits, especially on Windows where `C:\Program Files` is not
    // marked read-only but requires admin to write to.
    let Some(parent) = path.parent() else { return false };
    let probe = parent.join(format!(".rdc.write_test.{}", std::process::id()));
    let ok = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

/// Asset name for the current platform, matching what the release
/// workflow uploads. Returns an error on platforms we don't build for.
pub fn platform_asset_name() -> Result<String> {
    let os = match std::env::consts::OS {
        "macos" => "apple-darwin",
        "linux" => "unknown-linux-gnu",
        "windows" => "pc-windows-msvc",
        other => anyhow::bail!("unsupported OS '{other}'; build from source instead"),
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => anyhow::bail!("unsupported arch '{other}'; build from source instead"),
    };
    if os == "unknown-linux-gnu" && arch == "aarch64" {
        anyhow::bail!("linux aarch64 isn't pre-built; build from source instead");
    }
    if os == "pc-windows-msvc" && arch == "aarch64" {
        anyhow::bail!("windows aarch64 isn't pre-built; build from source instead");
    }
    Ok(format!("rdc-{arch}-{os}.tar.gz"))
}

/// Filename of the rdc binary on the current platform.
fn binary_filename() -> &'static str {
    if cfg!(windows) { "rdc.exe" } else { "rdc" }
}

/// Filename of the rollback copy left next to the installed binary.
fn backup_filename() -> &'static str {
    if cfg!(windows) { "rdc.bak.exe" } else { "rdc.bak" }
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

/// Cache file location. On Unix: `$XDG_CACHE_HOME/rdc/update.json`,
/// falling back to `$HOME/.cache/rdc/update.json`. On Windows:
/// `%LOCALAPPDATA%\rdc\update.json`, falling back to
/// `%USERPROFILE%\AppData\Local\rdc\update.json`. Returns None if we
/// can't locate a usable base directory.
fn cache_path() -> Option<PathBuf> {
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join("AppData").join("Local")))?;
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".cache")))?;
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
    if let Some(dir) = path.parent()
        && std::fs::create_dir_all(dir).is_err() {
            return;
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
        let log = crate::log::Log::new(crate::cli::resolve::detect_color_mode(false));
        log.event(crate::log::Action::Info, &format!("rdc v{latest} is available; run `rdc upgrade` to install"));
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
            println!("rdc v{current_v} -> v{latest} available");
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
    let log = crate::log::Log::new(crate::cli::resolve::detect_color_mode(false));
    let target_path = match &install {
        InstallLocation::Cargo(path) => {
            log.event(
                crate::log::Action::Warn,
                &format!(
                    "rdc was installed via cargo (binary at {}).\n\
                     To upgrade, run:\n\n  \
                     cargo install --git https://github.com/{REPO} --force\n",
                    path.display()
                ),
            );
            anyhow::bail!("cannot self-replace a cargo-installed binary");
        }
        InstallLocation::Unknown(path) => {
            let asset = platform_asset_name().unwrap_or_else(|_| "<your-platform>.tar.gz".into());
            let url = asset_download_url(&latest, &asset);
            let bin = binary_filename();
            #[cfg(windows)]
            log.event(
                crate::log::Action::Warn,
                &format!(
                    "rdc binary at {} is not in a writable directory.\n\
                     To upgrade manually (PowerShell):\n\n  \
                     Invoke-WebRequest -Uri \"{url}\" -OutFile \"$env:TEMP\\{asset}\"\n  \
                     tar -xzf \"$env:TEMP\\{asset}\" -C \"$env:TEMP\"\n  \
                     Move-Item -Force \"$env:TEMP\\{bin}\" \"{}\"\n",
                    path.display(),
                    path.display(),
                ),
            );
            #[cfg(not(windows))]
            log.event(
                crate::log::Action::Warn,
                &format!(
                    "rdc binary at {} is not in a writable directory.\n\
                     To upgrade manually:\n\n  \
                     curl -fsSL {url} -o /tmp/{asset}\n  \
                     tar xzf /tmp/{asset} -C /tmp\n  \
                     mv /tmp/{bin} {}\n",
                    path.display(),
                    path.display(),
                ),
            );
            anyhow::bail!("install location not writable");
        }
        InstallLocation::Replaceable(path) => path.clone(),
    };

    let asset_name = platform_asset_name()?;
    let url = asset_download_url(&latest, &asset_name);

    // Wrap the download in a spinner so the user sees activity while
    // multi-megabyte tarball bytes are streaming. Spinner only — no
    // per-byte progress bar (per the progress UX spec).
    let progress = crate::log::Log::new(crate::cli::resolve::detect_color_mode(false));
    progress.event(crate::log::Action::Upgr, &format!("downloading {asset_name}"));
    let bytes_result = async {
        http_client(UPGRADE_TIMEOUT)?
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()
            .with_context(|| format!("non-2xx from {url}"))?
            .bytes()
            .await
            .context("reading tarball body")
    }
    .await;
    let bytes = match bytes_result {
        Ok(b) => {
            progress.event(crate::log::Action::Upgr, &format!("downloaded {} bytes", b.len()));
            b
        }
        Err(e) => {
            progress.event(crate::log::Action::Upgr, "fail download");
            return Err(e);
        }
    };

    let tmp = tempfile::tempdir().context("creating temp dir for upgrade")?;
    extract_rdc_binary(bytes.as_ref(), tmp.path())
        .context("extracting rdc from tarball")?;

    let staged = tmp.path().join(binary_filename());
    if !staged.exists() {
        anyhow::bail!(
            "tarball did not contain a '{}' binary at the root",
            binary_filename()
        );
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

    // Self-replace pattern. Two cases:
    //
    // Unix: rename(staged, target) is atomic only when both paths sit
    // on the same filesystem. The staged binary lives under
    // tempfile::tempdir() which is often /tmp — a different fs from the
    // install dir. So we first COPY the staged binary into a sibling of
    // target (same dir → same fs), then atomically rename it over
    // target. The kernel keeps the old binary's inode alive for the
    // running process, so this self-update completes without breaking
    // the in-flight upgrade. We use COPY (not rename) for the backup so
    // target_path is never absent from the directory — a parallel shell
    // that runs `rdc` during the upgrade always sees a valid binary.
    //
    // Windows: a running .exe cannot be overwritten or deleted, but it
    // CAN be renamed. So we stage into a sibling, rename target →
    // backup (which the OS allows even though target is the running
    // process), then rename staging → target. If the second rename
    // fails we restore the backup. The running rdc keeps executing
    // because Windows tracks the open handle by id, not path.
    let target_dir = target_path.parent().ok_or_else(|| {
        anyhow!("target path {} has no parent directory", target_path.display())
    })?;

    // 1. Stage the new binary in the target's directory. Windows needs
    //    the `.exe` extension on the staging path so the pre-flight
    //    Command::new()/rename behave correctly.
    let staging_name = if cfg!(windows) {
        format!(".rdc.new.{}.exe", std::process::id())
    } else {
        format!(".rdc.new.{}", std::process::id())
    };
    let staging_path = target_dir.join(staging_name);
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

    let backup_path = target_path.with_file_name(backup_filename());
    let _ = std::fs::remove_file(&backup_path); // overwrite previous backup

    #[cfg(not(windows))]
    {
        // Unix path: copy aside, then atomic rename.
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
    }

    #[cfg(windows)]
    {
        // Windows path: rename aside (allowed for a running .exe), then
        // place the staged binary at the original path. Roll back on
        // failure so the user never ends up without a working `rdc`.
        if let Err(e) = std::fs::rename(&target_path, &backup_path) {
            let _ = std::fs::remove_file(&staging_path);
            return Err(anyhow::Error::new(e)).with_context(|| {
                format!(
                    "renaming current binary {} -> {} (no changes applied)",
                    target_path.display(),
                    backup_path.display()
                )
            });
        }
        if let Err(e) = std::fs::rename(&staging_path, &target_path) {
            // Rollback: put the original back where it was.
            let _ = std::fs::rename(&backup_path, &target_path);
            let _ = std::fs::remove_file(&staging_path);
            return Err(anyhow::Error::new(e)).with_context(|| {
                format!(
                    "placing new binary at {} (rolled back to previous)",
                    target_path.display(),
                )
            });
        }
    }

    // Refresh the nudge cache so we don't keep telling the user about
    // the upgrade they just did.
    save_cache(&UpdateCache {
        checked_at: now_unix_seconds(),
        latest: latest.to_string(),
    });

    progress.event(crate::log::Action::Upgr, &format!("done v{current_v} -> v{latest}"));
    println!(
        "upgraded rdc v{current_v} -> v{latest}\nprevious binary at {} (delete when ready)",
        backup_path.display()
    );
    Ok(())
}

/// Walk a `.tar.gz` byte slice and extract the rdc binary to `dest_dir`.
/// The tarball produced by `.github/workflows/release.yaml` has the
/// binary at the root; we accept any entry whose file name matches
/// the platform-appropriate binary (`rdc` on Unix, `rdc.exe` on
/// Windows) regardless of path prefix.
fn extract_rdc_binary(gz_bytes: &[u8], dest_dir: &Path) -> Result<()> {
    let gz = flate2::read::GzDecoder::new(gz_bytes);
    let mut archive = tar::Archive::new(gz);
    let expected = binary_filename();
    for entry in archive.entries().context("reading tarball entries")? {
        let mut entry = entry.context("reading tarball entry")?;
        let entry_path = entry.path().context("reading tarball entry path")?.into_owned();
        let file_name = match entry_path.file_name() {
            Some(n) => n.to_owned(),
            None => continue,
        };
        if file_name == expected {
            let dest = dest_dir.join(expected);
            entry
                .unpack(&dest)
                .with_context(|| format!("unpacking {expected} to {}", dest.display()))?;
            return Ok(());
        }
    }
    anyhow::bail!("no '{expected}' binary inside tarball")
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
        let home = home_dir().expect("test requires HOME or USERPROFILE");
        let p = home.join(".cargo").join("bin").join("rdc");
        assert!(is_under_cargo_bin(&p));
        let p2 = home.join(".local").join("bin").join("rdc");
        assert!(!is_under_cargo_bin(&p2));
    }

    #[test]
    fn binary_filename_matches_platform() {
        if cfg!(windows) {
            assert_eq!(binary_filename(), "rdc.exe");
            assert_eq!(backup_filename(), "rdc.bak.exe");
        } else {
            assert_eq!(binary_filename(), "rdc");
            assert_eq!(backup_filename(), "rdc.bak");
        }
    }

    #[test]
    fn platform_asset_name_windows_format() {
        // Just check the format hasn't regressed for current platform.
        if let Ok(name) = platform_asset_name() {
            assert!(name.ends_with(".tar.gz"));
            if cfg!(target_os = "windows") {
                assert!(name.contains("pc-windows-msvc"));
            }
        }
    }
}
