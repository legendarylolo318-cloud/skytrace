//! Ordered failover across sources with per-source exponential backoff.

use super::{AdsbxSource, FlightSource, Query, SourceError};
use crate::config::SourceChoice;
use crate::model::aircraft::Aircraft;
use std::time::{Duration, Instant};

const BACKOFF_BASE: Duration = Duration::from_secs(5);
const BACKOFF_MAX: Duration = Duration::from_secs(300);

/// Delay before retrying a source after `failures` consecutive failures.
pub fn backoff(failures: u32, retry_after: Option<Duration>) -> Duration {
    let exp = failures.saturating_sub(1).min(10);
    let d = BACKOFF_BASE.saturating_mul(1u32 << exp).min(BACKOFF_MAX);
    retry_after.map_or(d, |r| d.max(r.min(BACKOFF_MAX)))
}

struct Entry {
    source: Box<dyn FlightSource>,
    failures: u32,
    blocked_until: Option<Instant>,
    last_call: Option<Instant>,
}

#[derive(Debug)]
pub struct Polled {
    pub aircraft: Vec<Aircraft>,
    pub source: &'static str,
    /// True when a source other than the first choice answered.
    pub fallback: bool,
}

#[derive(Debug)]
pub struct PollError {
    pub message: String,
    pub retry_in: Duration,
    /// True when no request was made because every usable source is still inside
    /// its minimum request spacing (not a failure; just wait `retry_in`).
    pub throttled: bool,
}

/// Longest wait we accept inline to honour a source's minimum spacing; slower
/// sources (OpenSky) are skipped instead so they never stall the poll loop.
const MAX_INLINE_WAIT: Duration = Duration::from_millis(1500);

pub struct Manager {
    entries: Vec<Entry>,
}

impl Manager {
    pub fn new(sources: Vec<Box<dyn FlightSource>>) -> Self {
        Self {
            entries: sources
                .into_iter()
                .map(|source| Entry {
                    source,
                    failures: 0,
                    blocked_until: None,
                    last_call: None,
                })
                .collect(),
        }
    }

    /// Build the source list for a user choice. OpenSky is only included when enabled.
    pub fn from_choice(client: &reqwest::Client, choice: SourceChoice, opensky: bool) -> Self {
        use super::adsbx::Provider;
        use super::opensky::OpenSkySource;
        let lol = || {
            Box::new(AdsbxSource::new(client.clone(), Provider::AdsbLol)) as Box<dyn FlightSource>
        };
        let fi = || {
            Box::new(AdsbxSource::new(client.clone(), Provider::AdsbFi)) as Box<dyn FlightSource>
        };
        let osk = || Box::new(OpenSkySource::new(client.clone())) as Box<dyn FlightSource>;
        let mut v: Vec<Box<dyn FlightSource>> = match choice {
            SourceChoice::Auto | SourceChoice::AdsbLol => vec![lol(), fi()],
            SourceChoice::AdsbFi => vec![fi(), lol()],
            SourceChoice::OpenSky => vec![osk(), lol(), fi()],
        };
        if opensky && choice != SourceChoice::OpenSky {
            v.push(osk());
        }
        if !opensky && choice == SourceChoice::OpenSky {
            v.remove(0); // chosen but disabled in settings
        }
        Self::new(v)
    }

