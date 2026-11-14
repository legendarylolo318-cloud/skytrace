//! ADS-B Exchange v2 format (adsb.lol, adsb.fi).
//!
//! airplanes.live used to be supported here, but its REST API is no longer
//! open to the public (HTTP 403 for non-feeders), so adsb.fi is the fallback.

use super::{BoxFuture, FlightSource, Query, SourceError, get_text};
use crate::model::aircraft::{Aircraft, is_emergency_squawk};
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;

fn lenient_f64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
    let v = Value::deserialize(d)?;
    Ok(num(&v))
}

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Accept strings and numbers (e.g. a squawk sent as `1200`) as text.
fn lenient_str<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::String(s) => Some(s),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    })
}

fn clean(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

#[derive(Debug, Deserialize, Default)]
struct LastPos {
    #[serde(default, deserialize_with = "lenient_f64")]
    lat: Option<f64>,
    #[serde(default, deserialize_with = "lenient_f64")]
    lon: Option<f64>,
    #[serde(default, deserialize_with = "lenient_f64")]
    seen_pos: Option<f64>,
}

#[derive(Debug, Deserialize, Default)]
struct Raw {
    #[serde(default, deserialize_with = "lenient_str")]
    hex: Option<String>,
    #[serde(default, deserialize_with = "lenient_str")]
    flight: Option<String>,
    #[serde(default, deserialize_with = "lenient_str")]
    r: Option<String>,
    #[serde(default, deserialize_with = "lenient_str")]
    t: Option<String>,
    #[serde(default, deserialize_with = "lenient_f64")]
    lat: Option<f64>,
    #[serde(default, deserialize_with = "lenient_f64")]
    lon: Option<f64>,
    alt_baro: Option<Value>,
    #[serde(default, deserialize_with = "lenient_f64")]
    alt_geom: Option<f64>,
    #[serde(default, deserialize_with = "lenient_f64")]
    gs: Option<f64>,
    #[serde(default, deserialize_with = "lenient_f64")]
    track: Option<f64>,
    #[serde(default, deserialize_with = "lenient_f64")]
    true_heading: Option<f64>,
    #[serde(default, deserialize_with = "lenient_f64")]
    mag_heading: Option<f64>,
    #[serde(default, deserialize_with = "lenient_f64")]
    baro_rate: Option<f64>,
    #[serde(default, deserialize_with = "lenient_f64")]
    geom_rate: Option<f64>,
    #[serde(default, deserialize_with = "lenient_str")]
    squawk: Option<String>,
    #[serde(default, deserialize_with = "lenient_str")]
    emergency: Option<String>,
    #[serde(rename = "dbFlags", default, deserialize_with = "lenient_f64")]
    db_flags: Option<f64>,
    #[serde(default, deserialize_with = "lenient_f64")]
    seen_pos: Option<f64>,
    #[serde(rename = "lastPosition")]
    last_position: Option<LastPos>,
}

#[derive(Debug, Deserialize)]
struct Response {
    #[serde(alias = "aircraft")]
    ac: Option<Vec<Value>>,
}

/// Parse an ADS-B Exchange v2 response body. Entries without a usable
/// position or hex are skipped; unknown or malformed fields never fail the parse.
pub fn parse_response(body: &str, source: &'static str) -> Result<Vec<Aircraft>, SourceError> {
    let resp: Response =
        serde_json::from_str(body).map_err(|e| SourceError::Parse(e.to_string()))?;
    let mut out = Vec::new();
    for v in resp.ac.unwrap_or_default() {
        let Ok(raw) = serde_json::from_value::<Raw>(v) else {
            continue;
        };
        if let Some(a) = convert(raw, source) {
            out.push(a);
        }
    }
    Ok(out)
}

fn convert(r: Raw, source: &'static str) -> Option<Aircraft> {
    let hex = clean(r.hex)?.to_ascii_lowercase();
    let (lat, lon, pos_age) = match (r.lat, r.lon) {
        (Some(la), Some(lo)) => (la, lo, r.seen_pos.unwrap_or(0.0)),
        _ => {
            let lp = r.last_position?;
            (lp.lat?, lp.lon?, lp.seen_pos.unwrap_or(0.0))
        }
    };
    if !lat.is_finite()
        || !lon.is_finite()
        || !(-90.0..=90.0).contains(&lat)
        || !(-180.0..=180.0).contains(&lon)
    {
        return None;
    }
    let (on_ground, alt_baro) = match &r.alt_baro {
        Some(Value::String(s)) if s.eq_ignore_ascii_case("ground") => (true, Some(0.0)),
        Some(v) => (false, num(v)),
        None => (false, None),
    };
    let squawk = clean(r.squawk);
    let emergency = squawk.as_deref().is_some_and(is_emergency_squawk)
        || r.emergency
            .as_deref()
            .is_some_and(|e| !e.is_empty() && !e.eq_ignore_ascii_case("none"));
    Some(Aircraft {
        hex,
        callsign: clean(r.flight),
        registration: clean(r.r),
        type_code: clean(r.t),
        lat,
        lon,
        alt_baro_ft: alt_baro,
        alt_geom_ft: r.alt_geom,
        on_ground,
        gs_kt: r.gs,
        // Ground traffic and some transponders send only a heading, not a track.
        track_deg: r
            .track
            .or(r.true_heading)
            .or(r.mag_heading)
            .filter(|t| t.is_finite())
            .map(|t| t.rem_euclid(360.0)),
        vrate_fpm: r.baro_rate.or(r.geom_rate),
        squawk,
        emergency,
        military: r.db_flags.is_some_and(|f| (f as u64) & 1 == 1),
        pos_age_s: if pos_age.is_finite() {
            pos_age.max(0.0)
        } else {
            0.0
        },
        source,
    })
}

/// Normalise a longitude into -180..=180 (the map centre can drift past ±180 when panning).
fn wrap_lon(lon: f64) -> f64 {
    let l = (lon + 180.0).rem_euclid(360.0) - 180.0;
    if l == -180.0 && lon > 0.0 { 180.0 } else { l }
}

/// Which ADS-B Exchange v2 compatible host to use.
#[derive(Debug, Clone, Copy)]
pub enum Provider {
    AdsbLol,
    AdsbFi,
}

pub struct AdsbxSource {
    client: reqwest::Client,
    provider: Provider,
    base: String,
}

impl AdsbxSource {
    pub fn new(client: reqwest::Client, provider: Provider) -> Self {
        let base = match provider {
            Provider::AdsbLol => "https://api.adsb.lol/v2",
            Provider::AdsbFi => "https://opendata.adsb.fi/api/v3",
        };
        Self {
            client,
            provider,
            base: base.to_string(),
        }
    }

    /// Both hosts use the same `/lat/{lat}/lon/{lon}/dist/{nm}` path (radius ≤ 250 nm).
    fn url(&self, q: &Query) -> String {
        format!(
            "{}/lat/{:.4}/lon/{:.4}/dist/{}",
            self.base,
            q.lat.clamp(-90.0, 90.0),
            wrap_lon(q.lon),
            q.radius_nm.clamp(1, super::MAX_RADIUS_NM)
        )
    }
}

impl FlightSource for AdsbxSource {
    fn name(&self) -> &'static str {
        match self.provider {
            Provider::AdsbLol => "adsb.lol",
            Provider::AdsbFi => "adsb.fi",
        }
    }

    fn min_interval(&self) -> Duration {
        match self.provider {
            Provider::AdsbLol => Duration::from_secs(1),
            Provider::AdsbFi => Duration::from_millis(1100), // documented limit: 1 req/s
        }
    }

    fn fetch<'a>(&'a self, q: &'a Query) -> BoxFuture<'a, Result<Vec<Aircraft>, SourceError>> {
        Box::pin(async move {
            let body = get_text(&self.client, &self.url(q)).await?;
            parse_response(&body, self.name())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = include_str!("../../tests/fixtures/adsblol_sample.json");

    #[test]
    fn parses_sample_and_skips_bad_entries() {
        let v = parse_response(SAMPLE, "adsb.lol").unwrap();
        let hexes: Vec<_> = v.iter().map(|a| a.hex.as_str()).collect();
        assert_eq!(hexes, ["a1b2c3", "ae1234", "~abcdef", "lastpo", "strnum"]);
    }

    #[test]
    fn full_record() {
        let v = parse_response(SAMPLE, "adsb.lol").unwrap();
        let a = &v[0];
        assert_eq!(a.callsign.as_deref(), Some("SWA1234"));
        assert_eq!(a.registration.as_deref(), Some("N8620H"));
        assert_eq!(a.type_code.as_deref(), Some("B738"));
        assert_eq!(a.alt_baro_ft, Some(34000.0));
        assert_eq!(a.alt_geom_ft, Some(34650.0));
        assert_eq!(a.vrate_fpm, Some(-64.0));
        assert!(!a.on_ground && !a.emergency && !a.military);
        assert!((a.pos_age_s - 0.4).abs() < 1e-9);
    }

    #[test]
    fn ground_military_emergency() {
        let v = parse_response(SAMPLE, "x").unwrap();
        let a = &v[1];
        assert!(a.on_ground && a.military && a.emergency);
        assert_eq!(a.alt_baro_ft, Some(0.0));
    }

    #[test]
    fn missing_fields_and_last_position() {
        let v = parse_response(SAMPLE, "x").unwrap();
        let minimal = &v[2];
        assert!(minimal.callsign.is_none() && minimal.gs_kt.is_none());
        let last = &v[3];
        assert_eq!((last.lat, last.lon), (34.1, -111.5));
        assert!((last.pos_age_s - 20.5).abs() < 1e-9);
    }

    #[test]
    fn string_numbers_are_tolerated() {
        let v = parse_response(SAMPLE, "x").unwrap();
        let s = &v[4];
        assert_eq!(s.alt_baro_ft, Some(8000.0));
        assert_eq!(s.gs_kt, Some(250.0));
        assert_eq!(s.track_deg, None);
        assert_eq!(s.lat, 33.2);
    }

    /// Trimmed response captured from the live adsb.lol API (Nov 2026).
    #[test]
    fn parses_live_capture() {
        let v = parse_response(
            include_str!("../../tests/fixtures/adsblol_live.json"),
            "adsb.lol",
        )
        .unwrap();
        assert_eq!(v.len(), 6);
        let ground = v.iter().find(|a| a.hex == "a57d55").unwrap();
        assert!(
            ground.on_ground && ground.track_deg.is_some(),
            "heading from true_heading"
        );
        let dal = v.iter().find(|a| a.hex == "a8de10").unwrap();
        assert_eq!(dal.callsign.as_deref(), Some("DAL540"));
        assert_eq!(dal.alt_baro_ft, Some(33000.0));
        assert!(v.iter().all(|a| a.lat.is_finite() && a.pos_age_s >= 0.0));
    }

    #[test]
    fn heading_fallback_and_numeric_strings() {
        let body = r#"{"ac":[{"hex":"a57d55","alt_baro":"ground","gs":0.0,"true_heading":182.8,"squawk":1200,"lat":33.4,"lon":-112.3},
                             {"hex":"b1","mag_heading":-10,"lat":1,"lon":2}]}"#;
        let v = parse_response(body, "x").unwrap();
        assert_eq!(v[0].track_deg, Some(182.8));
        assert_eq!(v[0].squawk.as_deref(), Some("1200"));
        assert_eq!(v[1].track_deg, Some(350.0));
    }

    #[test]
    fn urls_match_documented_endpoints() {
        let c = crate::sources::test_client();
        let q = Query {
            lat: 33.44841,
            lon: 247.926, // panned past the antimeridian
            radius_nm: 900,
            bbox: crate::model::bbox::BBox {
                min_lat: 0.0,
                min_lon: 0.0,
                max_lat: 1.0,
                max_lon: 1.0,
            },
        };
        assert_eq!(
            AdsbxSource::new(c.clone(), Provider::AdsbLol).url(&q),
            "https://api.adsb.lol/v2/lat/33.4484/lon/-112.0740/dist/250"
        );
        assert_eq!(
            AdsbxSource::new(c, Provider::AdsbFi).url(&q),
            "https://opendata.adsb.fi/api/v3/lat/33.4484/lon/-112.0740/dist/250"
        );
        assert_eq!(wrap_lon(180.0), 180.0);
        assert_eq!(wrap_lon(-180.0), -180.0);
        assert!((wrap_lon(-190.0) - 170.0).abs() < 1e-9);
    }

    #[test]
    fn invalid_json_is_an_error_and_empty_is_ok() {
        assert!(parse_response("not json", "x").is_err());
        assert!(parse_response("{}", "x").unwrap().is_empty());
        assert!(
            parse_response(r#"{"aircraft":[]}"#, "x")
                .unwrap()
                .is_empty()
        );
    }
}
