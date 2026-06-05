//! Per-kind pull drivers, consumed by `cli::sync::execute`.
//!
//! The unified `rdc sync` command (with `--no-push`) is the user-facing
//! entry point for "pull from remote" workflows; the modules under this
//! namespace expose the per-kind `process` functions and the shared
//! catalog-listing helpers that sync stitches together.

pub(crate) mod common;
pub(crate) mod email_templates;
pub(crate) mod engine_fields;
pub(crate) mod engines;
pub(crate) mod hooks;
pub(crate) mod labels;
pub(crate) mod mdh;
pub(crate) mod organization;
pub mod portabilize;
pub(crate) mod queues;
pub(crate) mod rules;
pub(crate) mod workflow_steps;
pub(crate) mod workflows;
pub(crate) mod workspaces;

pub use common::PullCtx;
