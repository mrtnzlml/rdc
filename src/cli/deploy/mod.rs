//! Shared helpers retained after the `rdc deploy` command was replaced by
//! `rdc migrate` + `rdc sync`. These modules are pure filesystem / model
//! helpers reused by migrate, push, and doctor — the deploy orchestrator
//! and its remote create/apply machinery were removed.

pub mod create;
pub mod map;
pub mod realign;
pub(crate) mod selection;
pub mod store_extensions;
