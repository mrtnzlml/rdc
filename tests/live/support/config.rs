use std::sync::{Mutex, OnceLock};

/// Serializes tests that mutate process-global env vars.
pub(crate) fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

/// Credentials for ONE environment: a host, an org inside it, and a token.
#[derive(Debug, Clone)]
pub struct EnvCreds {
    pub api_base: String,
    pub org_id: u64,
    pub token: String,
}

/// Resolved live-test configuration. All fields come from the environment;
/// the repo never hardcodes a host, org id, or token.
///
/// The top-level fields are the SOURCE env. [`LiveConfig::target`] is an
/// optional SECOND org (`RDC_LIVE_TGT_*`) and is what makes a promotion
/// scenario honest: with one org, `test` and `prod` are two snapshots of the
/// same objects, so the source env's own objects appear in the target's
/// whole-org pull, `--mirror` reads them as target-only extras, and
/// convergence of the source after a deploy is unassertable. With a real
/// second org none of that applies. Scenarios that can run either way say so
/// individually.
#[derive(Debug, Clone)]
pub struct LiveConfig {
    pub api_base: String,
    pub org_id: u64,
    pub token: String,
    pub target: Option<EnvCreds>,
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
        Some(LiveConfig { api_base, org_id, token, target: target_from_env() })
    }

    /// The source env's credentials, in the same shape as [`Self::target`].
    #[allow(dead_code)]
    pub fn source(&self) -> EnvCreds {
        EnvCreds {
            api_base: self.api_base.clone(),
            org_id: self.org_id,
            token: self.token.clone(),
        }
    }

    /// Human-readable reason printed when a live scenario skips.
    #[allow(dead_code)]
    pub fn skip_reason() -> String {
        "SKIP live test: set RDC_LIVE_API_BASE, RDC_LIVE_ORG_ID, RDC_LIVE_TOKEN \
         to run (see tests/live.rs)"
            .to_string()
    }

    /// Reason printed when a scenario needs the second org and it is absent.
    #[allow(dead_code)]
    pub fn skip_reason_target() -> String {
        "SKIP live test: this scenario needs a SECOND org — set \
         RDC_LIVE_TGT_API_BASE, RDC_LIVE_TGT_ORG_ID, RDC_LIVE_TGT_TOKEN \
         (see tests/live.rs)"
            .to_string()
    }
}

/// `RDC_LIVE_TGT_*`, all-or-nothing: a half-configured target is treated as
/// absent rather than silently falling back to the source org, which would
/// make a cross-org scenario quietly become a same-org one and assert nothing.
fn target_from_env() -> Option<EnvCreds> {
    let api_base = std::env::var("RDC_LIVE_TGT_API_BASE").ok()?;
    let org_id = std::env::var("RDC_LIVE_TGT_ORG_ID").ok()?.parse::<u64>().ok()?;
    let token = std::env::var("RDC_LIVE_TGT_TOKEN").ok()?;
    if api_base.is_empty() || token.is_empty() {
        return None;
    }
    Some(EnvCreds { api_base, org_id, token })
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
        assert!(cfg.target.is_none(), "no RDC_LIVE_TGT_* set");
        unsafe {
            std::env::remove_var("RDC_LIVE_API_BASE");
            std::env::remove_var("RDC_LIVE_ORG_ID");
            std::env::remove_var("RDC_LIVE_TOKEN");
        }
    }

    #[test]
    fn target_is_read_from_its_own_vars() {
        let _g = env_lock();
        unsafe {
            std::env::set_var("RDC_LIVE_API_BASE", "https://a.example/api/v1");
            std::env::set_var("RDC_LIVE_ORG_ID", "1");
            std::env::set_var("RDC_LIVE_TOKEN", "tok-a");
            std::env::set_var("RDC_LIVE_TGT_API_BASE", "https://b.example/api/v1");
            std::env::set_var("RDC_LIVE_TGT_ORG_ID", "2");
            std::env::set_var("RDC_LIVE_TGT_TOKEN", "tok-b");
        }
        let cfg = LiveConfig::from_env().expect("config present");
        let tgt = cfg.target.clone().expect("target present");
        assert_eq!((tgt.org_id, tgt.token.as_str()), (2, "tok-b"));
        assert_eq!(cfg.source().org_id, 1);
        for v in ["RDC_LIVE_TGT_API_BASE", "RDC_LIVE_TGT_ORG_ID", "RDC_LIVE_TGT_TOKEN"] {
            unsafe { std::env::remove_var(v) };
        }
    }

    /// A half-configured target must read as ABSENT. Falling back to the source
    /// org would turn a cross-org promotion scenario into a same-org one that
    /// silently stops testing what it claims to.
    #[test]
    fn a_partial_target_is_treated_as_absent() {
        let _g = env_lock();
        unsafe {
            std::env::set_var("RDC_LIVE_API_BASE", "https://a.example/api/v1");
            std::env::set_var("RDC_LIVE_ORG_ID", "1");
            std::env::set_var("RDC_LIVE_TOKEN", "tok-a");
            std::env::set_var("RDC_LIVE_TGT_API_BASE", "https://b.example/api/v1");
            std::env::remove_var("RDC_LIVE_TGT_ORG_ID");
            std::env::set_var("RDC_LIVE_TGT_TOKEN", "tok-b");
        }
        assert!(LiveConfig::from_env().expect("config").target.is_none());
        for v in ["RDC_LIVE_TGT_API_BASE", "RDC_LIVE_TGT_TOKEN"] {
            unsafe { std::env::remove_var(v) };
        }
    }
}
