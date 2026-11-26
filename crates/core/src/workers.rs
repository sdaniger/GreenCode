//! Worker threads for subagents (`task.workerThreads`): up to N subagents run on the caller's runtime;
//! past the main-thread limit, a small pool of lanes runs each future on its own thread and runtime, so
//! the main thread keeps taking input. A lane stops a minute after no subagent is live in it.

use std::future::Future;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, LazyLock, Mutex,
};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

/// How many lanes must be up for `live` subagents running at once.
pub fn lanes_for(max: u64, live: usize, main_limit: usize) -> usize {
    if max == 0 { return 0; }
    let max = max.min(64) as usize;
    live.saturating_sub(main_limit).min(max)
}

pub const MAIN_THREAD_LIMIT: usize = 8;
const IDLE: Duration = Duration::from_secs(60);

type Job = Box<dyn FnOnce(&tokio::runtime::Handle) + Send>;
type Lane = (mpsc::UnboundedSender<Job>, Arc<AtomicUsize>);

static LANES: LazyLock<Mutex<Vec<Lane>>> = LazyLock::new(|| Mutex::new(Vec::new()));

fn spawn_lane() -> Lane {
    let (tx, mut rx) = mpsc::unbounded_channel::<Job>();
    let live = Arc::new(AtomicUsize::new(0));
    let counter = live.clone();
    std::thread::Builder::new().name("greencode-subagent".into()).spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
        let handle = rt.handle().clone();
        rt.block_on(async move {
            loop {
                match tokio::time::timeout(IDLE, rx.recv()).await {
                    Ok(Some(job)) => job(&handle),
                    Ok(None) => break,
                    // A lane stops a minute after no subagent is live in it.
                    Err(_) if counter.load(Ordering::Relaxed) == 0 => break,
                    Err(_) => {}
                }
            }
        });
    }).ok();
    (tx, live)
}

/// Runs `fut` on a worker lane, or on the caller's runtime when the pool is off or a lane died.
/// Failure to reach a lane runs the work inline: a missing worker never fails a task.
pub fn run<F, T>(max: u64, live: usize, fut: F) -> oneshot::Receiver<T>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = oneshot::channel();
    let lanes = lanes_for(max, live, MAIN_THREAD_LIMIT);
    if lanes > 0 {
        let lane = {
            let mut senders = LANES.lock().unwrap();
            while senders.len() < lanes {
                let lane = spawn_lane();
                if lane.0.is_closed() { break; }
                senders.push(lane);
            }
            if senders.is_empty() { None } else { Some(senders[live % senders.len()].clone()) }
        };
        if let Some((sender, count)) = lane {
            let job: Job = Box::new(move |handle: &tokio::runtime::Handle| {
                count.fetch_add(1, Ordering::Relaxed);
                let count = count.clone();
                handle.spawn(async move {
                    let _ = tx.send(fut.await);
                    count.fetch_sub(1, Ordering::Relaxed);
                });
            });
            if let Err(error) = sender.send(job) {
                // A lane whose thread died restarts on the next spawn; this work runs inline instead.
                LANES.lock().unwrap().retain(|(sender, _)| !sender.is_closed());
                error.0(&tokio::runtime::Handle::current());
            }
            return rx;
        }
    }
    tokio::spawn(async move {
        let _ = tx.send(fut.await);
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lane_count_table() {
        assert_eq!(lanes_for(0, 100, MAIN_THREAD_LIMIT), 0, "0 keeps every subagent on the caller's thread");
        assert_eq!(lanes_for(4, 8, MAIN_THREAD_LIMIT), 0);
        assert_eq!(lanes_for(4, 9, MAIN_THREAD_LIMIT), 1);
        assert_eq!(lanes_for(4, 12, MAIN_THREAD_LIMIT), 4);
        assert_eq!(lanes_for(99, 200, MAIN_THREAD_LIMIT), 64);
    }

    #[test]
    fn work_runs_on_a_worker_lane() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let rx = run(2, 12, async { 7u32 * 2 });
        let value = rt.block_on(async { tokio::time::timeout(Duration::from_secs(10), rx).await.unwrap().unwrap() });
        assert_eq!(value, 14);
    }

    #[test]
    fn disabled_pool_runs_inline() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let value = rt.block_on(async { run(0, 40, async { "inline" }).await.unwrap() });
        assert_eq!(value, "inline");
    }
}
