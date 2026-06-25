//! Live integration tests against a real Rossum test org.
//!
//! These are `#[ignore]` by default — `cargo test` never runs them. Opt in
//! with credentials in the environment:
//!
//! ```sh
//! export RDC_LIVE_API_BASE="https://<host>/v1"
//! export RDC_LIVE_ORG_ID="<org id>"
//! export RDC_LIVE_TOKEN="<token>"
//! cargo test --test live -- --ignored --test-threads=1
//! ```
//!
//! The harness's pure logic (config, run-id, manifest, ref resolution,
//! project fixture, expectations) is covered by fast hermetic unit tests in
//! the `support` modules, which DO run under a plain `cargo test`.

#[path = "live/support/mod.rs"]
mod support;
#[path = "live/scenarios/mod.rs"]
mod scenarios;
