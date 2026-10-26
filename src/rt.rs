//! Async-runtime shims. Everything that differs between the native build
//! (tokio) and a browser build goes through here, so the rest of the crate
//! names one spelling.

use std::future::Future;
use std::time::Duration;

/// A boxed future. `Send` natively; the wasm build swaps in the local
/// (non-`Send`) form because reqwest futures are `!Send` there.
pub type BoxFuture<'a, T> = futures::future::BoxFuture<'a, T>;

/// Run `f` detached on the runtime.
pub fn spawn<F: Future<Output = ()> + Send + 'static>(f: F) {
    tokio::spawn(f);
}

/// Wait for `d`. Tokio's timer, so paused-time tests keep working.
pub async fn sleep(d: Duration) {
    tokio::time::sleep(d).await;
}

pub type Instant = tokio::time::Instant;
