//! The multi-threaded tokio runtime herdr runs its server and in-process app on.
//!
//! tokio's default is one worker thread per CPU. Herdr's async work is I/O and
//! channel traffic, not computation, so on a large host that default only adds
//! idle threads: a 316-CPU machine ran 316 workers for a few dozen panes, and a
//! wakeup could involve many of them. The worker count is therefore bounded by
//! default and can be set with `HERDR_RUNTIME_WORKER_THREADS`.

use std::io;
use std::num::NonZeroUsize;

use tracing::warn;

/// Most worker threads the runtime starts unless the environment says otherwise.
pub(crate) const DEFAULT_MAX_WORKER_THREADS: usize = 8;

/// Environment variable that sets the worker count exactly (a positive integer).
pub(crate) const WORKER_THREADS_ENV_VAR: &str = "HERDR_RUNTIME_WORKER_THREADS";

/// Worker threads for a host with `available` CPUs, given the environment
/// override, if any. An override that is not a positive integer is ignored
/// with a warning, so a typo cannot stop the server from starting.
pub(crate) fn worker_threads(available: NonZeroUsize, override_value: Option<&str>) -> usize {
    if let Some(value) = override_value {
        match value.trim().parse::<usize>() {
            Ok(count) if count > 0 => return count,
            _ => warn!(
                value,
                "ignoring {WORKER_THREADS_ENV_VAR}: expected a positive integer"
            ),
        }
    }
    available.get().min(DEFAULT_MAX_WORKER_THREADS)
}

/// Build the multi-threaded runtime with the bounded worker count.
pub(crate) fn build() -> io::Result<tokio::runtime::Runtime> {
    let available = std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN);
    let override_value = std::env::var(WORKER_THREADS_ENV_VAR).ok();
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads(available, override_value.as_deref()))
        .enable_all()
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cpus(count: usize) -> NonZeroUsize {
        NonZeroUsize::new(count).unwrap()
    }

    #[test]
    fn default_is_the_cpu_count_up_to_the_bound() {
        assert_eq!(worker_threads(cpus(1), None), 1);
        assert_eq!(worker_threads(cpus(4), None), 4);
        assert_eq!(worker_threads(cpus(8), None), 8);
        assert_eq!(worker_threads(cpus(316), None), DEFAULT_MAX_WORKER_THREADS);
    }

    #[test]
    fn a_positive_override_is_used_exactly() {
        assert_eq!(worker_threads(cpus(316), Some("32")), 32);
        assert_eq!(worker_threads(cpus(2), Some(" 16 ")), 16);
        assert_eq!(worker_threads(cpus(316), Some("1")), 1);
    }

    #[test]
    fn an_invalid_override_falls_back_to_the_default() {
        for value in ["", "0", "-3", "eight", "2.5"] {
            assert_eq!(worker_threads(cpus(316), Some(value)), 8, "{value:?}");
        }
    }
}
