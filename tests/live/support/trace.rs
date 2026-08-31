//! Parse the `RDC_TRACE_HTTP` CSV so a scenario can assert the ORDER in which
//! `rdc` issued its requests.
//!
//! `src/api/retry.rs` writes one line per HTTP **attempt**, from the single
//! `send_once` chokepoint both the core API client and the Data Storage client
//! funnel through:
//!
//! ```text
//! epoch_ms,limiter_wait_ms,duration_ms,status,desc
//! 1788174659806.4,0.0,371.1,200,GET https://api.elis.rossum.ai/v1/workspaces?page=1
//! ```
//!
//! Two properties of that format drive the parser. `desc` is LAST and unquoted
//! and may itself contain commas, so a row splits on the first four separators
//! only. And because both clients share the sink, a run's file also holds
//! `POST https://elis.rossum.ai/svc/data-storage/...` lines from MDH — which
//! carry their own `/v1/` segment and would otherwise be mistaken for core-API
//! calls. [`Trace::endpoint_of`] rejects them explicitly.
//!
//! # Why last-of-A vs first-of-B is a sound ordering test
//!
//! `push::push_classified` awaits its per-kind drivers one after another, so
//! requests of two different kinds can never interleave, however much
//! concurrency a single driver uses internally (`push/concurrent.rs`).
//! Comparing the LAST index of kind A against the FIRST index of kind B is
//! therefore exact, and it is also immune to a retried attempt appearing in the
//! file twice.

use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceLine {
    pub status: String,
    pub method: String,
    pub url: String,
}

#[derive(Debug, Clone, Default)]
pub struct Trace {
    pub lines: Vec<TraceLine>,
}

#[allow(dead_code)]
impl Trace {
    /// Read and parse a trace file. A missing file is an EMPTY trace, not an
    /// error: a command that issued no request writes nothing at all.
    pub fn read(path: &Path) -> Trace {
        match std::fs::read_to_string(path) {
            Ok(raw) => Trace::parse(&raw),
            Err(_) => Trace::default(),
        }
    }

    pub fn parse(raw: &str) -> Trace {
        let mut lines = Vec::new();
        for row in raw.lines() {
            // epoch,limiter_wait,duration,status,desc — `desc` is last and may
            // contain commas, so split on the first four separators only.
            let mut it = row.splitn(5, ',');
            let (_epoch, _wait, _dur) = (it.next(), it.next(), it.next());
            let (Some(status), Some(desc)) = (it.next(), it.next()) else {
                continue;
            };
            let Some((method, url)) = desc.split_once(' ') else { continue };
            lines.push(TraceLine {
                status: status.to_string(),
                method: method.to_string(),
                url: url.to_string(),
            });
        }
        Trace { lines }
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// The core-API endpoint a url addresses: the first path segment after
    /// `/v1/`, with any query string removed. `None` for a Data Storage url,
    /// which carries its own `/v1/` but is a different service.
    fn endpoint_of(url: &str) -> Option<&str> {
        if url.contains("/svc/data-storage/") {
            return None;
        }
        let after = url.split("/v1/").nth(1)?;
        let seg = after.split(['/', '?']).next()?;
        (!seg.is_empty()).then_some(seg)
    }

    fn positions(&self, method: &str, endpoint: &str) -> Vec<usize> {
        self.lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.method == method && Trace::endpoint_of(&l.url) == Some(endpoint))
            .map(|(i, _)| i)
            .collect()
    }

    pub fn first(&self, method: &str, endpoint: &str) -> Option<usize> {
        self.positions(method, endpoint).first().copied()
    }

    pub fn last(&self, method: &str, endpoint: &str) -> Option<usize> {
        self.positions(method, endpoint).last().copied()
    }

