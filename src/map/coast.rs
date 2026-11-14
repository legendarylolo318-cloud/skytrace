//! Bundled Natural Earth (110m) coastlines and borders: the offline basemap.

use super::projection::{View, mercator_norm};
use std::fmt::Write;

const COAST: &[u8] = include_bytes!("../../assets/coast.bin");
const BORDERS: &[u8] = include_bytes!("../../assets/borders.bin");

/// A polyline in normalised Mercator coordinates with its bounding box.
struct Line {
    pts: Vec<(f32, f32)>,
    min: (f32, f32),
    max: (f32, f32),
}

pub struct Basemap {
    coast: Vec<Line>,
    borders: Vec<Line>,
}

/// Parse the compact binary format: u32 line count, then per line u32 n and n × (f32 lon, f32 lat).
fn parse(data: &[u8]) -> Vec<Line> {
    fn rd_u32(d: &[u8], p: &mut usize) -> Option<u32> {
        let b = d.get(*p..*p + 4)?;
        *p += 4;
        Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    let mut p = 0;
    let mut out = Vec::new();
    let Some(count) = rd_u32(data, &mut p) else {
        return out;
    };
    for _ in 0..count {
        let Some(n) = rd_u32(data, &mut p) else { break };
        let mut pts = Vec::with_capacity(n as usize);
        for _ in 0..n {
            let (Some(lo), Some(la)) = (rd_u32(data, &mut p), rd_u32(data, &mut p)) else {
                return out;
            };
            let (lon, lat) = (f32::from_bits(lo), f32::from_bits(la));
            let (x, y) = mercator_norm(f64::from(lat), f64::from(lon));
            pts.push((x as f32, y as f32));
        }
        if pts.len() < 2 {
            continue;
        }
        let mut min = (f32::MAX, f32::MAX);
        let mut max = (f32::MIN, f32::MIN);
        for &(x, y) in &pts {
            min = (min.0.min(x), min.1.min(y));
            max = (max.0.max(x), max.1.max(y));
        }
        out.push(Line { pts, min, max });
    }
    out
}

impl Basemap {
    pub fn load() -> Self {
        Self {
            coast: parse(COAST),
            borders: parse(BORDERS),
        }
    }

    pub fn coast_path(&self, v: &View) -> String {
        render(&self.coast, v)
    }

    pub fn border_path(&self, v: &View) -> String {
        render(&self.borders, v)
    }
}

const MARGIN: f64 = 50.0;

fn outcode(x: f64, y: f64, w: f64, h: f64) -> u8 {
    let mut c = 0;
    if x < -MARGIN {
        c |= 1;
    } else if x > w + MARGIN {
        c |= 2;
    }
    if y < -MARGIN {
        c |= 4;
    } else if y > h + MARGIN {
        c |= 8;
    }
    c
}

/// SVG path commands for a set of polylines, culled to the viewport.
fn render(lines: &[Line], v: &View) -> String {
    let w = v.world_size();
    let (nx, ny) = mercator_norm(v.lat, v.lon);
    let (cx, cy) = (nx * w, ny * w);
    let (hw, hh) = (v.width / 2.0, v.height / 2.0);
    let mut s = String::with_capacity(8192);
    for off in -1i32..=1 {
        let ox = f64::from(off) * w;
        for l in lines {
            let sx = |x: f32| f64::from(x) * w + ox - cx + hw;
            let sy = |y: f32| f64::from(y) * w - cy + hh;
            if outcode(sx(l.min.0), sy(l.min.1), v.width, v.height)
                & outcode(sx(l.max.0), sy(l.max.1), v.width, v.height)
                != 0
            {
                continue;
            }
            let mut prev: Option<(f64, f64)> = None;
            let mut pen_down = false;
            let last = l.pts.len() - 1;
            for (i, &(px, py)) in l.pts.iter().enumerate() {
                let (x, y) = (sx(px), sy(py));
                if let Some((qx, qy)) = prev {
                    if i != last && (x - qx).abs() < 1.0 && (y - qy).abs() < 1.0 {
                        continue;
                    }
                    if outcode(x, y, v.width, v.height) & outcode(qx, qy, v.width, v.height) != 0 {
                        pen_down = false; // segment entirely outside: skip it
                        prev = Some((x, y));
                        continue;
                    }
                }
                match (pen_down, prev) {
                    (true, _) => {
                        let _ = write!(s, "L{x:.1} {y:.1} ");
                    }
                    (false, Some((qx, qy))) => {
                        let _ = write!(s, "M{qx:.1} {qy:.1} L{x:.1} {y:.1} ");
                    }
                    (false, None) => {
                        let _ = write!(s, "M{x:.1} {y:.1} ");
                    }
                }
                pen_down = true;
                prev = Some((x, y));
            }
        }
    }
    s
}

/// Path commands for an arbitrary lat/lon polyline (e.g. a flight trail).
pub fn polyline(points: &[(f64, f64)], v: &View) -> String {
    let mut s = String::new();
    let mut prev: Option<(f64, f64)> = None;
    for (i, &(lat, lon)) in points.iter().enumerate() {
        let (x, y) = v.project(lat, lon);
        if let Some((qx, qy)) = prev
            && i + 1 != points.len()
            && (x - qx).abs() < 1.0
            && (y - qy).abs() < 1.0
        {
            continue;
        }
        let _ = write!(
            s,
            "{}{x:.1} {y:.1} ",
            if prev.is_none() { 'M' } else { 'L' }
        );
        prev = Some((x, y));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_data_parses() {
        let b = Basemap::load();
        assert!(b.coast.len() > 100 && b.borders.len() > 100);
    }

    #[test]
    fn world_view_has_paths_and_far_view_is_empty() {
        let b = Basemap::load();
        let v = View {
            lat: 20.0,
            lon: 0.0,
            zoom: 2.0,
            width: 1000.0,
            height: 600.0,
        };
        let p = b.coast_path(&v);
        assert!(p.starts_with('M') && p.len() > 1000);
        // mid-Pacific at high zoom: no coastline in view
        let v = View {
            lat: 0.0,
            lon: -140.0,
            zoom: 10.0,
            width: 800.0,
            height: 600.0,
        };
        assert!(b.coast_path(&v).is_empty());
    }

    #[test]
    fn truncated_data_is_handled() {
        assert!(parse(&[1, 0, 0, 0, 5, 0, 0, 0, 1]).is_empty());
        assert!(parse(&[]).is_empty());
    }

    #[test]
    fn polyline_commands() {
        let v = View {
            lat: 0.0,
            lon: 0.0,
            zoom: 5.0,
            width: 800.0,
            height: 600.0,
        };
        let s = polyline(&[(0.0, 0.0), (0.0, 1.0)], &v);
        assert!(s.starts_with("M400.0 300.0 L"));
    }
}
