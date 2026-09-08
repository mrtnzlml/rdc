//! An offline, stateful stand-in for a Rossum organization.
//!
//! See `docs/superpowers/specs/2026-09-07-stateful-fake-org-convergence-design.md`.
//! The short version: the live scenario suite is already parameterized on
//! `(api_base, org_id, token)`, so a fake that speaks HTTP lets the same
//! scenario bodies run in a plain `cargo test` — which is the only way
//! "nothing should happen the second time" becomes an assertion rather than
//! something a human notices in a customer env.

pub mod kinds;
pub mod state;
