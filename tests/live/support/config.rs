use std::sync::{Mutex, OnceLock};

/// Serializes tests that mutate process-global env vars.
pub(crate) fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

/// Resolved live-test configuration. All fields come from the environment;
/// the repo never hardcodes a host, org id, or token.
#[derive(Debug, Clone)]
pub struct LiveConfig {
    pub api_base: String,
    pub org_id: u64,
    pub token: String,
}

impl LiveConfig {
    /// Returns `Some` only when every required var is present and parseable.
    /// Token may come from `RDC_LIVE_TOKEN`.
    pub fn from_env() -> Option<LiveConfig> {
        let api_base = std::env::var("RDC_LIVE_API_BASE").ok()?;
        let org_id = std::env::var("RDC_LIVE_ORG_ID").ok()?.parse::<u64>().ok()?;
        let token = std::env::var("RDC_LIVE_TOKEN").ok()?;
        if api_base.is_empty() || token.is_empty() {
            return None;
        }
        Some(LiveConfig { api_base, org_id, token })
    }

    /// Human-readable reason printed when a live scenario skips.
    #[allow(dead_code)]
    pub fn skip_reason() -> String {
        "SKIP live test: set RDC_LIVE_API_BASE, RDC_LIVE_ORG_ID, RDC_LIVE_TOKEN \
         to run (see tests/live.rs)"
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_env_is_none_when_vars_absent() {
        let _g = env_lock();
        // SAFETY: serialized by env_lock; restored below.
        unsafe {
            std::env::remove_var("RDC_LIVE_API_BASE");
            std::env::remove_var("RDC_LIVE_ORG_ID");
            std::env::remove_var("RDC_LIVE_TOKEN");
        }
        assert!(LiveConfig::from_env().is_none());
    }

    #[test]
    fn from_env_parses_when_all_present() {
        let _g = env_lock();
        unsafe {
            std::env::set_var("RDC_LIVE_API_BASE", "https://example.rossum.app/api/v1");
            std::env::set_var("RDC_LIVE_ORG_ID", "12345");
            std::env::set_var("RDC_LIVE_TOKEN", "tok");
        }
        let cfg = LiveConfig::from_env().expect("config present");
        assert_eq!(cfg.org_id, 12345);
        assert_eq!(cfg.api_base, "https://example.rossum.app/api/v1");
        assert_eq!(cfg.token, "tok");
        unsafe {
            std::env::remove_var("RDC_LIVE_API_BASE");
            std::env::remove_var("RDC_LIVE_ORG_ID");
            std::env::remove_var("RDC_LIVE_TOKEN");
        }
    }
}
