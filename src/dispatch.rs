use std::{
    error::Error,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
#[cfg(not(unix))]
use tokio::signal;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use twilight_gateway::{CloseFrame, Event, Shard, StreamExt as _};

use crate::{EVENT_TYPES, events::event_handler, hierarchy::build_chunk_request, store::registry};

pub const HANDLER_MAX_TASKS: usize = 128;
pub const RESTORE_MAX_TASKS: usize = 32;
const CHUNK_DRAIN_TICK_MS: u64 = 500;
const SHUTDOWN_DRAIN_SECS: u64 = 30;

static HANDLER_SEM: OnceLock<Arc<Semaphore>> = OnceLock::new();
static RESTORE_SEM: OnceLock<Arc<Semaphore>> = OnceLock::new();

fn handler_sem() -> Arc<Semaphore> {
    HANDLER_SEM
        .get_or_init(|| Arc::new(Semaphore::new(HANDLER_MAX_TASKS)))
        .clone()
}

fn restore_sem() -> Arc<Semaphore> {
    RESTORE_SEM
        .get_or_init(|| Arc::new(Semaphore::new(RESTORE_MAX_TASKS)))
        .clone()
}

pub async fn shutdown_signal() -> CancellationToken {
    let token = CancellationToken::new();
    let child = token.clone();
    #[cfg(unix)]
    tokio::spawn(async move {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigint = signal(SignalKind::interrupt()).expect("SIGINT handler");
        let mut sigterm = signal(SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = sigint.recv() => {},
            _ = sigterm.recv() => {},
        }
        child.cancel();
    });
    #[cfg(not(unix))]
    tokio::spawn(async move {
        signal::ctrl_c().await.expect("CTRL+C handler");
        child.cancel();
    });
    token
}

pub async fn run(mut shard: Shard, shutdown: CancellationToken) {
    let tracker = TaskTracker::new();
    let live: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
    let shard_id = shard.id();
    let sender = shard.sender();
    let drain_stop = shutdown.clone();
    let drain = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(CHUNK_DRAIN_TICK_MS));
        loop {
            tokio::select! {
                _ = drain_stop.cancelled() => break,
                _ = tick.tick() => {}
            }
            if !registry::has_pending_chunks().await {
                continue;
            }
            let due = registry::pending_chunks_due_for_shard(
                shard_id.number(),
                shard_id.total(),
                registry::now_ms(),
            )
            .await;
            for gid in due {
                match sender.command(&build_chunk_request(gid)) {
                    Ok(()) => registry::note_pending_chunk_sent(gid, registry::now_ms()).await,
                    Err(_) => tracing::warn!("chunk request send failed; retrying on next tick"),
                }
            }
        }
    });

    let mut shutting_down = false;
    loop {
        tokio::select! {
            _ = shutdown.cancelled(), if !shutting_down => {
                shard.close(CloseFrame::NORMAL);
                shutting_down = true;
            }
            event = shard.next_event(EVENT_TYPES) => {
                match event {
                    Some(Ok(Event::GatewayClose(_))) if shutting_down => break,
                    Some(Ok(event)) => {
                        let sem = if matches!(event, Event::MemberAdd(_)) {
                            restore_sem()
                        } else {
                            handler_sem()
                        };
                        let permit = sem.acquire_owned().await.expect("semaphore closed");
                        let fut = event_handler(event, ());
                        let live = Arc::clone(&live);
                        let handle = tracker.spawn(async move {
                            let _permit = permit;
                            fut.await;
                        });
                        if let Ok(mut guard) = live.lock() {
                            guard.retain(|h| !h.is_finished());
                            guard.push(handle);
                        }
                    }
                    Some(Err(error)) => tracing::warn!(error = &error as &dyn Error, "shard event error; skipped"),
                    None => break,
                }
            }
        }
    }

    drain.abort();
    tracker.close();
    if tokio::time::timeout(Duration::from_secs(SHUTDOWN_DRAIN_SECS), tracker.wait())
        .await
        .is_err()
    {
        tracing::warn!("shutdown drain timed out; aborting remaining handlers");
        if let Ok(mut guard) = live.lock() {
            for handle in guard.drain(..) {
                handle.abort();
            }
        }
        tracker.wait().await;
    }
}
