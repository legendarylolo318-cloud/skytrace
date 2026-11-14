//! Web Mercator math, viewport handling and tile coverage.

use crate::model::bbox::BBox;
use crate::model::geo::{NM_M, distance_m};
use crate::sources::{MAX_RADIUS_NM, Query};
use std::f64::consts::PI;

pub const TILE_SIZE: f64 = 256.0;
pub const MIN_ZOOM: f64 = 2.0;
pub const MAX_ZOOM: f64 = 18.0;
/// Extra radius (nm) queried beyond the visible area.
pub const QUERY_MARGIN_NM: u32 = 10;
/// Web Mercator latitude limit.
pub const MAX_LAT: f64 = 85.051_128_78;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TileKey {
    pub z: u8,
    pub x: u32,
    pub y: u32,
}

/// Normalised Mercator coordinates in 0..1 for a lat/lon.
pub fn mercator_norm(lat: f64, lon: f64) -> (f64, f64) {
    let lat = lat.clamp(-MAX_LAT, MAX_LAT).to_radians();
    let x = (lon + 180.0) / 360.0;
    let y = (1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / PI) / 2.0;
    (x, y)
}

pub fn mercator_norm_inv(x: f64, y: f64) -> (f64, f64) {
    let lon = x * 360.0 - 180.0;
    let lat = (PI * (1.0 - 2.0 * y)).sinh().atan().to_degrees();
    (lat, lon)
}

/// Tile containing a lat/lon at an integer zoom.
#[cfg(test)]
pub fn latlon_to_tile(lat: f64, lon: f64, z: u8) -> TileKey {
    let n = f64::from(1u32 << z);
    let (x, y) = mercator_norm(lat, lon);
    TileKey {
        z,
        x: ((x * n).floor() as i64).clamp(0, n as i64 - 1) as u32,
        y: ((y * n).floor() as i64).clamp(0, n as i64 - 1) as u32,
    }
}

/// A query for a circle of `radius_nm` around a point, with its bounding box
/// (used by OpenSky). Boxes crossing a pole or the antimeridian are widened.
pub fn circle_query(lat: f64, lon: f64, radius_nm: u32) -> Query {
    let radius_nm = radius_nm.clamp(1, MAX_RADIUS_NM);
    let dlat = f64::from(radius_nm) / 60.0;
    let (min_lat, max_lat) = ((lat - dlat).max(-90.0), (lat + dlat).min(90.0));
    let cos = lat.to_radians().cos().max(0.01);
    let dlon = dlat / cos;
    let (min_lon, max_lon) =
        if max_lat >= 89.0 || min_lat <= -89.0 || lon - dlon < -180.0 || lon + dlon > 180.0 {
            (-180.0, 180.0)
        } else {
            (lon - dlon, lon + dlon)
        };
    Query {
        lat,
        lon,
        radius_nm,
        bbox: BBox {
            min_lat,
            min_lon,
            max_lat,
            max_lon,
        },
    }
}

/// Fill a `{z}/{x}/{y}` URL template.
pub fn tile_url(template: &str, k: TileKey) -> String {
    template
        .replace("{z}", &k.z.to_string())
        .replace("{x}", &k.x.to_string())
        .replace("{y}", &k.y.to_string())
        .replace("{s}", "a")
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlacedTile {
    pub key: TileKey,
    pub x: f64,
    pub y: f64,
    pub size: f64,
}

/// The visible map window: centre, (fractional) zoom and size in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View {
    pub lat: f64,
    pub lon: f64,
    pub zoom: f64,
    pub width: f64,
    pub height: f64,
}

impl View {
    pub fn world_size(&self) -> f64 {
        TILE_SIZE * 2f64.powf(self.zoom)
    }

    fn center_world(&self) -> (f64, f64) {
        let (nx, ny) = mercator_norm(self.lat, self.lon);
        let w = self.world_size();
        (nx * w, ny * w)
    }