    #[cfg(test)]
    pub fn source_names(&self) -> Vec<&'static str> {
        self.entries.iter().map(|e| e.source.name()).collect()
    }

    pub async fn poll(&mut self, q: &Query) -> Result<Polled, PollError> {
        let mut last_err = None;
        let mut throttled_for: Option<Duration> = None;
        for (idx, e) in self.entries.iter_mut().enumerate() {
            if e.blocked_until.is_some_and(|t| t > Instant::now()) {
                continue;
            }
            if let Some(last) = e.last_call {
                let min = e.source.min_interval();
                let el = last.elapsed();
                if el < min {
                    let wait = min - el;
                    if wait > MAX_INLINE_WAIT {
                        throttled_for = Some(throttled_for.map_or(wait, |t| t.min(wait)));
                        continue;
                    }
                    tokio::time::sleep(wait).await;
                }
            }
            e.last_call = Some(Instant::now());
            match e.source.fetch(q).await {
                Ok(aircraft) => {
                    e.failures = 0;
                    e.blocked_until = None;
                    return Ok(Polled {
                        aircraft,
                        source: e.source.name(),
                        fallback: idx > 0,
                    });
                }
                Err(err) => {
                    e.failures += 1;
                    let retry_after = match &err {
                        SourceError::RateLimited(r) => Some(r.unwrap_or(BACKOFF_BASE)),
                        _ => None,
                    };
                    e.blocked_until = Some(Instant::now() + backoff(e.failures, retry_after));
                    last_err = Some(format!("{}: {err}", e.source.name()));
                }
            }
        }
        let now = Instant::now();
        let retry_in = self
            .entries
            .iter()
            .filter_map(|e| e.blocked_until)
            .map(|t| t.saturating_duration_since(now))
            .chain(throttled_for)
            .min()
            .unwrap_or(BACKOFF_BASE);
        Err(PollError {
            throttled: last_err.is_none() && throttled_for.is_some(),
            message: last_err.unwrap_or_else(|| "all sources backing off".into()),
            retry_in,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::bbox::BBox;
    use crate::sources::BoxFuture;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct Fake {
        name: &'static str,
        ok: bool,
        calls: Arc<AtomicU32>,
        interval: Duration,
        err: fn() -> SourceError,
    }

    fn fake(name: &'static str, ok: bool, calls: &Arc<AtomicU32>) -> Box<dyn FlightSource> {
        Box::new(Fake {
            name,
            ok,
            calls: calls.clone(),
            interval: Duration::ZERO,
            err: || SourceError::Status(500),
        })
    }

    impl FlightSource for Fake {
        fn name(&self) -> &'static str {
            self.name
        }
        fn min_interval(&self) -> Duration {
            self.interval
        }
        fn fetch<'a>(&'a self, _q: &'a Query) -> BoxFuture<'a, Result<Vec<Aircraft>, SourceError>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let (ok, err) = (self.ok, self.err);
            Box::pin(async move { if ok { Ok(vec![]) } else { Err(err()) } })
        }
    }

    fn query() -> Query {
        Query {
            lat: 0.0,
            lon: 0.0,
            radius_nm: 10,
            bbox: BBox {
                min_lat: -1.0,
                min_lon: -1.0,
                max_lat: 1.0,
                max_lon: 1.0,
            },
        }
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff(1, None), Duration::from_secs(5));
        assert_eq!(backoff(2, None), Duration::from_secs(10));
        assert_eq!(backoff(3, None), Duration::from_secs(20));
        assert_eq!(backoff(50, None), BACKOFF_MAX);
        assert_eq!(
            backoff(1, Some(Duration::from_secs(60))),
            Duration::from_secs(60)
        );
    }

    #[tokio::test]
    async fn fails_over_and_backs_off_primary() {
        let (c1, c2) = (Arc::new(AtomicU32::new(0)), Arc::new(AtomicU32::new(0)));
        let mut m = Manager::new(vec![fake("a", false, &c1), fake("b", true, &c2)]);
        let r = m.poll(&query()).await.unwrap();
        assert_eq!((r.source, r.fallback), ("b", true));
        // Primary is now backed off and not hit again immediately.
        m.poll(&query()).await.unwrap();
        assert_eq!(c1.load(Ordering::SeqCst), 1);
        assert_eq!(c2.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn all_failing_reports_retry() {
        let c = Arc::new(AtomicU32::new(0));
        let mut m = Manager::new(vec![fake("a", false, &c)]);
        let e = m.poll(&query()).await.unwrap_err();
        assert!(e.message.contains("500") && e.retry_in > Duration::ZERO && !e.throttled);
        // second poll: source is backed off, so it is not called again
        let e = m.poll(&query()).await.unwrap_err();
        assert_eq!(c.load(Ordering::SeqCst), 1);
        assert!(e.retry_in <= Duration::from_secs(5));
    }

    #[tokio::test]
    async fn rate_limit_honours_retry_after() {
        let c = Arc::new(AtomicU32::new(0));
        let mut m = Manager::new(vec![Box::new(Fake {
            name: "a",
            ok: false,
            calls: c.clone(),
            interval: Duration::ZERO,
            err: || SourceError::RateLimited(Some(Duration::from_secs(90))),
        })]);
        let e = m.poll(&query()).await.unwrap_err();
        assert!(e.retry_in > Duration::from_secs(85), "{:?}", e.retry_in);
    }

    #[tokio::test]
    async fn slow_source_is_skipped_not_awaited() {
        let c = Arc::new(AtomicU32::new(0));
        let mut m = Manager::new(vec![Box::new(Fake {
            name: "slow",
            ok: true,
            calls: c.clone(),
            interval: Duration::from_secs(30),
            err: || SourceError::Status(500),
        })]);
        m.poll(&query()).await.unwrap();
        let started = Instant::now();
        let e = m.poll(&query()).await.unwrap_err();
        assert!(started.elapsed() < Duration::from_millis(200));
        assert!(e.throttled && e.retry_in > Duration::from_secs(25));
        assert_eq!(c.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn short_spacing_is_enforced() {
        let c = Arc::new(AtomicU32::new(0));
        let mut m = Manager::new(vec![Box::new(Fake {
            name: "fi",
            ok: true,
            calls: c.clone(),
            interval: Duration::from_millis(300),
            err: || SourceError::Status(500),
        })]);
        m.poll(&query()).await.unwrap();
        let started = Instant::now();
        m.poll(&query()).await.unwrap();
        assert!(started.elapsed() >= Duration::from_millis(280));
    }

    #[test]
    fn source_order_per_choice() {
        let c = crate::sources::test_client();
        let names = |ch, osk| Manager::from_choice(&c, ch, osk).source_names();
        assert_eq!(names(SourceChoice::Auto, false), ["adsb.lol", "adsb.fi"]);
        assert_eq!(names(SourceChoice::AdsbFi, false), ["adsb.fi", "adsb.lol"]);
        assert_eq!(
            names(SourceChoice::Auto, true),
            ["adsb.lol", "adsb.fi", "OpenSky"]
        );
        assert_eq!(
            names(SourceChoice::OpenSky, true),
            ["OpenSky", "adsb.lol", "adsb.fi"]
        );
        assert_eq!(names(SourceChoice::OpenSky, false), ["adsb.lol", "adsb.fi"]);
    }
}
