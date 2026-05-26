use anyhow::{anyhow, Context, Result};
use std::path::Path;
use std::process::Command;

/// Move the given path to the user's Trash via `osascript` (Finder
/// `move ... to trash`). This is the public-API macOS pattern that
/// preserves "Put Back" recoverability — Finder records the original
/// location.
#[cfg(target_os = "macos")]
pub fn trash_folder(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let p = path
        .to_str()
        .ok_or_else(|| anyhow!("non-UTF-8 path: {}", path.display()))?;
    let script = format!(
        r#"tell application "Finder" to move POSIX file "{}" to trash"#,
        p.replace('"', r#"\""#)
    );
    let out = Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .output()
        .context("running osascript")?;
    if !out.status.success() {
        return Err(anyhow!(
            "osascript failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn trash_folder(path: &Path) -> Result<()> {
    // Linux/Windows builds (future). For now, fall back to a permanent
    // delete with explicit logging — the caller is responsible for
    // confirming with the user.
    if path.exists() {
        std::fs::remove_dir_all(path)
            .with_context(|| format!("removing {}", path.display()))?;
    }
    Ok(())
}

/// Reveal the given path in the OS file manager. On macOS this opens
/// the folder in Finder; on Linux/Windows it dispatches via the default
/// handler. Used by the "Reveal in Finder" UI action — invoked from
/// JS via the standard `invoke()` surface, no plugin globals required.
pub fn reveal(path: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(target_os = "linux")]
    let program = "xdg-open";
    #[cfg(target_os = "windows")]
    let program = "explorer";

    Command::new(program)
        .arg(path)
        .spawn()
        .with_context(|| format!("spawning {program} {}", path.display()))?;
    Ok(())
}