    /// Render the trace rows in `lo..=hi` (clamped), one per line, for a
    /// failure message that says what actually ran.
    fn window(&self, lo: usize, hi: usize) -> String {
        let lo = lo.saturating_sub(2);
        let hi = (hi + 2).min(self.lines.len().saturating_sub(1));
        self.lines[lo..=hi]
            .iter()
            .enumerate()
            .map(|(n, l)| format!("  #{:<4} {} {} {}", lo + n, l.status, l.method, l.url))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Assert every `a` request precedes every `b` request. Both must have
    /// occurred — a missing one is a failure, not a vacuous pass, because the
    /// commonest way for an ordering test to rot is for the request to stop
    /// being made at all.
    pub fn assert_before(&self, a: (&str, &str), b: (&str, &str), why: &str) {
        let last_a = self.last(a.0, a.1).unwrap_or_else(|| {
            panic!(
                "ordering: no `{} /{}` in the trace, so `{} /{}` -> `{} /{}` could not be \
                 checked ({why}).\nTrace:\n{}",
                a.0, a.1, a.0, a.1, b.0, b.1,
                self.window(0, self.lines.len().saturating_sub(1))
            )
        });
        let first_b = self.first(b.0, b.1).unwrap_or_else(|| {
            panic!(
                "ordering: no `{} /{}` in the trace, so `{} /{}` -> `{} /{}` could not be \
                 checked ({why}).\nTrace:\n{}",
                b.0, b.1, a.0, a.1, b.0, b.1,
                self.window(0, self.lines.len().saturating_sub(1))
            )
        });
        assert!(
            last_a < first_b,
            "ordering violated: the last `{} /{}` is at #{last_a} but the first `{} /{}` is \
             at #{first_b} — {why}.\nTrace around the violation:\n{}",
            a.0, a.1, b.0, b.1,
            self.window(first_b.min(last_a), first_b.max(last_a))
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trace with the shape `src/api/retry.rs` really writes, including a
    /// Data Storage line and a `desc` containing a comma-bearing query string.
    const SAMPLE: &str = "\
1788174659802.8,0.0,367.6,200,GET https://api.elis.rossum.ai/v1/organizations/214757
1788174659838.5,0.0,404.0,200,POST https://elis.rossum.ai/svc/data-storage/api/v1/collections/list
1788174659900.0,0.0,110.0,201,POST https://api.elis.rossum.ai/v1/engines
1788174659950.0,0.0,110.0,201,POST https://api.elis.rossum.ai/v1/engine_fields
1788174660000.0,0.0,110.0,201,POST https://api.elis.rossum.ai/v1/schemas
1788174660100.0,0.0,110.0,201,POST https://api.elis.rossum.ai/v1/queues
1788174660200.0,0.0,110.0,200,GET https://api.elis.rossum.ai/v1/queues?page_size=100,ordering=id
";

    #[test]
    fn parses_method_status_and_url() {
        let t = Trace::parse(SAMPLE);
        assert_eq!(t.lines.len(), 7);
        assert_eq!(t.lines[2].method, "POST");
        assert_eq!(t.lines[2].status, "201");
        assert_eq!(t.lines[2].url, "https://api.elis.rossum.ai/v1/engines");
    }

    /// `desc` is unquoted and may contain commas; splitting on all of them
    /// would truncate the url.
    #[test]
    fn a_comma_inside_desc_does_not_truncate_the_url() {
        let t = Trace::parse(SAMPLE);
        assert_eq!(t.lines[6].url, "https://api.elis.rossum.ai/v1/queues?page_size=100,ordering=id");
    }

    /// Data Storage shares the sink and carries its own `/v1/`. Mistaking it
    /// for a core-API call would make MDH traffic pollute every assertion.
    #[test]
    fn data_storage_urls_are_not_core_api_endpoints() {
        let t = Trace::parse(SAMPLE);
        assert_eq!(t.positions("POST", "collections"), Vec::<usize>::new());
    }

    #[test]
    fn endpoint_ignores_the_id_and_the_query() {
        assert_eq!(
            Trace::endpoint_of("https://api.elis.rossum.ai/v1/queues/4135179"),
            Some("queues")
        );
        assert_eq!(
            Trace::endpoint_of("https://api.elis.rossum.ai/v1/queues?page=1"),
            Some("queues")
        );
    }

    #[test]
    fn assert_before_passes_on_correct_order() {
        Trace::parse(SAMPLE).assert_before(
            ("POST", "engine_fields"),
            ("POST", "queues"),
            "engine fields must exist before a queue binds the engine",
        );
    }

    #[test]
    #[should_panic(expected = "ordering violated")]
    fn assert_before_fails_on_reversed_order() {
        Trace::parse(SAMPLE).assert_before(
            ("POST", "queues"),
            ("POST", "engines"),
            "deliberately backwards",
        );
    }

    /// A request that stopped being made must fail loudly rather than pass by
    /// checking nothing — the commonest way an ordering test rots.
    #[test]
    #[should_panic(expected = "no `POST /labels`")]
    fn assert_before_fails_when_a_request_is_absent() {
        Trace::parse(SAMPLE).assert_before(
            ("POST", "labels"),
            ("POST", "rules"),
            "absent on both sides",
        );
    }

    #[test]
    fn a_missing_file_is_an_empty_trace() {
        assert!(Trace::read(std::path::Path::new("/nonexistent/trace.csv")).is_empty());
    }
}
