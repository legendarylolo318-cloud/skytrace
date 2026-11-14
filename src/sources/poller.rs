//! Background polling loop. Runs on the tokio runtime and reports to a sink.

use super::Query;
use super::manager::Manager;
use crate::config::{MIN_POLL_SECS, SourceChoice};
use crate::model::aircraft::Aircraft;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Notify, watch};

#[derive(Debug, Clone)]
pub struct PollParams {
    pub query: Query,
    pub interval: Duration,
    pub source: SourceChoice,
    pub opensky: bool,
}

pub enum PollEvent {
    Update {
        aircraft: Vec<Aircraft>,
        source: &'static str,
        fallback: bool,
    },
    Failure {
        message: String,
        retry_in: Duration,
    },
}

pub type Sink = Arc<dyn Fn(PollEvent) + Send + Sync>;

pub fn spawn(
    rt: &tokio::runtime::Handle,
    client: reqwest::Client,
    mut params: watch::Receiver<PollParams>,
    refresh: Arc<Notify>,
    sink: Sink,
) {
    rt.spawn(async move {
        let floor = Duration::from_secs(MIN_POLL_SECS);
        let mut key = {
            let p = params.borrow();
            (p.source, p.opensky)
        };
        let mut manager = Manager::from_choice(&client, key.0, key.1);
        let mut last: Option<Instant> = None;
        loop {
            let p = params.borrow_and_update().clone();
            if (p.source, p.opensky) != key {
                key = (p.source, p.opensky);
                manager = Manager::from_choice(&client, key.0, key.1);
            }
            if let Some(l) = last {
                let el = l.elapsed();
                if el < floor {
                    tokio::time::sleep(floor - el).await;
                }
            }
            last = Some(Instant::now());
            let wait = match manager.poll(&p.query).await {
                Ok(r) => {
                    sink(PollEvent::Update {
                        aircraft: r.aircraft,
                        source: r.source,
                        fallback: r.fallback,
                    });
                    p.interval
                }
                Err(e) if e.throttled => e.retry_in.max(floor),
                Err(e) => {
                    let wait = e.retry_in.max(floor);
                    sink(PollEvent::Failure {
                        message: e.message,
                        retry_in: wait,
                    });
                    wait
                }
            };
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = refresh.notified() => {}
            }
        }
    });
}
