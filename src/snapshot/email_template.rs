use crate::model::EmailTemplate;
use crate::snapshot::writer::write_atomic;
use anyhow::{Context, Result};
use std::path::Path;

/// Write an email template as `<dir>/<slug>.json`. Returns the bytes written.
pub fn write_email_template(dir: &Path, slug: &str, t: &EmailTemplate) -> Result<Vec<u8>> {
    let path = dir.join(format!("{slug}.json"));
    let bytes = crate::snapshot::key_order::serialize_for_disk(t)
        .context("serializing email template")?;
    write_atomic(&path, &bytes)?;
    Ok(bytes)
}

pub fn read_email_template(dir: &Path, slug: &str) -> Result<EmailTemplate> {
    let path = dir.join(format!("{slug}.json"));
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn sample() -> EmailTemplate {
        let v = json!({
            "id": 1,
            "url": "https://x/api/v1/email_templates/1",
            "name": "T",
            "subject": "Subj",
            "queue": "https://x/api/v1/queues/1"
        });
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn round_trip() {
        let dir = TempDir::new().unwrap();
        let original = sample();
        write_email_template(dir.path(), "t", &original).unwrap();
        let read = read_email_template(dir.path(), "t").unwrap();
        assert_eq!(original, read);
    }
}
