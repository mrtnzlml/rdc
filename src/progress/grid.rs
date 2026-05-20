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

use std::collections::{BTreeMap, VecDeque};
use crate::cli::sync::classify::ClassifiedItem;

/// In-memory state of the grid view. Survives across watch cycles;
/// thrown away on `rdc sync` exit.
pub struct GridState {
    /// (kind, slug) → entry. Populated by `ingest`.
    pub(crate) entries: BTreeMap<(String, String), Entry>,
    /// Per-kind canonical slug order (alphabetical, fixed at first
    /// observation). Drives row layout — new slugs append in
    /// alphabetical insertion order so rows don't shuffle redraws.
    pub(crate) order: BTreeMap<String, Vec<String>>,
    /// Two-cycle no-show eviction: each `ingest` increments this;
    /// entries remember the last cycle they were observed in.
    pub(crate) cycle: u64,
    /// Per-entry last-observed cycle, used by the eviction rule.
    pub(crate) last_seen_cycle: BTreeMap<(String, String), u64>,
    /// Banner queue for transient errors / auth refresh.
    pub(crate) banners: VecDeque<Banner>,
    /// Most recent `phase()` label.
    pub(crate) current_op: String,
    /// Header context.
    pub(crate) env: String,
    pub(crate) started_at: Instant,
    pub(crate) is_watch: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct Banner {
    pub severity: crate::progress::Severity,
    pub text: String,
    pub posted_at: Instant,
}

impl GridState {
    pub fn new(env: String, is_watch: bool) -> Self {
        Self {
            entries: BTreeMap::new(),
            order: BTreeMap::new(),
            cycle: 0,
            last_seen_cycle: BTreeMap::new(),
            banners: VecDeque::new(),
            current_op: String::new(),
            env,
            started_at: Instant::now(),
            is_watch,
        }
    }

    /// Fold a fresh classification snapshot into the grid state.
    /// Creates / updates / evicts entries per spec section 4.2.
    pub fn ingest(&mut self, items: &[ClassifiedItem], now: Instant) {
        self.cycle = self.cycle.wrapping_add(1);
        for it in items {
            // BothDeleted entries never get a square.
            if matches!(it.class, SyncClass::BothDeleted) {
                self.entries.remove(&(it.kind.clone(), it.slug.clone()));
                self.last_seen_cycle.remove(&(it.kind.clone(), it.slug.clone()));
                if let Some(order) = self.order.get_mut(&it.kind) {
                    order.retain(|s| s != &it.slug);
                }
                continue;
            }
            let key = (it.kind.clone(), it.slug.clone());
            self.last_seen_cycle.insert(key.clone(), self.cycle);
            let entry = self.entries.entry(key).or_insert_with(|| Entry {
                last_verified_at: now,
                class: it.class.clone(),
                in_flight: None,
            });
            entry.class = it.class.clone();
            entry.last_verified_at = now;

            let order_for_kind = self.order.entry(it.kind.clone()).or_default();
            if !order_for_kind.iter().any(|s| s == &it.slug) {
                // Insert in alphabetical order so the row is stable.
                let pos = order_for_kind.binary_search(&it.slug).unwrap_or_else(|p| p);
                order_for_kind.insert(pos, it.slug.clone());
            }
        }

        // Eviction sweep: entries last seen ≥ 2 cycles ago are gone.
        let cutoff = self.cycle.saturating_sub(2);
        let stale: Vec<(String, String)> = self.last_seen_cycle.iter()
            .filter(|&(_, &cyc)| cyc <= cutoff)
            .map(|(k, _)| k.clone())
            .collect();
        for key in stale {
            self.entries.remove(&key);
            self.last_seen_cycle.remove(&key);
            if let Some(order) = self.order.get_mut(&key.0) {
                order.retain(|s| s != &key.1);
            }
        }

        // Drop expired banners (≥ 5 s old).
        while let Some(front) = self.banners.front() {
            if now.saturating_duration_since(front.posted_at) > Duration::from_secs(5) {
                self.banners.pop_front();
            } else {
                break;
            }
        }
    }

