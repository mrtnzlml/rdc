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

/// Detected terminal color depth. Drives [`emit_square`] in
/// [`Self::TrueColor`] / [`Self::Color256`] / [`Self::Color16`] /
/// [`Self::None`] modes per spec section 5.5.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorDepth {
    TrueColor,
    Color256,
    Color16,
    None,
}

pub fn detect_color_depth() -> ColorDepth {
    if std::env::var("NO_COLOR").is_ok() {
        return ColorDepth::None;
    }
    match std::env::var("COLORTERM").as_deref() {
        Ok("truecolor") | Ok("24bit") => ColorDepth::TrueColor,
        _ => match std::env::var("TERM").as_deref() {
            Ok(t) if t.contains("256color") => ColorDepth::Color256,
            _ => ColorDepth::Color16,
        },
    }
}

/// Build the ANSI escape sequence for one square. Output is always two
/// cells wide (the square) plus one gap cell. The caller appends this
/// directly to the row string fed to `MultiProgress::set_message`.
///
/// `dim` reduces brightness by 30% — used by the in-flight pulse.
pub fn emit_square(color: Color, depth: ColorDepth, dim: bool) -> String {
    let (r, g, b) = match color {
        Color::FreshGreen       => (0x1f, 0x6e, 0x3e),
        Color::Green            => (0x2a, 0x8a, 0x4b),
        Color::Yellow           => (0xc7, 0x9a, 0x2b),
        Color::Orange           => (0xd8, 0x61, 0x2e),
        Color::StaleRed         => (0xa5, 0x2a, 0x2a),
        Color::PendingOrange    => (0xe8, 0x96, 0x22),
        Color::EditRed          => (0xff, 0x3b, 0x30),
        Color::ConflictOutlined => (0xc9, 0x30, 0x30),
    };
    let (r, g, b) = if dim {
        ((r as f32 * 0.7) as u8, (g as f32 * 0.7) as u8, (b as f32 * 0.7) as u8)
    } else {
        (r, g, b)
    };

    let outline = matches!(color, Color::ConflictOutlined);

    match depth {
        ColorDepth::TrueColor => {
            if outline {
                format!("\x1b[48;2;{r};{g};{b}m\x1b[38;2;255;209;102m▏▕\x1b[0m ")
            } else {
                format!("\x1b[48;2;{r};{g};{b}m  \x1b[0m ")
            }
        }
        ColorDepth::Color256 => {
            // Approximate via the 6x6x6 color cube. Index = 16 + 36r' + 6g' + b'
            // where r' = round(r/255 * 5).
            let idx = 16u16
                + 36 * (r as u16 * 5 / 255)
                + 6  * (g as u16 * 5 / 255)
                +      (b as u16 * 5 / 255);
            if outline {
                format!("\x1b[48;5;{idx}m\x1b[38;5;221m▏▕\x1b[0m ")
            } else {
                format!("\x1b[48;5;{idx}m  \x1b[0m ")
            }
        }
        ColorDepth::Color16 => {
            // Collapse to 3 buckets: green / yellow / red.
            let ansi_bg = match color {
                Color::FreshGreen | Color::Green => 42, // green bg
                Color::Yellow | Color::Orange | Color::PendingOrange => 43, // yellow bg
                Color::StaleRed | Color::EditRed | Color::ConflictOutlined => 41, // red bg
            };
            format!("\x1b[{ansi_bg}m  \x1b[0m ")
        }
        ColorDepth::None => {
            // ASCII fallback: "· " (clean band) / "o " (aging) / "x " (stamp/conflict).
            match color {
                Color::FreshGreen | Color::Green => "·  ".to_string(),
                Color::Yellow | Color::Orange | Color::PendingOrange => "o  ".to_string(),
                Color::StaleRed | Color::EditRed | Color::ConflictOutlined => "x  ".to_string(),
            }
        }
    }
}

use std::sync::{Arc, Mutex};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle, ProgressDrawTarget};
use crate::progress::{ResourceOp, ResourceOutcome, Severity, SyncRenderer};

/// Maximum kinds we pre-allocate row slots for. The classifier emits 11
/// SyncClass kinds (`workspaces`, `queues`, `schemas`, `inboxes`,
/// `email_templates`, `hooks`, `rules`, `labels`, `engines`,
/// `engine_fields`, `mdh`) plus 3 read-only (`organization`,
/// `workflows`, `workflow_steps`). 16 is conservative.
const MAX_KINDS: usize = 16;
/// Max footer entries shown before "+ N more".
const MAX_FOOTER: usize = 12;
const MAX_BANNERS: usize = 2;
/// Max continuation rows per kind. With ~500 squares in a 60-col-wide
/// budget, one kind needs ≤ 30 rows worst case; 32 is safe headroom.
const MAX_CONT_ROWS: usize = 32;

