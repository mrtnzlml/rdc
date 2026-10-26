//! Async-runtime shims. Everything that differs between the native build
//! (tokio) and a browser build goes through here, so the rest of the crate
//! names one spelling.

use std::future::Future;
use std::time::Duration;

/// A boxed future. `Send` natively; the wasm build swaps in the local
/// (non-`Send`) form because reqwest futures are `!Send` there.
#[cfg(not(target_arch = "wasm32"))]
pub type BoxFuture<'a, T> = futures::future::BoxFuture<'a, T>;
#[cfg(target_arch = "wasm32")]
pub type BoxFuture<'a, T> = futures::future::LocalBoxFuture<'a, T>;

/// Run `f` detached on the runtime. Task-locals do not follow a spawn, so
/// the caller's current store and prompt route, when set, are re-scoped
/// around `f`.
#[cfg(not(target_arch = "wasm32"))]
pub fn spawn<F: Future<Output = ()> + Send + 'static>(f: F) {
    tokio::spawn(rescope(Box::pin(f)));
}

/// The browser's [`spawn`]: the same re-scoping, on the page's event loop.
#[cfg(target_arch = "wasm32")]
pub fn spawn<F: Future<Output = ()> + 'static>(f: F) {
    wasm_bindgen_futures::spawn_local(rescope(Box::pin(f)));
}

fn rescope(mut f: BoxFuture<'static, ()>) -> BoxFuture<'static, ()> {
    if let Some(route) = crate::cli::stdin_coord::current_route() {
        f = Box::pin(crate::cli::stdin_coord::with_route(route, f));
    }
    if let Some(store) = crate::vfs::current() {
        f = Box::pin(crate::vfs::scope(store, f));
    }
    f
}

/// Wait for `d`. Tokio's timer, so paused-time tests keep working.
#[cfg(not(target_arch = "wasm32"))]
pub async fn sleep(d: Duration) {
    tokio::time::sleep(d).await;
}

/// Wait for `d` on the browser's `setTimeout`.
#[cfg(target_arch = "wasm32")]
pub async fn sleep(d: Duration) {
    gloo_timers::future::sleep(d).await;
}

#[cfg(not(target_arch = "wasm32"))]
pub type Instant = tokio::time::Instant;
#[cfg(target_arch = "wasm32")]
pub type Instant = web_time::Instant;
