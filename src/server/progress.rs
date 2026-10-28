//! Ties a tool call's MCP request to a `tools::progress::Watch`: cancellation of the request sets the
//! watch's flag, and its progress reports go to the client as `notifications/progress` when the
//! request asked for them with a progress token.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use rmcp::{RoleServer, model::ProgressNotificationParam, service::RequestContext};
use tokio::sync::{mpsc, oneshot};

use super::GitServer;
use crate::tools::progress::{Progress, Watch};

/// Least time between two progress notifications for one request.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

impl GitServer {
    /// Run `f` on the blocking pool with a watch for `context`'s request. Progress notifications
    /// are all sent before this returns, so they reach the client ahead of the result.
    pub(super) async fn run_watched<T, F>(
        &self,
        context: RequestContext<RoleServer>,
        f: F,
    ) -> Result<T, String>
    where
        F: FnOnce(&gix::Repository, &Watch) -> anyhow::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let cancelled = Arc::new(AtomicBool::new(false));
        let (finished, finish) = oneshot::channel::<()>();
        let flag = cancelled.clone();
        let request = context.ct.clone();
        tokio::spawn(async move {
            tokio::select! {
                () = request.cancelled() => flag.store(true, Ordering::Relaxed),
                _ = finish => {}
            }
        });

        let (reporter, forwarder) = match context.meta.get_progress_token() {
            Some(token) => {
                let (sender, mut reports) = mpsc::unbounded_channel::<Progress>();
                let peer = context.peer.clone();
                let forwarder = tokio::spawn(async move {
                    while let Some(progress) = reports.recv().await {
                        let mut note =
                            ProgressNotificationParam::new(token.clone(), progress.done as f64)
                                .with_message(progress.message);
                        if let Some(total) = progress.total {
                            note = note.with_total(total as f64);
                        }
                        // A client that went away just misses the update; the call carries on.
                        if peer.notify_progress(note).await.is_err() {
                            break;
                        }
                    }
                });
                let report = move |progress| {
                    let _ = sender.send(progress);
                };
                (Some(report), Some(forwarder))
            }
            None => (None, None),
        };

        let watch = Watch::new(cancelled, PROGRESS_INTERVAL, reporter);
        let result = self.repo.run(move |r| f(r, &watch)).await;
        drop(finished);
        // The watch (and with it the sender) is gone, so the forwarder ends once it has sent the rest.
        if let Some(forwarder) = forwarder {
            let _ = forwarder.await;
        }
        result
    }
}
