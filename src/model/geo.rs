//! Spherical geometry helpers and dead reckoning.

pub const EARTH_RADIUS_M: f64 = 6_371_008.8;
pub const KNOT_MS: f64 = 0.514_444;
pub const NM_M: f64 = 1852.0;

/// Great-circle distance in metres (haversine).
pub fn distance_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dphi = p2 - p1;
    let dl = (lon2 - lon1).to_radians();
    let a = (dphi / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_M * a.sqrt().min(1.0).asin()
}

/// Move from a position along a true track at a ground speed for `dt` seconds.
pub fn dead_reckon(lat: f64, lon: f64, track_deg: f64, gs_kt: f64, dt_s: f64) -> (f64, f64) {
    if !(dt_s > 0.0 && gs_kt > 0.0 && track_deg.is_finite()) {
        return (lat, lon);
    }
    let d = gs_kt * KNOT_MS * dt_s / EARTH_RADIUS_M; // angular distance
    let brg = track_deg.to_radians();
    let p1 = lat.to_radians();
    let l1 = lon.to_radians();
    // clamp: rounding can push the argument just past ±1 near the poles (-> NaN)
    let p2 = (p1.sin() * d.cos() + p1.cos() * d.sin() * brg.cos())
        .clamp(-1.0, 1.0)
        .asin();
    let l2 = l1 + (brg.sin() * d.sin() * p1.cos()).atan2(d.cos() - p1.sin() * p2.sin());
    let mut lon2 = l2.to_degrees();
    if lon2 > 180.0 {
        lon2 -= 360.0;
    } else if lon2 < -180.0 {
        lon2 += 360.0;
    }
    (p2.to_degrees(), lon2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_one_degree_lat() {
        let d = distance_m(0.0, 0.0, 1.0, 0.0);
        assert!((d - 111_195.0).abs() < 100.0, "{d}");
    }

    #[test]
    fn dead_reckon_north() {
        // 360 kt for 10 s = ~1852 m north = ~1/60 degree.
        let (lat, lon) = dead_reckon(10.0, 20.0, 0.0, 360.0, 10.0);
        assert!((lat - (10.0 + 1.0 / 60.0)).abs() < 1e-3, "{lat}");
        assert!((lon - 20.0).abs() < 1e-9);
    }

    #[test]
    fn dead_reckon_east_matches_distance() {
        let (lat, lon) = dead_reckon(45.0, 10.0, 90.0, 500.0, 60.0);
        let d = distance_m(45.0, 10.0, lat, lon);
        assert!((d - 500.0 * KNOT_MS * 60.0).abs() < 1.0, "{d}");
        assert!(lon > 10.0);
    }

    #[test]
    fn dead_reckon_south_west_and_poles() {
        let (lat, lon) = dead_reckon(10.0, 20.0, 225.0, 600.0, 60.0);
        assert!(lat < 10.0 && lon < 20.0);
        let (lat, lon) = dead_reckon(89.999, 0.0, 0.0, 600.0, 60.0);
        assert!(lat.is_finite() && lon.is_finite());
        assert_eq!(dead_reckon(1.0, 2.0, f64::NAN, 300.0, 5.0), (1.0, 2.0));
    }

    #[test]
    fn dead_reckon_noop_and_wrap() {
        assert_eq!(dead_reckon(1.0, 2.0, 45.0, 0.0, 10.0), (1.0, 2.0));
        let (_, lon) = dead_reckon(0.0, 179.99, 90.0, 600.0, 120.0);
        assert!(lon < 0.0, "should wrap past antimeridian: {lon}");
    }
}
