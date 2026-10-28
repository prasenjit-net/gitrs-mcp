//! Progress reports and cancellation for long-running tools, without any MCP types: the server turns
//! a request's progress token and cancellation into a [`Watch`], and tools check it as they walk.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::bail;

/// How far a tool has got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    /// Items handled so far (commits walked, files searched); only ever grows.
    pub done: u64,
    /// Items in all, when known up front.
    pub total: Option<u64>,
    pub message: String,
}

type Reporter = Box<dyn Fn(Progress) + Send + Sync>;

/// A cancellation flag and an optional progress callback for one tool call.
pub struct Watch {
    cancelled: Arc<AtomicBool>,
    report: Option<Reporter>,
    /// Least time between two reports, so a fast walk doesn't flood the client.
    interval: Duration,
}

impl Watch {
    /// Never cancelled, reports nowhere: for callers outside a request, such as other tools.
    pub fn none() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            report: None,
            interval: Duration::MAX,
        }
    }

    pub fn new(
        cancelled: Arc<AtomicBool>,
        interval: Duration,
        report: Option<impl Fn(Progress) + Send + Sync + 'static>,
    ) -> Self {
        Self {
            cancelled,
            report: report.map(|r| Box::new(r) as Reporter),
            interval,
        }
    }

    /// Fail once the client has cancelled the request.
    pub fn check(&self) -> anyhow::Result<()> {
        if self.cancelled.load(Ordering::Relaxed) {
            bail!("cancelled by the client");
        }
        Ok(())
    }

    pub fn report(&self, progress: Progress) {
        if let Some(report) = &self.report {
            report(progress);
        }
    }

    /// Pass `items` through, failing as soon as the request is cancelled and reporting how many
    /// have gone by (`what` names them, e.g. `commits walked`) at most once per interval.
    pub fn track<'a, T>(
        &'a self,
        items: impl Iterator<Item = anyhow::Result<T>> + 'a,
        total: Option<u64>,
        what: &'static str,
    ) -> impl Iterator<Item = anyhow::Result<T>> + 'a {
        let mut done = 0u64;
        let mut last = Instant::now();
        let mut stopped = false;
        items.map_while(move |item| {
            if stopped {
                return None;
            }
            if let Err(err) = self.check() {
                stopped = true;
                return Some(Err(err));
            }
            done += 1;
            if self.report.is_some() && last.elapsed() >= self.interval {
                last = Instant::now();
                let message = match total {
                    Some(total) => format!("{done} of {total} {what}"),
                    None => format!("{done} {what}"),
                };
                self.report(Progress {
                    done,
                    total,
                    message,
                });
            }
            Some(item)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::recording_watch as recording;

    #[test]
    fn tracking_reports_each_item_without_throttling() {
        let (watch, _, seen) = recording(Duration::ZERO);
        let items: Vec<_> = watch
            .track((1..=3).map(Ok), Some(3), "files searched")
            .collect::<anyhow::Result<_>>()
            .unwrap();
        assert_eq!(items, [1, 2, 3]);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert_eq!(
            seen[2],
            Progress {
                done: 3,
                total: Some(3),
                message: "3 of 3 files searched".into()
            }
        );
    }

    #[test]
    fn throttling_and_unknown_totals() {
        let (watch, _, seen) = recording(Duration::from_secs(3600));
        assert_eq!(watch.track((0..100).map(Ok), None, "commits").count(), 100);
        assert!(
            seen.lock().unwrap().is_empty(),
            "nothing within the interval"
        );

        let (watch, _, seen) = recording(Duration::ZERO);
        watch
            .track((0..2).map(Ok), None, "commits walked")
            .for_each(drop);
        assert_eq!(seen.lock().unwrap()[1].message, "2 commits walked");
    }

    #[test]
    fn cancellation_stops_the_walk_with_an_error() {
        let (watch, cancelled, _) = recording(Duration::ZERO);
        let mut walked = watch.track((0..10).map(Ok), None, "commits");
        assert_eq!(walked.next().unwrap().unwrap(), 0);
        cancelled.store(true, Ordering::Relaxed);
        let err = walked.next().unwrap().unwrap_err();
        assert!(err.to_string().contains("cancelled"));
        assert!(walked.next().is_none(), "nothing after the error");
        assert!(watch.check().is_err());
        assert!(Watch::none().check().is_ok());
        Watch::none().report(Progress {
            done: 1,
            total: None,
            message: String::new(),
        });
    }
}
