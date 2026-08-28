use crate::support::config::{EnvCreds, LiveConfig};
use anyhow::{Context, Result};
use std::path::Path;
use std::process::Output;
use tempfile::TempDir;

pub struct ProjectFixture {
    dir: TempDir,
}

impl ProjectFixture {
    /// Bootstrap a tempdir project with one or more envs all pointing at the
    /// SAME live org, plus a secrets file per env containing the live token.
    pub fn init(cfg: &LiveConfig, envs: &[&str]) -> Result<ProjectFixture> {
        let creds = cfg.source();
        let pairs: Vec<(&str, &EnvCreds)> = envs.iter().map(|e| (*e, &creds)).collect();
        Self::init_envs(&pairs)
    }

    /// Bootstrap a tempdir project where each env points at its OWN org.
    ///
    /// This is what a real promotion looks like: `test` in one organization,
    /// `prod` in another. Pointing both at one org (what [`Self::init`] does)
    /// makes every env a view of the same objects, which quietly defeats any
    /// assertion about one env not seeing the other's work.
    pub fn init_envs(envs: &[(&str, &EnvCreds)]) -> Result<ProjectFixture> {
        let dir = TempDir::new().context("creating tempdir")?;
        let mut args: Vec<String> = vec!["init".into()];
        for (env, creds) in envs {
            args.push("--env".into());
            args.push(format!("{}={}:{}", env, creds.api_base, creds.org_id));
        }
        let out = assert_cmd::Command::cargo_bin("rdc")
            .context("locating rdc binary")?
            .current_dir(dir.path())
            .args(&args)
            .output()
            .context("running rdc init")?;
        anyhow::ensure!(
            out.status.success(),
            "rdc init failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        for (env, creds) in envs {
            let secrets = serde_json::json!({ "api_token": creds.token });
            std::fs::write(
                dir.path().join(format!("secrets/{env}.secrets.json")),
                serde_json::to_vec_pretty(&secrets)?,
            )
            .with_context(|| format!("writing secrets for {env}"))?;
        }
        Ok(ProjectFixture { dir })
    }

    #[allow(dead_code)]
    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    #[allow(dead_code)]
    pub fn run_rdc(&self, args: &[&str]) -> Output {
        assert_cmd::Command::cargo_bin("rdc")
            .unwrap()
            .current_dir(self.dir.path())
            .args(args)
            .output()
            .expect("spawning rdc")
    }

    #[allow(dead_code)]
    pub fn read_json(&self, rel: &str) -> serde_json::Value {
        let raw = std::fs::read_to_string(self.dir.path().join(rel))
            .unwrap_or_else(|e| panic!("reading {rel}: {e}"));
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parsing {rel}: {e}"))
    }

    #[allow(dead_code)]
    pub fn read_to_string(&self, rel: &str) -> Option<String> {
        std::fs::read_to_string(self.dir.path().join(rel)).ok()
    }

    #[allow(dead_code)]
    pub fn exists(&self, rel: &str) -> bool {
        self.dir.path().join(rel).exists()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Hermetic: uses a fake config and asserts the files `rdc init` writes.
    // Requires the `rdc` binary to be built (it is, as the test target's bin
    // dependency), but makes NO network calls.
    #[test]
    fn init_writes_rdc_toml_and_secrets() {
        let cfg = LiveConfig {
            api_base: "https://example.rossum.app/api/v1".into(),
            org_id: 999,
            token: "tok".into(),
            target: None,
        };
        let p = ProjectFixture::init(&cfg, &["test", "prod"]).unwrap();
        assert!(p.exists("rdc.toml"));
        let toml = p.read_to_string("rdc.toml").unwrap();
        assert!(toml.contains("[envs.test]"));
        assert!(toml.contains("[envs.prod]"));
        assert!(toml.contains("api_base = \"https://example.rossum.app/api/v1\""));
        let sec = p.read_json("secrets/test.secrets.json");
        assert_eq!(sec["api_token"], "tok");
    }

    // Hermetic: two envs, two DIFFERENT orgs and tokens.
    #[test]
    fn init_envs_gives_each_env_its_own_org_and_token() {
        let a = EnvCreds {
            api_base: "https://a.example/api/v1".into(),
            org_id: 111,
            token: "tok-a".into(),
        };
        let b = EnvCreds {
            api_base: "https://b.example/api/v1".into(),
            org_id: 222,
            token: "tok-b".into(),
        };
        let p = ProjectFixture::init_envs(&[("test", &a), ("prod", &b)]).unwrap();
        let toml = p.read_to_string("rdc.toml").unwrap();
        assert!(toml.contains("https://a.example/api/v1"), "{toml}");
        assert!(toml.contains("https://b.example/api/v1"), "{toml}");
        assert!(toml.contains("org_id = 111"), "{toml}");
        assert!(toml.contains("org_id = 222"), "{toml}");
        assert_eq!(p.read_json("secrets/test.secrets.json")["api_token"], "tok-a");
        assert_eq!(p.read_json("secrets/prod.secrets.json")["api_token"], "tok-b");
    }
}
