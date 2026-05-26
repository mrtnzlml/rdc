use crate::connection::Connection;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use ulid::Ulid;

const REGISTRY_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default = "default_version")]
    version: u32,
    #[serde(default)]
    connections: Vec<Connection>,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            version: REGISTRY_VERSION,
            connections: Vec::new(),
        }
    }
}

fn default_version() -> u32 {
    REGISTRY_VERSION
}

impl Registry {
    pub fn connections(&self) -> &[Connection] {
        &self.connections
    }

    pub fn upsert(&mut self, conn: Connection) {
        if let Some(slot) = self.connections.iter_mut().find(|c| c.id == conn.id) {
            *slot = conn;
        } else {
            self.connections.push(conn);
        }
    }

    pub fn remove(&mut self, id: Ulid) -> Option<Connection> {
        let pos = self.connections.iter().position(|c| c.id == id)?;
        Some(self.connections.remove(pos))
    }

    pub fn get(&self, id: Ulid) -> Option<&Connection> {
        self.connections.iter().find(|c| c.id == id)
    }

    pub fn used_slugs(&self) -> std::collections::HashSet<String> {
        self.connections.iter().map(|c| c.slug.clone()).collect()
    }

    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => {
                let mut reg: Registry = serde_json::from_slice(&bytes)
                    .with_context(|| format!("parsing {}", path.display()))?;
                if reg.version != REGISTRY_VERSION {
                    anyhow::bail!(
                        "unsupported registry version {} (expected {})",
                        reg.version,
                        REGISTRY_VERSION
                    );
                }
                // Defensive: deduplicate by id (in case the file was hand-edited)
                let mut seen = std::collections::HashSet::new();
                reg.connections.retain(|c| seen.insert(c.id));
                Ok(reg)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(self).context("serializing registry")?;
        rdc::snapshot::writer::write_atomic(path, &bytes)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}
