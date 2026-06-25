use std::time::{SystemTime, UNIX_EPOCH};

/// Per-process unique, slug-safe namespace token. Every seeded object name is
/// prefixed with `rdc-it-<id>-` so concurrent or crashed runs never collide
/// and a janitor can identify leftovers.
#[derive(Debug, Clone)]
pub struct RunId(String);

impl RunId {
    pub fn new() -> RunId {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let pid = std::process::id() as u128;
        // base36 of (nanos XOR-mixed with pid), lowercase alnum only.
        let mixed = nanos.wrapping_mul(1_000_003).wrapping_add(pid);
        RunId(format!("{}", to_base36(mixed)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `rdc-it-<id>-<name>` — used as the *display name* sent to the API.
    pub fn prefix(&self, name: &str) -> String {
        format!("{}{}-{}", Self::marker(), self.0, name)
    }

    /// Stable substring shared by every object this harness creates.
    pub fn marker() -> &'static str {
        "rdc-it-"
    }
}

fn to_base36(mut n: u128) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".to_string();
    }
    let mut out = Vec::new();
    while n > 0 {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_is_slug_safe_and_marked() {
        let id = RunId::new();
        let p = id.prefix("Invoices Alpha");
        assert!(p.starts_with(RunId::marker()));
        assert!(p.contains("-Invoices Alpha"));
        // the id segment is lowercase alnum
        assert!(id.as_str().chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
        assert!(!id.as_str().is_empty());
    }
}
