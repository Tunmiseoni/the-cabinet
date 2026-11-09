use std::future::Future;
use std::ops::ControlFlow;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Default)]
pub struct TaskTree {
    token: CancellationToken,
}

impl TaskTree {
    pub fn spawn<F>(&self, task: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let token = self.token.clone();
        tauri::async_runtime::spawn(async move {
            token.run_until_cancelled(task).await;
        });
    }

    pub fn cancel(&self) {
        self.token.cancel();
    }
}

pub async fn blocking<T, F>(work: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    match tauri::async_runtime::spawn_blocking(work).await {
        Ok(value) => Some(value),
        Err(err) => {
            log::warn!("blocking task failed: {err}");
            None
        }
    }
}

pub async fn poll_blocking<S>(interval: Duration, mut state: S, tick: fn(&mut S) -> ControlFlow<()>)
where
    S: Send + 'static,
{
    loop {
        tokio::time::sleep(interval).await;
        let Some((next, flow)) = blocking(move || {
            let flow = tick(&mut state);
            (state, flow)
        })
        .await
        else {
            break;
        };
        state = next;
        if flow.is_break() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use std::time::Instant;

    fn eventually(timeout: Duration, check: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if check() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        check()
    }

    #[test]
    fn poll_blocking_ticks_until_the_tick_breaks() {
        let ticks = Arc::new(AtomicU32::new(0));
        let tree = TaskTree::default();
        tree.spawn(poll_blocking(
            Duration::from_millis(5),
            Arc::clone(&ticks),
            |ticks| {
                if ticks.fetch_add(1, Ordering::SeqCst) + 1 >= 3 {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            },
        ));
        let reached = eventually(Duration::from_secs(5), || ticks.load(Ordering::SeqCst) == 3);
        assert!(reached);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(ticks.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn cancelling_the_tree_stops_its_tasks() {
        let ticks = Arc::new(AtomicU32::new(0));
        let tree = TaskTree::default();
        tree.spawn(poll_blocking(
            Duration::from_millis(5),
            Arc::clone(&ticks),
            |ticks| {
                ticks.fetch_add(1, Ordering::SeqCst);
                ControlFlow::Continue(())
            },
        ));
        let started = eventually(Duration::from_secs(5), || ticks.load(Ordering::SeqCst) > 0);
        assert!(started);
        tree.cancel();
        std::thread::sleep(Duration::from_millis(50));
        let after_cancel = ticks.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(ticks.load(Ordering::SeqCst), after_cancel);
    }

    #[test]
    fn a_task_spawned_on_a_cancelled_tree_never_runs() {
        let ran = Arc::new(AtomicU32::new(0));
        let tree = TaskTree::default();
        tree.cancel();
        let flag = Arc::clone(&ran);
        tree.spawn(async move {
            flag.fetch_add(1, Ordering::SeqCst);
        });
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(ran.load(Ordering::SeqCst), 0);
    }
}
