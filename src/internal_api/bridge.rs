//! The one sync boundary into the async internal API. A future is spawned on the runtime and
//! the caller waits on a std channel (never `block_on`, which panics inside a runtime). It
//! blocks the caller like the blocking Web API calls do. Per caller kind:
//! - the cursive UI thread and plain `std::thread`s wait on the channel directly;
//! - `spawn_blocking` threads wait directly too (they are not runtime workers);
//! - async tasks on a multi-thread runtime worker (MPRIS handlers) wait inside
//!   `tokio::task::block_in_place`, which hands the worker's other tasks to another thread, so
//!   the spawned future still runs on a runtime with one worker;
//! - async tasks on a current-thread runtime wait directly (`block_in_place` would panic).

use std::future::Future;
use std::sync::mpsc;
use std::time::Duration;

use super::error::InternalError;
use crate::application::ASYNC_RUNTIME;

/// How long one internal call may take before it is aborted.
pub const BRIDGE_TIMEOUT: Duration = Duration::from_secs(10);

/// Run `fut` on ncspot's runtime and wait up to `timeout` for it.
pub fn run<T, Fut>(fut: Fut, timeout: Duration) -> Result<T, InternalError>
where
    Fut: Future<Output = Result<T, InternalError>> + Send + 'static,
    T: Send + 'static,
{
    let Some(runtime) = ASYNC_RUNTIME.get() else {
        return Err(InternalError::NoSession);
    };
    run_on(runtime.handle(), fut, timeout)
}

/// Run `fut` on `handle` and wait up to `timeout` for it. On timeout the task is aborted.
pub fn run_on<T, Fut>(
    handle: &tokio::runtime::Handle,
    fut: Fut,
    timeout: Duration,
) -> Result<T, InternalError>
where
    Fut: Future<Output = Result<T, InternalError>> + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    let task = handle.spawn(async move {
        // The receiver is gone after a timeout; nothing to do then.
        let _ = tx.send(fut.await);
    });
    let on_multi_thread_runtime = tokio::runtime::Handle::try_current()
        .is_ok_and(|h| h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread);
    let received = if on_multi_thread_runtime {
        // Outside a worker (spawn_blocking threads) block_in_place just runs the closure.
        tokio::task::block_in_place(|| rx.recv_timeout(timeout))
    } else {
        rx.recv_timeout(timeout)
    };
    match received {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            task.abort();
            Err(InternalError::Transient(format!(
                "timed out after {}s",
                timeout.as_secs_f32()
            )))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(InternalError::Transient(
            "task ended without a result".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn returns_the_value() {
        let rt = runtime();
        let out = run_on(
            rt.handle(),
            async { Ok::<_, InternalError>(7) },
            Duration::from_secs(1),
        );
        assert_eq!(out, Ok(7));
    }

    #[test]
    fn passes_errors_through() {
        let rt = runtime();
        let out = run_on(
            rt.handle(),
            async { Err::<u8, _>(InternalError::NotFound) },
            Duration::from_secs(1),
        );
        assert_eq!(out, Err(InternalError::NotFound));
    }

    #[test]
    fn times_out_and_aborts_the_task() {
        let rt = runtime();
        let finished = Arc::new(AtomicBool::new(false));
        let flag = finished.clone();
        let out = run_on(
            rt.handle(),
            async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                flag.store(true, Ordering::SeqCst);
                Ok::<_, InternalError>(())
            },
            Duration::from_millis(50),
        );
        assert!(matches!(out, Err(InternalError::Transient(_))));
        std::thread::sleep(Duration::from_millis(400));
        assert!(
            !finished.load(Ordering::SeqCst),
            "task kept running after the timeout"
        );
    }

    #[test]
    fn works_inside_an_async_task_on_a_one_worker_runtime() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let handle = rt.handle().clone();
        let out = rt.block_on(async move {
            tokio::spawn(async move {
                run_on(
                    &handle,
                    async { Ok::<_, InternalError>(5) },
                    Duration::from_secs(2),
                )
            })
            .await
            .unwrap()
        });
        assert_eq!(out, Ok(5));
    }

    #[test]
    fn works_inside_spawn_blocking_on_the_runtime() {
        let rt = runtime();
        let handle = rt.handle().clone();
        let out = rt.block_on(async move {
            tokio::task::spawn_blocking(move || {
                run_on(
                    &handle,
                    async { Ok::<_, InternalError>(3) },
                    Duration::from_secs(1),
                )
            })
            .await
            .unwrap()
        });
        assert_eq!(out, Ok(3));
    }
}
