//! The application's Tokio runtime.
//!
//! `gpui_tokio` binds Tokio to Zed's GPUI crate, which is not the GPUI this app is
//! built on, so the binding is the app's own: one runtime, held in a global so it
//! outlives every window, and a handle read back from there. Nothing about the two
//! runtimes has to be shared — a tokio task hands its result back to GPUI through
//! `background_spawn`, which is an ordinary GPUI task.

use std::sync::Arc;

use gpui_kit::{App, Global};
use tokio::runtime::{Handle, Runtime};

/// Worker threads for the runtime.
///
/// The app runs cluster polling, the update manifest fetch, and pod exec streams.
/// One thread per CPU would make the bench harness's idle app look busy, and one
/// thread serializes the exec pumps behind each other.
const WORKER_THREADS: usize = 2;

struct AppRuntime(Arc<Runtime>);

impl Global for AppRuntime {}

/// Installs the runtime. Call once, before any window opens.
pub fn install(cx: &mut App) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(WORKER_THREADS)
        .enable_all()
        .build()
        .expect("the Tokio runtime starts");
    cx.set_global(AppRuntime(Arc::new(runtime)));
}

/// The runtime's handle, for the work that has to run off the UI thread.
///
/// # Panics
///
/// Panics when called before [`install`]. Every caller is startup or a spawned
/// task, both of which run after `install`, and a silent fallback to the current
/// thread would turn a startup ordering mistake into work that quietly never
/// completes.
pub fn handle(cx: &App) -> Handle {
    let Some(runtime) = cx.try_global::<AppRuntime>() else {
        panic!("the Tokio runtime is installed before anything needs a handle");
    };
    runtime.0.handle().clone()
}
