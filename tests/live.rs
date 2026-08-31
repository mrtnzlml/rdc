//! Live integration tests against a real Rossum test org.
//!
//! These are `#[ignore]` by default — `cargo test` never runs them. Opt in
//! with credentials in the environment:
//!
//! ```sh
//! export RDC_LIVE_API_BASE="https://<host>/v1"
//! export RDC_LIVE_ORG_ID="<org id>"
//! export RDC_LIVE_TOKEN="<token>"
//!
//! # Optional SECOND org, for the promotion scenarios (see below):
//! export RDC_LIVE_TGT_API_BASE="https://<host>/v1"
//! export RDC_LIVE_TGT_ORG_ID="<other org id>"
//! export RDC_LIVE_TGT_TOKEN="<other token>"
//!
//! cargo test --test live -- --ignored --test-threads=1
//! ```
//!
//! # The second org
//!
//! `deploy_flow` and `migrate_promotion` SKIP without `RDC_LIVE_TGT_*`, on
//! purpose. Pointing `test` and `prod` at one organization makes them two
//! views of the same objects, and promotion assertions then either go vacuous
//! or go wrong: the source env's objects show up in the target's whole-org
//! pull, so `migrate --mirror` correctly reads them as target-only extras and
//! a following `--allow-deletes` deletes the SOURCE env. A promotion is only
//! a promotion across orgs.
//!
//! # Reading a failure
//!
//! Every scenario ends its remote-writing phases with
//! `support::converge::assert_converged`, which asserts that a second cycle
//! plans nothing and rewrites nothing FOR THIS RUN'S OBJECTS. Its failures
//! name both the plan lines and the exact bytes that moved. Note the scoping:
//! the sandbox is a live org whose unrelated content changes under the suite,
//! so org-wide counters are meaningless and everything is filtered by the
//! run's `rdc-it-<id>-` prefix.
//!
//! Three scenarios contain a deliberate extra `sync` labelled KNOWN DEFECT,
//! each documenting a real one-cycle lag in a non-pull write path. They are
//! pinned rather than hidden: delete the extra cycle when the defect is fixed
//! and the convergence assertion after it should still pass.
//!
//! # Asserting request ORDER
//!
//! `live_push_create_ordering` runs `rdc` with `RDC_TRACE_HTTP` set and reads
//! the CSV back through `support::trace`, so it can assert that (say) every
//! `POST /engine_fields` precedes the first `POST /queues`. Use
//! `ProjectFixture::run_rdc_traced` rather than `run_rdc` when a scenario cares
//! about order. Comparisons are last-of-A against first-of-B, which is exact
//! because `push_classified` awaits its per-kind drivers sequentially.
//!
//! # The engine that gets left behind
//!
//! `live_push_create_ordering` binds a queue to the engine it creates, on
//! purpose: that binding is what makes the SERVER enforce rdc's push order, by
//! refusing `POST /queues` when the engine lacks a field covering an extracted
//! schema field. The price is that the engine and its field cannot be deleted
//! until the queue finishes purging — up to 24 hours, with no unbind escape
//! hatch — so each run of that scenario strands one of each. They carry the run
//! marker and `live_janitor_sweep` collects them later, which is why the
//! janitor asserts every other kind is empty but only REPORTS leftover engines.
//! `live_engines_round_trip` covers the same kinds without binding anything and
//! therefore cleans up completely.
//!
//! The harness's pure logic (config, run-id, manifest, ref resolution,
//! project fixture, expectations) is covered by fast hermetic unit tests in
//! the `support` modules, which DO run under a plain `cargo test`.

#[path = "live/support/mod.rs"]
mod support;
#[path = "live/scenarios/mod.rs"]
mod scenarios;