pub struct GridRenderer {
    inner: Mutex<GridInner>,
}

struct GridInner {
    state: GridState,
    color_depth: ColorDepth,
    mp: MultiProgress,
    /// Stable bar handles, allocated at construction:
    header: ProgressBar,
    /// Per-kind row groups. Up to MAX_KINDS, each with up to
    /// MAX_CONT_ROWS continuation bars (used when the row wraps).
    kind_rows: Vec<Vec<ProgressBar>>,
    separator: ProgressBar,
    banner_slots: Vec<ProgressBar>,
    footer_header: ProgressBar,
    footer_slots: Vec<ProgressBar>,
    footer_more: ProgressBar,
    /// Mapping kind → row group index. Populated lazily on first
    /// observation of a kind.
    kind_index: BTreeMap<String, usize>,
    next_kind_slot: usize,
    finished: bool,
}

impl GridRenderer {
    pub fn new(env: String, is_watch: bool) -> Arc<Self> {
        let mp = MultiProgress::with_draw_target(ProgressDrawTarget::stderr_with_hz(8));
        let style_plain = ProgressStyle::with_template("{msg}").unwrap();
        let style_spinner = ProgressStyle::with_template("{spinner} {msg}")
            .unwrap()
            .tick_strings(&["|", "/", "-", "\\"]);

        let mk_plain = |msg: &str| {
            let bar = mp.add(ProgressBar::new(1));
            bar.set_style(style_plain.clone());
            bar.set_message(msg.to_string());
            bar
        };
        let header = mp.add(ProgressBar::new(1));
        header.set_style(style_spinner.clone());
        header.enable_steady_tick(Duration::from_millis(250));
        header.set_message(String::new());

        let mut kind_rows: Vec<Vec<ProgressBar>> = Vec::with_capacity(MAX_KINDS);
        for _ in 0..MAX_KINDS {
            let mut group = Vec::with_capacity(MAX_CONT_ROWS);
            for _ in 0..MAX_CONT_ROWS {
                group.push(mk_plain(""));
            }
            kind_rows.push(group);
        }

        let separator = mk_plain("");
        let banner_slots: Vec<_> = (0..MAX_BANNERS).map(|_| mk_plain("")).collect();
        let footer_header = mk_plain("");
        let footer_slots: Vec<_> = (0..MAX_FOOTER).map(|_| mk_plain("")).collect();
        let footer_more = mk_plain("");

        let color_depth = detect_color_depth();

        Arc::new(Self {
            inner: Mutex::new(GridInner {
                state: GridState::new(env, is_watch),
                color_depth,
                mp,
                header,
                kind_rows,
                separator,
                banner_slots,
                footer_header,
                footer_slots,
                footer_more,
                kind_index: BTreeMap::new(),
                next_kind_slot: 0,
                finished: false,
            }),
        })
    }
}

impl SyncRenderer for GridRenderer {
    fn phase(&self, label: &str) {
        let mut g = self.inner.lock().unwrap();
        g.state.current_op = label.to_string();
        // Task 8 adds repaint() here.
    }

    fn warn_line(&self, msg: &str) {
        // Wrap as a Warn banner so it lands in the banner queue.
        self.banner(Severity::Warn, msg);
    }

    fn resource_started(&self, kind: &str, slug: &str, op: ResourceOp) {
        let mut g = self.inner.lock().unwrap();
        g.state.mark_in_flight(kind, slug, Some(op));
    }

    fn resource_finished(&self, kind: &str, slug: &str, _outcome: ResourceOutcome) {
        let mut g = self.inner.lock().unwrap();
        g.state.mark_in_flight(kind, slug, None);
    }

    fn ingest_classification(&self, items: &[ClassifiedItem]) {
        let mut g = self.inner.lock().unwrap();
        g.state.ingest(items, Instant::now());
        // Task 8 adds repaint() here.
    }

    fn banner(&self, severity: Severity, msg: &str) {
        let mut g = self.inner.lock().unwrap();
        // Dedup within 10 s by exact text (spec 9.3).
        let now = Instant::now();
        // Look for an existing banner whose body matches (ignoring any
        // existing `(×N)` suffix) within the dedup window.
        let body = strip_count_suffix(msg);
        if let Some(b) = g.state.banners.iter_mut().find(|b| {
            strip_count_suffix(&b.text) == body
                && now.saturating_duration_since(b.posted_at) < Duration::from_secs(10)
        }) {
            // Append `(×N)` suffix; bump posted_at.
            let prev_count = match b.text.rfind(" (×") {
                Some(open) => b.text[open + " (×".len()..b.text.len() - 1].parse::<u32>().unwrap_or(1),
                None => 1,
            };
            b.text = format!("{} (×{})", body, prev_count + 1);
            b.posted_at = now;
            b.severity = severity;
            return;
        }
        g.state.banners.push_back(Banner {
            severity,
            text: msg.to_string(),
            posted_at: now,
        });
    }

