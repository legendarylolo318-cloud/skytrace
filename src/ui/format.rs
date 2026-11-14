//! Unit conversion, text formatting and altitude colouring.

use crate::config::Units;

pub const FT_M: f64 = 0.3048;
pub const KT_KMH: f64 = 1.852;
const FPM_MS: f64 = 0.00508;

pub fn altitude(ft: f64, u: Units) -> String {
    match u {
        Units::Imperial => format!("{} ft", group(ft.round() as i64)),
        Units::Metric => format!("{} m", group((ft * FT_M).round() as i64)),
    }
}

pub fn speed(kt: f64, u: Units) -> String {
    match u {
        Units::Imperial => format!("{} kt", kt.round() as i64),
        Units::Metric => format!("{} km/h", (kt * KT_KMH).round() as i64),
    }
}

pub fn vrate(fpm: f64, u: Units) -> String {
    match u {
        Units::Imperial => format!("{:+} ft/min", fpm.round() as i64),
        Units::Metric => format!("{:+.1} m/s", fpm * FPM_MS),
    }
}

pub fn alt_unit(u: Units) -> &'static str {
    match u {
        Units::Imperial => "ft",
        Units::Metric => "m",
    }
}

/// Convert a user-entered altitude (in display units) to feet.
pub fn altitude_input_to_ft(v: f64, u: Units) -> f64 {
    match u {
        Units::Imperial => v,
        Units::Metric => v / FT_M,
    }
}

fn group(n: i64) -> String {
    let s = n.abs().to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

pub fn age(secs: f64) -> String {
    let s = secs.max(0.0).round() as u64;
    if s < 60 {
        format!("{s} s ago")
    } else {
        format!("{} min ago", s / 60)
    }
}

pub fn position(lat: f64, lon: f64) -> String {
    format!(
        "{:.4}° {}, {:.4}° {}",
        lat.abs(),
        if lat >= 0.0 { 'N' } else { 'S' },
        lon.abs(),
        if lon >= 0.0 { 'E' } else { 'W' }
    )
}

/// Colour stops (feet, rgb) — must match the legend gradient in `main.slint`.
const STOPS: [(f64, [u8; 3]); 6] = [
    (0.0, [0xf5, 0xa5, 0x24]),
    (5_000.0, [0xd4, 0xe0, 0x3c]),
    (10_000.0, [0x3d, 0xdc, 0x84]),
    (20_000.0, [0x20, 0xc9, 0xc9]),
    (30_000.0, [0x3b, 0x82, 0xf6]),
    (40_000.0, [0xa8, 0x55, 0xf7]),
];

pub const UNKNOWN_ALT_COLOR: [u8; 3] = [0x9a, 0xa5, 0xb1];

pub fn altitude_color(alt_ft: Option<f64>) -> [u8; 3] {
    let Some(a) = alt_ft else {
        return UNKNOWN_ALT_COLOR;
    };
    let a = a.clamp(0.0, 40_000.0);
    for w in STOPS.windows(2) {
        let (a0, c0) = w[0];
        let (a1, c1) = w[1];
        if a <= a1 {
            let t = (a - a0) / (a1 - a0);
            let mix = |i: usize| {
                (f64::from(c0[i]) + (f64::from(c1[i]) - f64::from(c0[i])) * t).round() as u8
            };
            return [mix(0), mix(1), mix(2)];
        }
    }
    STOPS[STOPS.len() - 1].1
}

/// Legend tick labels for 0..40k ft in the chosen units.
pub fn legend_labels(u: Units) -> Vec<String> {
    [0.0, 10_000.0, 20_000.0, 30_000.0, 40_000.0]
        .iter()
        .map(|&ft| match u {
            Units::Imperial => format!("{}k ft", (ft / 1000.0) as i64),
            Units::Metric => format!("{:.0} km", ft * FT_M / 1000.0),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_units() {
        assert_eq!(altitude(34000.0, Units::Imperial), "34,000 ft");
        assert_eq!(altitude(34000.0, Units::Metric), "10,363 m");
        assert_eq!(speed(450.0, Units::Metric), "833 km/h");
        assert_eq!(vrate(-640.0, Units::Imperial), "-640 ft/min");
        assert_eq!(vrate(1000.0, Units::Metric), "+5.1 m/s");
        assert_eq!(group(-1234567), "-1,234,567");
        assert!((altitude_input_to_ft(3048.0, Units::Metric) - 10000.0).abs() < 1e-9);
    }

    #[test]
    fn colors_interpolate_and_clamp() {
        assert_eq!(altitude_color(Some(0.0)), STOPS[0].1);
        assert_eq!(altitude_color(Some(99_999.0)), STOPS[5].1);
        assert_eq!(altitude_color(Some(10_000.0)), STOPS[2].1);
        assert_eq!(altitude_color(None), UNKNOWN_ALT_COLOR);
        let mid = altitude_color(Some(2_500.0));
        assert!(mid[0] < 0xf5 && mid[0] > 0xd4);
    }

    #[test]
    fn misc() {
        assert_eq!(age(3.2), "3 s ago");
        assert_eq!(age(125.0), "2 min ago");
        assert_eq!(position(-33.5, 151.2), "33.5000° S, 151.2000° E");
        assert_eq!(legend_labels(Units::Imperial)[4], "40k ft");
    }
}