    /// Update the in-flight flag for one (kind, slug). Cleared on
    /// resource_finished or on the next ingest.
    pub fn mark_in_flight(&mut self, kind: &str, slug: &str, op: Option<crate::progress::ResourceOp>) {
        if let Some(e) = self.entries.get_mut(&(kind.to_string(), slug.to_string())) {
            e.in_flight = op;
        }
    }
}

#[cfg(test)]
mod grid_state_tests {
    use super::*;
    use crate::cli::sync::classify::ClassifiedItem;

    fn item(kind: &str, slug: &str, class: SyncClass) -> ClassifiedItem {
        ClassifiedItem {
            kind: kind.to_string(),
            slug: slug.to_string(),
            class,
            local_hash: None,
            remote_hash: None,
            base_hash: None,
        }
    }

    #[test]
    fn first_ingest_creates_entries_with_now_as_clock() {
        let mut g = GridState::new("test".into(), false);
        let now = Instant::now();
        g.ingest(&[item("labels", "audit-hold", SyncClass::Clean)], now);
        let e = &g.entries[&("labels".into(), "audit-hold".into())];
        assert_eq!(e.last_verified_at, now);
        assert!(matches!(e.class, SyncClass::Clean));
    }

    #[test]
    fn second_ingest_updates_class_and_advances_clock() {
        let mut g = GridState::new("test".into(), false);
        let t0 = Instant::now();
        g.ingest(&[item("labels", "audit-hold", SyncClass::Clean)], t0);
        let t1 = t0 + Duration::from_secs(60);
        g.ingest(&[item("labels", "audit-hold", SyncClass::LocalEdit)], t1);
        let e = &g.entries[&("labels".into(), "audit-hold".into())];
        assert_eq!(e.last_verified_at, t1);
        assert!(matches!(e.class, SyncClass::LocalEdit));
    }

    #[test]
    fn both_deleted_evicts_entry() {
        let mut g = GridState::new("test".into(), false);
        let t0 = Instant::now();
        g.ingest(&[item("labels", "gone", SyncClass::Clean)], t0);
        g.ingest(&[item("labels", "gone", SyncClass::BothDeleted)], t0);
        assert!(!g.entries.contains_key(&("labels".into(), "gone".into())));
    }

    #[test]
    fn two_cycle_no_show_evicts_entry() {
        let mut g = GridState::new("test".into(), false);
        let t0 = Instant::now();
        g.ingest(&[item("labels", "ephemeral", SyncClass::Clean)], t0);
        g.ingest(&[], t0 + Duration::from_secs(60));
        // After one no-show, entry is still present.
        assert!(g.entries.contains_key(&("labels".into(), "ephemeral".into())));
        g.ingest(&[], t0 + Duration::from_secs(120));
        // After two no-shows, entry is evicted.
        assert!(!g.entries.contains_key(&("labels".into(), "ephemeral".into())));
    }

    #[test]
    fn slug_order_is_alphabetical_and_stable() {
        let mut g = GridState::new("test".into(), false);
        let t0 = Instant::now();
        g.ingest(&[
            item("labels", "zebra", SyncClass::Clean),
            item("labels", "alpha", SyncClass::Clean),
            item("labels", "mike",  SyncClass::Clean),
        ], t0);
        assert_eq!(g.order["labels"], vec!["alpha", "mike", "zebra"]);
        // A second ingest with the same set must not reshuffle.
        g.ingest(&[
            item("labels", "mike",  SyncClass::Clean),
            item("labels", "zebra", SyncClass::Clean),
            item("labels", "alpha", SyncClass::Clean),
        ], t0);
        assert_eq!(g.order["labels"], vec!["alpha", "mike", "zebra"]);
    }

    #[test]
    fn new_slug_inserts_into_existing_order_at_alphabetical_position() {
        let mut g = GridState::new("test".into(), false);
        let t0 = Instant::now();
        g.ingest(&[
            item("labels", "alpha", SyncClass::Clean),
            item("labels", "zebra", SyncClass::Clean),
        ], t0);
        g.ingest(&[
            item("labels", "alpha", SyncClass::Clean),
            item("labels", "mike",  SyncClass::Clean),
            item("labels", "zebra", SyncClass::Clean),
        ], t0);
        assert_eq!(g.order["labels"], vec!["alpha", "mike", "zebra"]);
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
