//! Background polling loop. Runs on the tokio runtime and reports to a sink.

use super::Query;
use super::manager::Manager;
use crate::config::{MIN_POLL_SECS, SourceChoice};
use crate::model::aircraft::Aircraft;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Notify, watch};

/// How often the wide preload area is re-fetched between visible-area polls.
pub const WIDE_EVERY: Duration = Duration::from_secs(30);
/// Visible radius (nm) at and above which the full poll interval is used.
const ADAPTIVE_FULL_NM: f64 = 100.0;

#[derive(Debug, Clone)]
pub struct PollParams {
    /// The visible area (plus margin): polled every `interval`.
    pub query: Query,
    /// Optional wider area, polled first and then every `WIDE_EVERY`.
    pub wide: Option<Query>,
    pub interval: Duration,
    pub source: SourceChoice,
    pub opensky: bool,
}

pub enum PollEvent {
    Update {
        aircraft: Vec<Aircraft>,
        source: &'static str,
        fallback: bool,
        /// The area this response covers (aircraft outside it are not "gone").
        area: Query,
    },
    Failure {
        message: String,
        retry_in: Duration,
    },
}

pub type Sink = Arc<dyn Fn(PollEvent) + Send + Sync>;

/// Poll interval for a view of the given visible radius: the configured
/// interval when zoomed out, proportionally shorter when zoomed in, never
/// below the global minimum.
pub fn adaptive_interval(base: Duration, visible_radius_nm: u32, adaptive: bool) -> Duration {
    let floor = Duration::from_secs(MIN_POLL_SECS);
    if !adaptive {
        return base.max(floor);
    }
    let f = (f64::from(visible_radius_nm) / ADAPTIVE_FULL_NM).clamp(0.0, 1.0);
    base.mul_f64(f).max(floor).min(base.max(floor))
}

/// Which area to poll now: the wide one when it is due, else the visible one.
pub fn pick_query(p: &PollParams, last_wide: Option<Instant>, now: Instant) -> (Query, bool) {
    match &p.wide {
        Some(w) if last_wide.is_none_or(|t| now.duration_since(t) >= WIDE_EVERY) => {
            (w.clone(), true)
        }
        _ => (p.query.clone(), false),
    }
}

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
        let mut last_wide: Option<Instant> = None;
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
            let now = Instant::now();
            last = Some(now);
            let (query, wide) = pick_query(&p, last_wide, now);
            if wide {
                last_wide = Some(now);
            }
            let wait = match manager.poll(&query).await {
                Ok(r) => {
                    sink(PollEvent::Update {
                        aircraft: r.aircraft,
                        source: r.source,
                        fallback: r.fallback,
                        area: query,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::projection::circle_query;

    #[test]
    fn interval_speeds_up_when_zoomed_in() {
        let base = Duration::from_secs(10);
        assert_eq!(adaptive_interval(base, 250, true), base);
        assert_eq!(adaptive_interval(base, 100, true), base);
        assert_eq!(adaptive_interval(base, 50, true), Duration::from_secs(5));
        assert_eq!(
            adaptive_interval(base, 3, true),
            Duration::from_secs(MIN_POLL_SECS)
        );
        assert_eq!(adaptive_interval(base, 3, false), base);
        // never below the global floor, even with a tiny configured interval
        assert_eq!(
            adaptive_interval(Duration::from_secs(1), 3, true),
            Duration::from_secs(MIN_POLL_SECS)
        );
    }

    #[test]
    fn wide_area_first_then_every_30s() {
        let p = PollParams {
            query: circle_query(33.0, -112.0, 15),
            wide: Some(circle_query(33.0, -112.0, 150)),
            interval: Duration::from_secs(2),
            source: SourceChoice::Auto,
            opensky: false,
        };
        let t0 = Instant::now();
        let (q, wide) = pick_query(&p, None, t0);
        assert!(wide && q.radius_nm == 150, "first poll loads the wide area");
        let (q, wide) = pick_query(&p, Some(t0), t0 + Duration::from_secs(5));
        assert!(!wide && q.radius_nm == 15);
        let (_, wide) = pick_query(&p, Some(t0), t0 + WIDE_EVERY);
        assert!(wide);
        let narrow_only = PollParams { wide: None, ..p };
        assert!(!pick_query(&narrow_only, None, t0).1);
    }
}
