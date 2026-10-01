//! Proof that requests fan out, whatever the machine's load.
//!
//! A wall-clock bound on a whole operation flakes on a busy CI runner: four
//! push PATCHes that did overlap once took 1.7s against a 650ms bound.
//! Arrival times do not flake that way. A request sent only after the
//! previous one was answered arrives at least [`DELAY`] later, so a round of
//! requests whose arrivals spread less than [`DELAY`] was in flight at once.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long each stamped response takes. Wider than any fan-out jitter.
pub(crate) const DELAY: Duration = Duration::from_millis(500);

pub(crate) type Arrivals = Arc<Mutex<Vec<Instant>>>;

/// Answers 200 with `body` after [`DELAY`], noting when each request arrived.
pub(crate) struct Stamped {
    body: serde_json::Value,
    arrivals: Arrivals,
}

impl Stamped {
    pub(crate) fn new(body: serde_json::Value, arrivals: &Arrivals) -> Self {
        Self { body, arrivals: arrivals.clone() }
    }
}

impl wiremock::Respond for Stamped {
    fn respond(&self, _: &wiremock::Request) -> wiremock::ResponseTemplate {
        self.arrivals.lock().unwrap().push(Instant::now());
        wiremock::ResponseTemplate::new(200).set_body_json(self.body.clone()).set_delay(DELAY)
    }
}

/// The stamped requests came in rounds of `width`, each round in flight at
/// once. A round is the `width` next arrivals in time order: with four items
/// that each GET then PATCH, the four GETs, then the four PATCHes.
pub(crate) fn assert_overlapped(arrivals: &Arrivals, width: usize) {
    let mut stamps = arrivals.lock().unwrap().clone();
    stamps.sort();
    assert!(
        !stamps.is_empty() && stamps.len().is_multiple_of(width),
        "expected whole rounds of {width} stamped requests, got {}",
        stamps.len(),
    );
    for (n, round) in stamps.chunks(width).enumerate() {
        let spread = round[width - 1] - round[0];
        assert!(
            spread < DELAY,
            "round {n} of {width} requests must overlap: its last arrived {spread:?} \
             after its first, but each takes {DELAY:?} to answer",
        );
    }
}