    pub fn project(&self, lat: f64, lon: f64) -> (f64, f64) {
        let (cx, cy) = self.center_world();
        let w = self.world_size();
        let (nx, ny) = mercator_norm(lat, lon);
        let mut dx = nx * w - cx;
        // choose the nearest world copy so wrapping near the antimeridian works
        dx = (dx + w / 2.0).rem_euclid(w) - w / 2.0;
        (dx + self.width / 2.0, ny * w - cy + self.height / 2.0)
    }

    pub fn unproject(&self, sx: f64, sy: f64) -> (f64, f64) {
        let (cx, cy) = self.center_world();
        let w = self.world_size();
        let nx = ((cx + sx - self.width / 2.0) / w).rem_euclid(1.0);
        let ny = ((cy + sy - self.height / 2.0) / w).clamp(0.0, 1.0);
        mercator_norm_inv(nx, ny)
    }

    pub fn pan(&mut self, dx: f64, dy: f64) {
        let (lat, lon) = self.unproject(self.width / 2.0 - dx, self.height / 2.0 - dy);
        self.lat = lat.clamp(-MAX_LAT, MAX_LAT);
        self.lon = lon;
    }

    /// Change zoom keeping the geographic point under the cursor fixed.
    pub fn zoom_at(&mut self, sx: f64, sy: f64, new_zoom: f64) {
        let new_zoom = new_zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        let (alat, alon) = self.unproject(sx, sy);
        self.zoom = new_zoom;
        let (ax, ay) = self.project(alat, alon);
        self.pan(sx - ax, sy - ay);
    }

    pub fn bbox(&self) -> BBox {
        let (tl_lat, tl_lon) = self.unproject(0.0, 0.0);
        let (br_lat, br_lon) = self.unproject(self.width, self.height);
        // When the view spans the antimeridian the left edge is east of the right
        // edge (e.g. 170 vs -170); a min/max box would then cover the wrong side of
        // the globe, so query the full longitude range instead.
        let (min_lon, max_lon) = if self.world_size() <= self.width || tl_lon > br_lon {
            (-180.0, 180.0)
        } else {
            (tl_lon, br_lon)
        };
        BBox {
            min_lat: tl_lat.min(br_lat),
            max_lat: tl_lat.max(br_lat),
            min_lon,
            max_lon,
        }
    }

    /// Radius (nm) of the circle around the centre that just covers the view.
    pub fn visible_radius_nm(&self) -> u32 {
        let (clat, clon) = self.unproject(0.0, 0.0);
        let r_m = distance_m(self.lat, self.lon, clat, clon) * 1.05;
        (r_m / NM_M).ceil() as u32
    }

    /// Circle (centre + radius) covering the viewport, capped to the API limit.
    pub fn query(&self) -> Query {
        // A small margin so aircraft about to enter the view are already known
        // (otherwise a zoomed-in view stays empty until a plane is inside it).
        let radius_nm = (self.visible_radius_nm() + QUERY_MARGIN_NM).clamp(1, MAX_RADIUS_NM);
        Query {
            lat: self.lat,
            lon: self.lon,
            radius_nm,
            bbox: self.bbox(),
        }
    }

