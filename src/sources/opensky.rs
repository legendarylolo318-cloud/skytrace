//! OpenSky Network (optional, off by default; anonymous access, credit-limited).

use super::{BoxFuture, FlightSource, Query, SourceError, get_text};
use crate::model::aircraft::{Aircraft, is_emergency_squawk};
use serde_json::Value;
use std::time::Duration;

const M_TO_FT: f64 = 3.280_84;
const MS_TO_KT: f64 = 1.943_84;
const MS_TO_FPM: f64 = 196.850;

pub fn parse_response(body: &str, source: &'static str) -> Result<Vec<Aircraft>, SourceError> {
    let v: Value = serde_json::from_str(body).map_err(|e| SourceError::Parse(e.to_string()))?;
    let Some(states) = v.get("states").and_then(Value::as_array) else {
        return Ok(Vec::new()); // `states: null` means no aircraft
    };
    let now = v.get("time").and_then(Value::as_f64);
    Ok(states
        .iter()
        .filter_map(|s| convert(s, now, source))
        .collect())
}

/// Convert one state vector (index order per the OpenSky REST docs:
/// 0 icao24, 1 callsign, 3 time_position, 5 lon, 6 lat, 7 baro_altitude [m],
/// 8 on_ground, 9 velocity [m/s], 10 true_track, 11 vertical_rate [m/s],
/// 13 geo_altitude [m], 14 squawk).
fn convert(s: &Value, now: Option<f64>, source: &'static str) -> Option<Aircraft> {
    let a = s.as_array()?;
    let f = |i: usize| a.get(i).and_then(Value::as_f64);
    let st = |i: usize| {
        a.get(i)
            .and_then(Value::as_str)
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
    };
    let hex = st(0)?.to_ascii_lowercase();
    let (lon, lat) = (f(5)?, f(6)?);
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return None;
    }
    let squawk = st(14);
    // Positions can be many seconds older than the response; age them for dead reckoning.
    let pos_age_s = match (now, f(3)) {
        (Some(t), Some(tp)) => (t - tp).clamp(0.0, 300.0),
        _ => 0.0,
    };
    Some(Aircraft {
        hex,
        callsign: st(1),
        lat,
        lon,
        alt_baro_ft: f(7).map(|m| m * M_TO_FT),
        alt_geom_ft: f(13).map(|m| m * M_TO_FT),
        on_ground: a.get(8).and_then(Value::as_bool).unwrap_or(false),
        gs_kt: f(9).map(|v| v * MS_TO_KT),
        track_deg: f(10),
        vrate_fpm: f(11).map(|v| v * MS_TO_FPM),
        emergency: squawk.as_deref().is_some_and(is_emergency_squawk),
        squawk,
        pos_age_s,
        source,
        ..Default::default()
    })
}

pub struct OpenSkySource {
    client: reqwest::Client,
}

impl OpenSkySource {
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

impl FlightSource for OpenSkySource {
    fn name(&self) -> &'static str {
        "OpenSky"
    }

    fn min_interval(&self) -> Duration {
        // Anonymous data has 10 s resolution and a 400 credits/day budget
        // (1–4 credits per request depending on area), so poll it sparingly.
        Duration::from_secs(30)
    }

    fn fetch<'a>(&'a self, q: &'a Query) -> BoxFuture<'a, Result<Vec<Aircraft>, SourceError>> {
        Box::pin(async move {
            let b = q.bbox;
            let url = format!(
                "https://opensky-network.org/api/states/all?lamin={:.3}&lomin={:.3}&lamax={:.3}&lomax={:.3}",
                b.min_lat, b.min_lon, b.max_lat, b.max_lon
            );
            let body = get_text(&self.client, &url).await?;
            parse_response(&body, self.name())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_states_and_converts_units() {
        let v = parse_response(
            include_str!("../../tests/fixtures/opensky_sample.json"),
            "OpenSky",
        )
        .unwrap();
        let hexes: Vec<_> = v.iter().map(|a| a.hex.as_str()).collect();
        assert_eq!(hexes, ["a1b2c3", "123456"]); // no-position and malformed rows skipped
        assert!((v[0].alt_baro_ft.unwrap() - 32808.4).abs() < 1.0);
        assert!((v[0].gs_kt.unwrap() - 388.77).abs() < 0.1);
        assert!(v[1].on_ground && v[1].emergency);
        assert_eq!(v[1].alt_baro_ft, None);
        assert!((v[0].vrate_fpm.unwrap() - 1000.0).abs() < 1.0);
        assert_eq!(v[0].pos_age_s, 0.0);
    }

    #[test]
    fn position_age_from_time_position() {
        let body = r#"{"time":1700000020,"states":[["abc","X",null,1700000005,1700000019,10.0,50.0,1000.0,false,100.0,45.0,0.0,null,null,null,false,0]]}"#;
        let v = parse_response(body, "x").unwrap();
        assert_eq!(v[0].pos_age_s, 15.0);
        assert_eq!(v[0].track_deg, Some(45.0));
    }

    #[test]
    fn null_states_is_empty() {
        assert!(
            parse_response(r#"{"time":1,"states":null}"#, "x")
                .unwrap()
                .is_empty()
        );
        assert!(parse_response("[", "x").is_err());
    }
}
