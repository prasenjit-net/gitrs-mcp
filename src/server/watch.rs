//! Change notifications (`--watch`): clients subscribe to `gitrs://` resources and get
//! `notifications/resources/updated` when one changes.
//!
//! Both MCP styles are served: the legacy `resources/subscribe` / `unsubscribe` requests, and the
//! `subscriptions/listen` stream of protocol 2026-07-28. One polling task checks the fingerprint of
//! every subscribed URI (`tools::resources::fingerprint`) and broadcasts the URIs that changed.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

use rmcp::{
    ErrorData as McpError, Peer, RoleServer,
    model::{ResourceUpdatedNotificationParam, SubscriptionFilter},
    service::SubscriptionContext,
};
use tokio::sync::broadcast;

use super::GitServer;
use super::repos::{Repos, split_repo};
use crate::tools::resources;

/// Subscribed URIs and the state of the polling task, shared by every clone of the server.
pub(super) struct Watcher {
    repos: Arc<Repos>,
    interval: Duration,
    /// Subscribed URIs with how many subscriptions name each, and the fingerprint last seen.
    watched: Mutex<HashMap<String, (usize, u64)>>,
    /// URIs subscribed with the legacy request, and the client to notify about them.
    legacy: Mutex<(HashSet<String>, Option<Peer<RoleServer>>)>,
    changes: broadcast::Sender<String>,
    started: Mutex<bool>,
}

/// Lock a mutex, recovering the data if a panicking thread poisoned it: the maps stay consistent
/// because every update is a single insert or remove.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Watcher {
    pub(super) fn new(repos: Arc<Repos>, interval: Duration) -> Arc<Self> {
        Arc::new(Self {
            repos,
            interval,
            watched: Mutex::default(),
            legacy: Mutex::default(),
            changes: broadcast::channel(256).0,
            started: Mutex::new(false),
        })
    }

    /// Start watching `uri` (once more), taking its current fingerprint as the baseline.
    async fn watch(self: &Arc<Self>, uri: &str) -> Result<(), McpError> {
        let (base, repo) = split_repo(uri).map_err(|e| McpError::invalid_params(e, None))?;
        resources::Address::parse(base)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        self.repos
            .select(repo.as_deref())
            .map_err(|e| McpError::invalid_params(e, None))?;
        let (repos, request) = (self.repos.clone(), uri.to_owned());
        let print = tokio::task::spawn_blocking(move || fingerprint(&repos, &request))
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        self.record(uri, print);
        Ok(())
    }

    /// Like [`Self::watch`], on the calling thread, for a URI already known to be valid.
    fn watch_now(self: &Arc<Self>, uri: &str) {
        self.record(uri, fingerprint(&self.repos, uri));
    }

    fn record(self: &Arc<Self>, uri: &str, print: u64) {
        lock(&self.watched)
            .entry(uri.to_owned())
            .and_modify(|(count, _)| *count += 1)
            .or_insert((1, print));
        self.start();
    }

    fn unwatch(&self, uri: &str) {
        let mut watched = lock(&self.watched);
        if let Some((count, _)) = watched.get_mut(uri) {
            *count -= 1;
            if *count == 0 {
                watched.remove(uri);
            }
        }
    }

    /// Start the polling task the first time something is watched. It holds only a weak reference,
    /// so it ends once the server is gone.
    fn start(self: &Arc<Self>) {
        let mut started = lock(&self.started);
        if *started {
            return;
        }
        *started = true;
        let weak = Arc::downgrade(self);
        tokio::spawn(poll(weak, self.interval));
    }

    /// Fingerprint every watched URI and broadcast the ones that changed since the last poll.
    async fn poll_once(&self) {
        let uris: Vec<String> = lock(&self.watched).keys().cloned().collect();
        if uris.is_empty() {
            return;
        }
        let repos = self.repos.clone();
        let Ok(prints) = tokio::task::spawn_blocking(move || {
            uris.into_iter()
                .map(|uri| {
                    let print = fingerprint(&repos, &uri);
                    (uri, print)
                })
                .collect::<Vec<_>>()
        })
        .await
        else {
            return;
        };
        let mut changed = Vec::new();
        {
            let mut watched = lock(&self.watched);
            for (uri, print) in prints {
                // Unsubscribed while this poll ran: nothing to report.
                let Some((_, last)) = watched.get_mut(&uri) else {
                    continue;
                };
                if *last != print {
                    *last = print;
                    changed.push(uri);
                }
            }
        }
        for uri in changed {
            self.notify_legacy(&uri).await;
            // No receivers just means no `subscriptions/listen` stream is open.
            let _ = self.changes.send(uri);
        }
    }

    async fn notify_legacy(&self, uri: &str) {
        let peer = {
            let legacy = lock(&self.legacy);
            if !legacy.0.contains(uri) {
                return;
            }
            legacy.1.clone()
        };
        if let Some(peer) = peer {
            let note = ResourceUpdatedNotificationParam::new(uri);
            if let Err(err) = peer.notify_resource_updated(note).await {
                tracing::debug!(%uri, %err, "could not send a resource update");
            }
        }
    }
}