    fn with_prompt(&self, f: &mut dyn FnMut() -> anyhow::Result<()>) -> anyhow::Result<()> {
        let mp_clone = {
            let g = self.inner.lock().unwrap();
            g.mp.clone()
        };
        mp_clone.set_draw_target(ProgressDrawTarget::hidden());
        let result = f();
        mp_clone.set_draw_target(ProgressDrawTarget::stderr_with_hz(8));
        result
    }

    fn finish_ok(&self, summary: &str) {
        let mut g = self.inner.lock().unwrap();
        if g.finished { return; }
        g.finished = true;
        // Task 12 fills in the final-frame commit. For now, just println the summary.
        let _ = g.mp.println(format!("DONE: {summary}"));
    }

    fn finish_err(&self, msg: &str) {
        let mut g = self.inner.lock().unwrap();
        if g.finished { return; }
        g.finished = true;
        let _ = g.mp.println(format!("FAIL: {msg}"));
    }
}

/// Strip any trailing ` (×N)` count suffix so banner dedup compares bodies.
fn strip_count_suffix(s: &str) -> &str {
    if let Some(open) = s.rfind(" (×") {
        if s.ends_with(')') {
            // Verify the inner is a number
            let inner = &s[open + " (×".len()..s.len() - 1];
            if inner.parse::<u32>().is_ok() {
                return &s[..open];
            }
        }
    }
    s
}

#[cfg(test)]
mod grid_renderer_skeleton_tests {
    use super::*;

    #[test]
    fn new_constructs_without_panicking() {
        let r = GridRenderer::new("test".into(), false);
        r.phase("listing remote");
        r.resource_started("hooks", "foo", ResourceOp::Get);
        r.resource_finished("hooks", "foo", ResourceOutcome::Ok);
        r.banner(Severity::Info, "ready");
        r.finish_ok("done");
    }

    #[test]
    fn banner_dedup_appends_count_suffix() {
        let r = GridRenderer::new("test".into(), false);
        r.banner(Severity::Warn, "auth expired");
        r.banner(Severity::Warn, "auth expired");
        r.banner(Severity::Warn, "auth expired");
        let g = r.inner.lock().unwrap();
        assert_eq!(g.state.banners.len(), 1);
        assert_eq!(g.state.banners[0].text, "auth expired (×3)");
    }

    #[test]
    fn strip_count_suffix_handles_clean_text() {
        assert_eq!(strip_count_suffix("auth expired"), "auth expired");
        assert_eq!(strip_count_suffix("auth expired (×2)"), "auth expired");
        assert_eq!(strip_count_suffix("auth expired (×42)"), "auth expired");
        // Not a count — leave alone
        assert_eq!(strip_count_suffix("auth expired (×abc)"), "auth expired (×abc)");
    }
}

#[cfg(test)]
mod emit_square_tests {
    use super::*;

    #[test]
    fn truecolor_edit_red_emits_known_escape() {
        let s = emit_square(Color::EditRed, ColorDepth::TrueColor, false);
        assert_eq!(s, "\x1b[48;2;255;59;48m  \x1b[0m ");
    }

    #[test]
    fn truecolor_conflict_emits_outline_glyphs() {
        let s = emit_square(Color::ConflictOutlined, ColorDepth::TrueColor, false);
        assert!(s.contains("▏▕"), "conflict outline missing: {s:?}");
    }

    #[test]
    fn dim_reduces_brightness() {
        let bright = emit_square(Color::FreshGreen, ColorDepth::TrueColor, false);
        let dim    = emit_square(Color::FreshGreen, ColorDepth::TrueColor, true);
        assert_ne!(bright, dim);
    }

    #[test]
    fn no_color_emits_ascii_only() {
        assert_eq!(emit_square(Color::FreshGreen, ColorDepth::None, false), "·  ");
        assert_eq!(emit_square(Color::EditRed, ColorDepth::None, false), "x  ");
        assert_eq!(emit_square(Color::PendingOrange, ColorDepth::None, false), "o  ");
    }

    #[test]
    fn detect_color_depth_respects_no_color() {
        // SAFETY: env-var mutation is process-global; ensure tests in
        // this file run single-threaded by gating on a serial lock if
        // needed. For now, save+restore the env var.
        let saved = std::env::var("NO_COLOR").ok();
        // SAFETY: see above
        unsafe { std::env::set_var("NO_COLOR", "1"); }
        assert_eq!(detect_color_depth(), ColorDepth::None);
        unsafe {
            match saved {
                Some(v) => std::env::set_var("NO_COLOR", v),
                None => std::env::remove_var("NO_COLOR"),
            };
        }
    }
}
