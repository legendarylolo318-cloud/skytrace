//! adsbdb.com aircraft + route lookups, cached in memory and on disk.

use super::{SourceError, get_text};
use crate::model::meta::{AircraftInfo, Airport, RouteInfo};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const AIRCRAFT_TTL_S: u64 = 30 * 24 * 3600;
const ROUTE_TTL_S: u64 = 12 * 3600;
const MAX_ENTRIES: usize = 5000;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry<T> {
    t: u64,
    /// `None` is a cached "unknown".
    v: Option<T>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Cache {
    aircraft: HashMap<String, Entry<AircraftInfo>>,
    routes: HashMap<String, Entry<RouteInfo>>,
}

impl Cache {
    /// Keep the cache bounded: drop expired entries, then the oldest ones.
    fn prune(&mut self, now: u64, max: usize) {
        self.aircraft
            .retain(|_, e| now.saturating_sub(e.t) < AIRCRAFT_TTL_S);
        self.routes
            .retain(|_, e| now.saturating_sub(e.t) < ROUTE_TTL_S);
        let total = self.aircraft.len() + self.routes.len();
        if total <= max {
            return;
        }
        let mut ts: Vec<u64> = self.aircraft.values().map(|e| e.t).collect();
        ts.extend(self.routes.values().map(|e| e.t));
        ts.sort_unstable();
        let cutoff = ts[total - max];
        self.aircraft.retain(|_, e| e.t >= cutoff);
        self.routes.retain(|_, e| e.t >= cutoff);
        // ties at the cutoff can leave a few extra; trim deterministically
        while self.aircraft.len() + self.routes.len() > max {
            if let Some(k) = self.routes.keys().next().cloned() {
                self.routes.remove(&k);
            } else if let Some(k) = self.aircraft.keys().next().cloned() {
                self.aircraft.remove(&k);
            }
        }
    }
}

fn now_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn s(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(String::from)
}

pub fn parse_aircraft(body: &str) -> Option<AircraftInfo> {
    let v: Value = serde_json::from_str(body).ok()?;
    let a = v.get("response")?.get("aircraft")?;
    Some(AircraftInfo {
        registration: s(a, "registration"),
        type_name: s(a, "type"),
        icao_type: s(a, "icao_type"),
        manufacturer: s(a, "manufacturer"),
        operator: s(a, "registered_owner"),
        country: s(a, "registered_owner_country_name"),
    })
}

fn airport(v: Option<&Value>) -> Option<Airport> {
    let v = v?;
    if !v.is_object() {
        return None;
    }
    Some(Airport {
        iata: s(v, "iata_code"),
        icao: s(v, "icao_code"),
        name: s(v, "name"),
        municipality: s(v, "municipality"),
    })
}

pub fn parse_route(body: &str) -> Option<RouteInfo> {
    let v: Value = serde_json::from_str(body).ok()?;
    let r = v.get("response")?.get("flightroute")?;
    Some(RouteInfo {
        airline: r.get("airline").and_then(|a| s(a, "name")),
        origin: airport(r.get("origin")),
        destination: airport(r.get("destination")),
    })
}

pub struct AdsbDb {
    client: reqwest::Client,
    cache: Mutex<Cache>,
    path: PathBuf,
}

impl AdsbDb {
    pub fn new(client: reqwest::Client, cache_dir: PathBuf) -> Self {
        let path = cache_dir.join("adsbdb.json");
        let mut cache: Cache = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        cache.prune(now_s(), MAX_ENTRIES);
        Self {
            client,
            cache: Mutex::new(cache),
            path,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Cache> {
        self.cache.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Write the cache atomically (temp file + rename) so concurrent lookups or a
    /// crash mid-write can never leave a truncated/corrupt cache file behind.
    async fn persist(&self) {
        let text = {
            let mut c = self.lock();
            c.prune(now_s(), MAX_ENTRIES);
            serde_json::to_string(&*c).ok()
        };
        let Some(text) = text else { return };
        if let Some(dir) = self.path.parent() {
            let _ = tokio::fs::create_dir_all(dir).await;
        }
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let tmp = self
            .path
            .with_extension(format!("json.{}.tmp", SEQ.fetch_add(1, Ordering::Relaxed)));
        if tokio::fs::write(&tmp, text).await.is_ok()
            && tokio::fs::rename(&tmp, &self.path).await.is_err()
        {
            let _ = tokio::fs::remove_file(&tmp).await;
        }
    }

    /// Fetch `url`; `Ok(None)` means the service answered "unknown" (cacheable).
    async fn fetch<T>(
        &self,
        url: &str,
        parse: fn(&str) -> Option<T>,
    ) -> Result<Option<T>, SourceError> {
        match get_text(&self.client, url).await {
            Ok(body) => Ok(parse(&body)),
            Err(SourceError::Status(404)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn aircraft(&self, hex: &str) -> Option<AircraftInfo> {
        let key = hex.to_ascii_lowercase();
        if !key.chars().all(|c| c.is_ascii_hexdigit()) || key.is_empty() {
            return None;
        }
        if let Some(e) = self.lock().aircraft.get(&key)
            && now_s().saturating_sub(e.t) < AIRCRAFT_TTL_S
        {
            return e.v.clone();
        }
        let url = format!("https://api.adsbdb.com/v0/aircraft/{key}");
        let v = self.fetch(&url, parse_aircraft).await.ok()?;
        self.lock().aircraft.insert(
            key,
            Entry {
                t: now_s(),
                v: v.clone(),
            },
        );
        self.persist().await;
        v
    }

    pub async fn route(&self, callsign: &str) -> Option<RouteInfo> {
        let key = callsign.trim().to_ascii_uppercase();
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric()) {
            return None;
        }
        if let Some(e) = self.lock().routes.get(&key)
            && now_s().saturating_sub(e.t) < ROUTE_TTL_S
        {
            return e.v.clone();
        }
        let url = format!("https://api.adsbdb.com/v0/callsign/{key}");
        let v = self.fetch(&url, parse_route).await.ok()?;
        self.lock().routes.insert(
            key,
            Entry {
                t: now_s(),
                v: v.clone(),
            },
        );
        self.persist().await;
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_aircraft() {
        let a = parse_aircraft(include_str!("../../tests/fixtures/adsbdb_aircraft.json")).unwrap();
        assert_eq!(a.registration.as_deref(), Some("N8620H"));
        assert_eq!(a.operator.as_deref(), Some("Southwest Airlines"));
        assert_eq!(a.icao_type.as_deref(), Some("B738"));
    }

    #[test]
    fn unknown_aircraft_is_none() {
        assert!(parse_aircraft(include_str!("../../tests/fixtures/adsbdb_unknown.json")).is_none());
        assert!(parse_aircraft("garbage").is_none());
    }

    #[test]
    fn cache_is_bounded_and_expires() {
        let mut c = Cache::default();
        let now = 10_000_000;
        for i in 0..50u64 {
            c.aircraft.insert(
                format!("a{i}"),
                Entry {
                    t: now - i,
                    v: None,
                },
            );
            c.routes.insert(
                format!("r{i}"),
                Entry {
                    t: now - i,
                    v: None,
                },
            );
        }
        c.routes.insert(
            "old".into(),
            Entry {
                t: now - ROUTE_TTL_S - 1,
                v: None,
            },
        );
        c.prune(now, 30);
        assert_eq!(c.aircraft.len() + c.routes.len(), 30);
        assert!(!c.routes.contains_key("old"));
        assert!(c.aircraft.contains_key("a0") && !c.aircraft.contains_key("a49"));
    }

    #[test]
    fn corrupt_cache_file_is_ignored() {
        let dir = std::env::temp_dir().join(format!("skytrace-test-adsbdb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("adsbdb.json"), "{not json").unwrap();
        let db = AdsbDb::new(crate::sources::test_client(), dir.clone());
        assert!(db.lock().aircraft.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn route_without_airports_has_no_summary() {
        let r =
            parse_route(r#"{"response":{"flightroute":{"callsign":"X1","origin":null}}}"#).unwrap();
        assert!(r.summary().is_none() && r.airline.is_none());
        assert!(parse_route(r#"{"response":"unknown callsign"}"#).is_none());
    }

    #[test]
    fn parses_route() {
        let r = parse_route(include_str!("../../tests/fixtures/adsbdb_route.json")).unwrap();
        assert_eq!(r.summary().unwrap(), "PHX (Phoenix) → LAX (Los Angeles)");
        assert_eq!(r.airline.as_deref(), Some("Southwest Airlines"));
    }
}
