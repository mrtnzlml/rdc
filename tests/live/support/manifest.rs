use anyhow::{anyhow, bail, Result};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Deserialize)]
pub struct ObjectSpec {
    pub key: String,
    #[allow(dead_code)]
    pub kind: String,
    #[allow(dead_code)]
    pub body: String,
    #[serde(default)]
    pub deps: Vec<String>,
    #[allow(dead_code)]
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    #[serde(rename = "object", default)]
    pub objects: Vec<ObjectSpec>,
}

impl Manifest {
    pub fn parse(toml_src: &str) -> Result<Manifest> {
        let m: Manifest = toml::from_str(toml_src).map_err(|e| anyhow!("parsing manifest: {e}"))?;
        // keys must be unique
        let mut seen = BTreeSet::new();
        for o in &m.objects {
            if !seen.insert(o.key.clone()) {
                bail!("duplicate manifest key: {}", o.key);
            }
        }
        Ok(m)
    }

    /// Kahn topological sort over `deps`. Errors on unknown dep or cycle.
    pub fn topo_order(&self) -> Result<Vec<&ObjectSpec>> {
        let by_key: BTreeMap<&str, &ObjectSpec> =
            self.objects.iter().map(|o| (o.key.as_str(), o)).collect();
        for o in &self.objects {
            for d in &o.deps {
                if !by_key.contains_key(d.as_str()) {
                    bail!("object {} depends on unknown key {}", o.key, d);
                }
            }
        }
        let mut indeg: BTreeMap<&str, usize> =
            self.objects.iter().map(|o| (o.key.as_str(), o.deps.len())).collect();
        let mut ready: Vec<&str> =
            indeg.iter().filter(|&(_, &d)| d == 0).map(|(&k, _)| k).collect();
        ready.sort();
        let mut out = Vec::new();
        while let Some(k) = ready.pop() {
            out.push(by_key[k]);
            for o in &self.objects {
                if o.deps.iter().any(|d| d == k) {
                    let e = indeg.get_mut(o.key.as_str()).unwrap();
                    *e -= 1;
                    if *e == 0 {
                        ready.push(o.key.as_str());
                        ready.sort();
                    }
                }
            }
        }
        if out.len() != self.objects.len() {
            bail!("dependency cycle in manifest");
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[[object]]
key = "ws"
kind = "workspace"
body = "bodies/workspaces/ws.json"

[[object]]
key = "schema"
kind = "schema"
body = "bodies/schemas/schema.json"

[[object]]
key = "queue"
kind = "queue"
body = "bodies/queues/queue.json"
deps = ["ws", "schema"]
tags = ["core"]
"#;

    #[test]
    fn parses_and_orders_deps_first() {
        let m = Manifest::parse(SAMPLE).unwrap();
        assert_eq!(m.objects.len(), 3);
        let order: Vec<&str> = m.topo_order().unwrap().iter().map(|o| o.key.as_str()).collect();
        let qpos = order.iter().position(|k| *k == "queue").unwrap();
        let wpos = order.iter().position(|k| *k == "ws").unwrap();
        let spos = order.iter().position(|k| *k == "schema").unwrap();
        assert!(wpos < qpos && spos < qpos, "deps must precede dependent: {order:?}");
    }

    #[test]
    fn errors_on_unknown_dep() {
        let bad = "[[object]]\nkey=\"a\"\nkind=\"queue\"\nbody=\"b\"\ndeps=[\"missing\"]\n";
        assert!(Manifest::parse(bad).unwrap().topo_order().is_err());
    }
}
