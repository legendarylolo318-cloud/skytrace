//! Aircraft state as shown in the UI, independent of any data source.

use super::geo::dead_reckon;
use std::collections::VecDeque;
use std::time::Instant;

/// Maximum time (seconds) a position is extrapolated before being held.
pub const MAX_EXTRAPOLATION_S: f64 = 60.0;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Aircraft {
    pub hex: String,
    pub callsign: Option<String>,
    pub registration: Option<String>,
    pub type_code: Option<String>,
    pub lat: f64,
    pub lon: f64,
    /// Barometric altitude in feet; `None` if unknown.
    pub alt_baro_ft: Option<f64>,
    pub alt_geom_ft: Option<f64>,
    pub on_ground: bool,
    pub gs_kt: Option<f64>,
    pub track_deg: Option<f64>,
    pub vrate_fpm: Option<f64>,
    pub squawk: Option<String>,
    pub emergency: bool,
    pub military: bool,
    /// Age of the position (seconds) at the moment it was received.
    pub pos_age_s: f64,
    pub source: &'static str,
}

impl Aircraft {
    pub fn display_name(&self) -> &str {
        self.callsign
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.hex)
    }

    /// Position extrapolated `elapsed_s` seconds after reception.
    pub fn predicted(&self, elapsed_s: f64) -> (f64, f64) {
        let dt = (self.pos_age_s + elapsed_s).clamp(0.0, MAX_EXTRAPOLATION_S);
        match (self.track_deg, self.gs_kt) {
            (Some(trk), Some(gs)) if !self.on_ground && gs > 20.0 => {
                dead_reckon(self.lat, self.lon, trk, gs, dt)
            }
            _ => (self.lat, self.lon),
        }
    }
}

pub fn is_emergency_squawk(s: &str) -> bool {
    matches!(s, "7500" | "7600" | "7700")
}

/// A tracked aircraft together with session-local data.
#[derive(Debug, Clone)]
pub struct Tracked {
    pub ac: Aircraft,
    pub received: Instant,
}

impl Tracked {
    pub fn position_now(&self, now: Instant) -> (f64, f64) {
        self.ac
            .predicted(now.saturating_duration_since(self.received).as_secs_f64())
    }
}

/// Position history of one aircraft (this session only).
#[derive(Debug, Clone, Default)]
pub struct Trail {
    pub points: VecDeque<(f64, f64)>,
    pub last_seen: Option<Instant>,
}

impl Trail {
    pub const MAX_POINTS: usize = 600;

    pub fn push(&mut self, lat: f64, lon: f64, now: Instant) {
        self.last_seen = Some(now);
        if let Some(&(plat, plon)) = self.points.back()
            && super::geo::distance_m(plat, plon, lat, lon) < 80.0
        {
            return;
        }
        if self.points.len() >= Self::MAX_POINTS {
            self.points.pop_front();
        }
        self.points.push_back((lat, lon));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plane() -> Aircraft {
        Aircraft {
            hex: "abc123".into(),
            lat: 33.0,
            lon: -112.0,
            gs_kt: Some(400.0),
            track_deg: Some(90.0),
            ..Default::default()
        }
    }

    #[test]
    fn predicted_moves_east() {
        let (lat, lon) = plane().predicted(5.0);
        assert!(lon > -112.0 && (lat - 33.0).abs() < 1e-3);
    }

    #[test]
    fn predicted_is_capped_and_ground_static() {
        let p = plane();
        assert_eq!(p.predicted(1000.0), p.predicted(MAX_EXTRAPOLATION_S));
        let g = Aircraft {
            on_ground: true,
            ..plane()
        };
        assert_eq!(g.predicted(10.0), (33.0, -112.0));
    }

    #[test]
    fn emergency_codes() {
        assert!(is_emergency_squawk("7700"));
        assert!(!is_emergency_squawk("1200"));
    }

    #[test]
    fn trail_skips_tiny_moves() {
        let mut t = Trail::default();
        let now = Instant::now();
        t.push(10.0, 10.0, now);
        t.push(10.00001, 10.0, now);
        t.push(10.1, 10.0, now);
        assert_eq!(t.points.len(), 2);
    }

    #[test]
    fn trail_is_bounded() {
        let mut t = Trail::default();
        let now = Instant::now();
        for i in 0..(Trail::MAX_POINTS + 50) {
            t.push(0.0, i as f64 * 0.01, now);
        }
        assert_eq!(t.points.len(), Trail::MAX_POINTS);
        assert_eq!(
            t.points.back(),
            Some(&(0.0, (Trail::MAX_POINTS + 49) as f64 * 0.01))
        );
    }

    #[test]
    fn position_age_is_included_in_prediction() {
        let a = Aircraft {
            pos_age_s: 10.0,
            ..plane()
        };
        assert_eq!(a.predicted(0.0), plane().predicted(10.0));
    }
}
