use core_affinity::CoreId;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::runtime::Runtime;

/// Configuration for a Tokio multi-thread runtime.
///
/// `cpus` is a list of logical CPU indices to pin worker threads to (round-robin).
/// If `cpus` is empty, no affinity is set and the OS scheduler decides placement.
///
/// Environment variables (with Figment `__` splitting):
///   TPU_CLIENT_RT__NUM_THREADS=4
///   TPU_CLIENT_RT__CPUS=[0,1,2,3]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokioRtConfig {
    pub num_threads: usize,
    #[serde(default)]
    pub cpus: Vec<usize>,
}

/// Build a `tokio::runtime::Runtime` with optional CPU affinity.
///
/// Each worker thread is pinned to `cpus[thread_index % cpus.len()]` in order.
/// If `cpus` is empty the runtime is built without any affinity hooks.
pub fn build_runtime(name: &'static str, config: &TokioRtConfig) -> Runtime {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder
        .thread_name(name)
        .worker_threads(config.num_threads)
        .enable_all();

    if !config.cpus.is_empty() {
        let cpus = config.cpus.clone();
        let counter = Arc::new(AtomicUsize::new(0));
        builder.on_thread_start(move || {
            let idx = counter.fetch_add(1, Ordering::Relaxed);
            let cpu = cpus[idx % cpus.len()];
            core_affinity::set_for_current(CoreId { id: cpu });
        });
    }

    builder.build().expect("failed to build tokio runtime")
}