/// The fingerprint of `uri` in the repository its `?repo=` names. A repository that stopped being
/// served (a client root removed) fingerprints its error, so that counts as a change.
fn fingerprint(repos: &Repos, uri: &str) -> u64 {
    let located =
        split_repo(uri).and_then(|(base, repo)| Ok((base, repos.select(repo.as_deref())?)));
    match located {
        Ok((base, handle)) => handle.run_now(|r| resources::fingerprint(r, base)),
        Err(err) => {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::hash::DefaultHasher::new();
            err.hash(&mut hasher);
            hasher.finish()
        }
    }
}

async fn poll(watcher: Weak<Watcher>, interval: Duration) {
    let mut ticks = tokio::time::interval(interval);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticks.tick().await;
        let Some(watcher) = watcher.upgrade() else {
            return;
        };
        watcher.poll_once().await;
    }
}

impl GitServer {
    fn watcher(&self) -> Result<&Arc<Watcher>, McpError> {
        self.watcher.as_ref().ok_or_else(|| {
            McpError::invalid_request(
                "resource subscriptions need the server to run with --watch",
                None,
            )
        })
    }

    /// `resources/subscribe`: notify the subscribing client when `uri` changes.
    pub(super) async fn subscribe_legacy(
        &self,
        uri: String,
        peer: Peer<RoleServer>,
    ) -> Result<(), McpError> {
        let watcher = self.watcher()?;
        let mut first = false;
        {
            let mut legacy = lock(&watcher.legacy);
            legacy.1 = Some(peer);
            if legacy.0.insert(uri.clone()) {
                first = true;
            }
        }
        // A repeated subscribe is one subscription, as the specification asks.
        if first && let Err(err) = watcher.watch(&uri).await {
            lock(&watcher.legacy).0.remove(&uri);
            return Err(err);
        }
        Ok(())
    }

    /// `resources/unsubscribe`. Unsubscribing from something never subscribed is not an error.
    pub(super) fn unsubscribe_legacy(&self, uri: &str) -> Result<(), McpError> {
        let watcher = self.watcher()?;
        if lock(&watcher.legacy).0.remove(uri) {
            watcher.unwatch(uri);
        }
        Ok(())
    }

    /// The part of a `subscriptions/listen` filter we serve: resource URIs that parse. `None`
    /// without `--watch`, which leaves listening unimplemented.
    ///
    /// The SDK acknowledges the subscription before [`Self::listen_stream`] runs, so the URIs are
    /// watched here, where a change right after the acknowledgment still counts. The stream stops
    /// watching them when it ends.
    pub(super) fn listen_filter(
        &self,
        requested: &SubscriptionFilter,
    ) -> Option<SubscriptionFilter> {
        let watcher = self.watcher.as_ref()?;
        let uris: Vec<String> = requested
            .resource_subscriptions
            .iter()
            .flatten()
            .filter(|uri| {
                split_repo(uri).is_ok_and(|(base, repo)| {
                    resources::Address::parse(base).is_ok()
                        && self.repos.select(repo.as_deref()).is_ok()
                })
            })
            .cloned()
            .collect();
        for uri in &uris {
            watcher.watch_now(uri);
        }
        Some(
            SubscriptionFilter::builder()
                .resource_subscriptions(uris)
                .build(),
        )
    }

    /// Serve one `subscriptions/listen` stream until the client cancels it.
    pub(super) async fn listen_stream(&self, context: SubscriptionContext) -> Result<(), McpError> {
        let watcher = self.watcher()?.clone();
        let uris: HashSet<String> = context
            .accepted()
            .resource_subscriptions
            .iter()
            .flatten()
            .cloned()
            .collect();
        let mut changes = watcher.changes.subscribe();
        loop {
            tokio::select! {
                () = context.cancelled() => break,
                change = changes.recv() => match change {
                    Ok(uri) if uris.contains(&uri) => {
                        if context.sink().notify_resource_updated(uri).await.is_err() {
                            break;
                        }
                    }
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                },
            }
        }
        for uri in &uris {
            watcher.unwatch(uri);
        }
        Ok(())
    }
}