    /// Tiles covering the view at the nearest integer zoom, with screen placement.
    pub fn visible_tiles(&self) -> Vec<PlacedTile> {
        let z = (self.zoom.round() as i64).clamp(0, 19) as u8;
        let scale = 2f64.powf(self.zoom - f64::from(z));
        let n = 1i64 << z;
        let (nx, ny) = mercator_norm(self.lat, self.lon);
        let (cx, cy) = (nx * n as f64 * TILE_SIZE, ny * n as f64 * TILE_SIZE);
        let half_w = self.width / 2.0 / scale;
        let half_h = self.height / 2.0 / scale;
        let tx0 = ((cx - half_w) / TILE_SIZE).floor() as i64;
        let tx1 = ((cx + half_w) / TILE_SIZE).floor() as i64;
        let ty0 = (((cy - half_h) / TILE_SIZE).floor() as i64).max(0);
        let ty1 = (((cy + half_h) / TILE_SIZE).floor() as i64).min(n - 1);
        let mut out = Vec::new();
        for ty in ty0..=ty1 {
            for tx in tx0..=tx1 {
                if out.len() >= 80 {
                    return out;
                }
                out.push(PlacedTile {
                    key: TileKey {
                        z,
                        x: tx.rem_euclid(n) as u32,
                        y: ty as u32,
                    },
                    x: (tx as f64 * TILE_SIZE - cx) * scale + self.width / 2.0,
                    y: (ty as f64 * TILE_SIZE - cy) * scale + self.height / 2.0,
                    size: TILE_SIZE * scale,
                });
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> View {
        View {
            lat: 33.4484,
            lon: -112.074,
            zoom: 8.0,
            width: 1000.0,
            height: 600.0,
        }
    }

    #[test]
    fn known_tiles() {
        assert_eq!(latlon_to_tile(0.0, 0.0, 0), TileKey { z: 0, x: 0, y: 0 });
        assert_eq!(
            latlon_to_tile(51.5074, -0.1278, 10),
            TileKey {
                z: 10,
                x: 511,
                y: 340
            }
        );
        assert_eq!(
            latlon_to_tile(-89.0, 179.9, 2),
            TileKey { z: 2, x: 3, y: 3 }
        );
    }

    #[test]
    fn mercator_roundtrip() {
        let (x, y) = mercator_norm(33.4484, -112.074);
        let (lat, lon) = mercator_norm_inv(x, y);
        assert!((lat - 33.4484).abs() < 1e-9 && (lon + 112.074).abs() < 1e-9);
    }

    #[test]
    fn url_template() {
        let u = tile_url("https://t/{z}/{x}/{y}.png", TileKey { z: 3, x: 4, y: 5 });
        assert_eq!(u, "https://t/3/4/5.png");
    }

    #[test]
    fn center_is_screen_center_and_roundtrips() {
        let v = view();
        let (x, y) = v.project(v.lat, v.lon);
        assert!((x - 500.0).abs() < 1e-6 && (y - 300.0).abs() < 1e-6);
        let (lat, lon) = v.unproject(123.0, 456.0);
        let (sx, sy) = v.project(lat, lon);
        assert!((sx - 123.0).abs() < 1e-6 && (sy - 456.0).abs() < 1e-6);
    }

    #[test]
    fn zoom_keeps_cursor_point_fixed() {
        let mut v = view();
        let before = v.unproject(800.0, 100.0);
        v.zoom_at(800.0, 100.0, 10.0);
        let after = v.unproject(800.0, 100.0);
        assert!((before.0 - after.0).abs() < 1e-7 && (before.1 - after.1).abs() < 1e-7);
        assert_eq!(v.zoom, 10.0);
    }

    #[test]
    fn pan_moves_center_opposite_to_drag() {
        let mut v = view();
        v.pan(100.0, 0.0); // drag right -> map content moves right -> centre moves west
        assert!(v.lon < -112.074);
    }

    #[test]
    fn bbox_and_radius() {
        let v = view();
        let b = v.bbox();
        assert!(b.min_lat < v.lat && v.lat < b.max_lat);
        assert!(b.min_lon < v.lon && v.lon < b.max_lon);
        let q = v.query();
        // zoom 8 at 33N: ~ 0.0138 deg/px lon -> half diagonal ~ 583 px ~ 0.0
        // sanity: radius is positive and covers the corner distance
        let corner = distance_m(v.lat, v.lon, b.max_lat, b.max_lon) / NM_M;
        assert!(
            f64::from(q.radius_nm) >= corner * 0.95,
            "{} < {}",
            q.radius_nm,
            corner
        );
        assert!(q.radius_nm <= MAX_RADIUS_NM);
    }

    #[test]
    fn bbox_across_antimeridian_covers_full_longitude() {
        let v = View {
            lat: 0.0,
            lon: 179.5,
            zoom: 6.0,
            width: 1000.0,
            height: 600.0,
        };
        let b = v.bbox();
        assert_eq!((b.min_lon, b.max_lon), (-180.0, 180.0));
        // planes just across the line still project onto the screen
        let (x, _) = v.project(0.0, -179.8);
        assert!(x > 500.0 && x < 1000.0, "{x}");
    }

    #[test]
    fn known_world_pixel_coordinates() {
        // zoom 0: the world is one 256 px tile; (0,0) is its centre
        let v = View {
            lat: 0.0,
            lon: 0.0,
            zoom: 0.0,
            width: 256.0,
            height: 256.0,
        };
        let (x, y) = v.project(0.0, 0.0);
        assert!((x - 128.0).abs() < 1e-9 && (y - 128.0).abs() < 1e-9);
        let (_, top) = v.project(MAX_LAT, 0.0);
        assert!(top.abs() < 1e-6, "{top}");
        let (lat, _) = mercator_norm_inv(0.0, 0.0);
        assert!((lat - MAX_LAT).abs() < 1e-6);
        // Tile at zoom 3 for Phoenix, checked against the OSM slippy-map formula
        assert_eq!(
            latlon_to_tile(33.4484, -112.074, 3),
            TileKey { z: 3, x: 1, y: 3 }
        );
        assert_eq!(
            latlon_to_tile(33.4484, -112.074, 12),
            TileKey {
                z: 12,
                x: 772,
                y: 1643
            }
        );
    }

    #[test]
    fn tiles_wrap_horizontally_and_are_capped() {
        let v = View {
            lat: 0.0,
            lon: 179.9,
            zoom: 3.0,
            width: 1600.0,
            height: 1200.0,
        };
        let tiles = v.visible_tiles();
        assert!(tiles.len() <= 80);
        assert!(tiles.iter().all(|t| t.key.x < 8 && t.key.y < 8));
        assert!(tiles.iter().any(|t| t.key.x == 0) && tiles.iter().any(|t| t.key.x == 7));
    }

    #[test]
    fn zoomed_in_query_has_margin_but_stays_local() {
        let v = View {
            zoom: 13.0,
            ..view()
        };
        let q = v.query();
        assert!(
            q.radius_nm > QUERY_MARGIN_NM && q.radius_nm <= QUERY_MARGIN_NM + 10,
            "{}",
            q.radius_nm
        );
    }

    #[test]
    fn circle_query_bbox() {
        let q = circle_query(33.0, -112.0, 120);
        assert_eq!(q.radius_nm, 120);
        assert!((q.bbox.max_lat - 35.0).abs() < 1e-9 && (q.bbox.min_lat - 31.0).abs() < 1e-9);
        assert!(q.bbox.min_lon < -114.0 && q.bbox.max_lon > -110.0);
        let edge = circle_query(10.0, 179.0, 200);
        assert_eq!((edge.bbox.min_lon, edge.bbox.max_lon), (-180.0, 180.0));
        assert_eq!(circle_query(0.0, 0.0, 9999).radius_nm, MAX_RADIUS_NM);
    }

    #[test]
    fn radius_is_capped_when_zoomed_out() {
        let v = View {
            zoom: 4.0,
            ..view()
        };
        assert_eq!(v.query().radius_nm, MAX_RADIUS_NM);
    }

    #[test]
    fn tile_coverage() {
        let v = view();
        let tiles = v.visible_tiles();
        assert!(!tiles.is_empty() && tiles.len() < 30);
        // every pixel of the view is covered
        let minx = tiles.iter().map(|t| t.x).fold(f64::MAX, f64::min);
        let maxx = tiles.iter().map(|t| t.x + t.size).fold(f64::MIN, f64::max);
        assert!(minx <= 0.0 && maxx >= v.width);
        // tile that contains the centre
        let c = latlon_to_tile(v.lat, v.lon, 8);
        assert!(tiles.iter().any(|t| t.key == c));
    }
}
