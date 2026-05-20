//! Grid renderer for `rdc sync` and `rdc sync --watch`.
//!
//! Spec: docs/superpowers/specs/2026-05-20-sync-grid-visualization-design.md

use std::time::{Duration, Instant};
use crate::cli::sync::classify::SyncClass;

/// Painted color of a single square. Resolved to an ANSI escape by
/// [`emit_square`] (Task 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    FreshGreen,
    Green,
    Yellow,
    Orange,
    StaleRed,
    PendingOrange,
    EditRed,
    ConflictOutlined,
}

/// One tracked `(kind, slug)` in [`GridState`]. Filled in in Task 5.
#[derive(Debug, Clone)]
pub struct Entry {
    pub last_verified_at: Instant,
    pub class: SyncClass,
    pub in_flight: Option<crate::progress::ResourceOp>,
}

/// Map an entry to its current paint color. Stamps short-circuit the
/// freshness clock; `Clean` falls through to a 5-band age check.
///
/// The bands match section 5.1 of the spec:
///   0..=15  → FreshGreen
///   16..=60 → Green
///   61..=300 → Yellow
///   301..=900 → Orange
///   _ → StaleRed
pub fn color_for(e: &Entry, now: Instant) -> Color {
    match e.class {
        SyncClass::LocalEdit | SyncClass::LocalCreate | SyncClass::LocalDelete => {
            return Color::EditRed;
        }
        SyncClass::RemoteEdit | SyncClass::RemoteCreate | SyncClass::RemoteDelete => {
            return Color::PendingOrange;
        }
        SyncClass::BothDiverged
        | SyncClass::LocalEditRemoteDelete
        | SyncClass::LocalDeleteRemoteEdit => {
            return Color::ConflictOutlined;
        }
        SyncClass::BothDeleted => {
            // GridState evicts these at ingest time; reaching here is a bug.
            debug_assert!(false, "color_for called on BothDeleted entry");
            return Color::StaleRed;
        }
        SyncClass::Clean => {}
    }

    let age = now.saturating_duration_since(e.last_verified_at).as_secs();
    match age {
        0..=15 => Color::FreshGreen,
        16..=60 => Color::Green,
        61..=300 => Color::Yellow,
        301..=900 => Color::Orange,
        _ => Color::StaleRed,
    }
}

#[cfg(test)]
mod color_for_tests {
    use super::*;

    fn entry_clean_aged(now: Instant, secs: u64) -> Entry {
        Entry {
            last_verified_at: now.checked_sub(Duration::from_secs(secs)).unwrap(),
            class: SyncClass::Clean,
            in_flight: None,
        }
    }

    #[test]
    fn fresh_green_band_includes_zero_and_fifteen() {
        let now = Instant::now();
        assert_eq!(color_for(&entry_clean_aged(now, 0), now), Color::FreshGreen);
        assert_eq!(color_for(&entry_clean_aged(now, 15), now), Color::FreshGreen);
    }

    #[test]
    fn green_band_starts_at_sixteen_and_includes_sixty() {
        let now = Instant::now();
        assert_eq!(color_for(&entry_clean_aged(now, 16), now), Color::Green);
        assert_eq!(color_for(&entry_clean_aged(now, 60), now), Color::Green);
    }

    #[test]
    fn yellow_band_spans_one_to_five_minutes() {
        let now = Instant::now();
        assert_eq!(color_for(&entry_clean_aged(now, 61), now), Color::Yellow);
        assert_eq!(color_for(&entry_clean_aged(now, 300), now), Color::Yellow);
    }

    #[test]
    fn orange_band_spans_five_to_fifteen_minutes() {
        let now = Instant::now();
        assert_eq!(color_for(&entry_clean_aged(now, 301), now), Color::Orange);
        assert_eq!(color_for(&entry_clean_aged(now, 900), now), Color::Orange);
    }

    #[test]
    fn stale_red_beyond_fifteen_minutes() {
        let now = Instant::now();
        assert_eq!(color_for(&entry_clean_aged(now, 901), now), Color::StaleRed);
        assert_eq!(color_for(&entry_clean_aged(now, 7200), now), Color::StaleRed);
    }

    #[test]
    fn local_edit_stamp_overrides_clock_at_any_age() {
        let now = Instant::now();
        let mut e = entry_clean_aged(now, 0);
        e.class = SyncClass::LocalEdit;
        assert_eq!(color_for(&e, now), Color::EditRed);
        let mut e = entry_clean_aged(now, 10_000);
        e.class = SyncClass::LocalEdit;
        assert_eq!(color_for(&e, now), Color::EditRed);
    }

    #[test]
    fn remote_create_stamp_paints_pending_orange() {
        let now = Instant::now();
        let mut e = entry_clean_aged(now, 0);
        e.class = SyncClass::RemoteCreate;
        assert_eq!(color_for(&e, now), Color::PendingOrange);
    }

    #[test]
    fn both_diverged_paints_conflict() {
        let now = Instant::now();
        let mut e = entry_clean_aged(now, 120); // would have been Yellow
        e.class = SyncClass::BothDiverged;
        assert_eq!(color_for(&e, now), Color::ConflictOutlined);
    }
}
