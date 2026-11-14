//! Persistent user settings stored in the platform config directory.

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const MIN_POLL_SECS: u64 = 2;
pub const MAX_POLL_SECS: u64 = 300;
pub const DEFAULT_TILE_URL: &str = "https://tile.openstreetmap.org/{z}/{x}/{y}.png";
/// Identifies the app (name, version, contact URL) as the OSM tile policy and the
/// adsb.lol / adsb.fi APIs ask for. Library-default User-Agents get blocked.
pub const USER_AGENT: &str = concat!(
    "skytrace/",
    env!("CARGO_PKG_VERSION"),
    " (desktop flight tracker; non-commercial; +https://github.com/legendarylolo318-cloud/skytrace)"
);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Units {
    #[default]
    Imperial,
    Metric,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
    Midnight,
    Sepia,
    HighContrast,
}

impl Theme {
    pub const ALL: [Theme; 6] = [
        Theme::System,
        Theme::Light,
        Theme::Dark,
        Theme::Midnight,
        Theme::Sepia,
        Theme::HighContrast,
    ];

    /// Index in the settings combo box (same order as `ALL`).
    pub fn index(self) -> i32 {
        Self::ALL.iter().position(|t| *t == self).unwrap_or(0) as i32
    }

    pub fn from_index(i: i32) -> Self {
        usize::try_from(i)
            .ok()
            .and_then(|i| Self::ALL.get(i).copied())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SourceChoice {
    #[default]
    Auto,
    AdsbLol,
    /// airplanes.live closed its public API; old configs map to its replacement.
    #[serde(alias = "airplanes_live")]
    AdsbFi,
    OpenSky,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub home_lat: f64,
    pub home_lon: f64,
    pub home_zoom: f64,
    pub poll_interval_secs: u64,
    pub units: Units,
    pub theme: Theme,
    pub source: SourceChoice,
    pub opensky_enabled: bool,
    pub tile_url: String,
    /// Wide area (nm around the view) loaded on start and refreshed every 30 s
    /// so the map is not empty and zooming in has planes ready. 0 = visible area only.
    pub preload_radius_nm: u32,
    /// Poll the visible area more often the further the map is zoomed in.
    pub adaptive_refresh: bool,
}

/// Largest preload radius (the ADS-B APIs' maximum query radius).
pub const MAX_PRELOAD_NM: u32 = 250;
pub const DEFAULT_PRELOAD_NM: u32 = 250;

impl Default for Config {
    fn default() -> Self {
        Self {
            // Phoenix, AZ
            home_lat: 33.4484,
            home_lon: -112.0740,
            home_zoom: 8.0,
            poll_interval_secs: 5,
            units: Units::default(),
            theme: Theme::default(),
            source: SourceChoice::default(),
            opensky_enabled: false,
            tile_url: DEFAULT_TILE_URL.to_string(),
            preload_radius_nm: DEFAULT_PRELOAD_NM,
            adaptive_refresh: true,
        }
    }
}

impl Config {
    /// Clamp values into their valid ranges.
    pub fn sanitize(&mut self) {
        let d = Self::default();
        self.poll_interval_secs = self.poll_interval_secs.clamp(MIN_POLL_SECS, MAX_POLL_SECS);
        // `clamp` passes NaN through, which would poison every projection.
        let finite = |v: f64, def: f64| if v.is_finite() { v } else { def };
        self.home_lat = finite(self.home_lat, d.home_lat).clamp(-85.0, 85.0);
        self.home_lon = finite(self.home_lon, d.home_lon).clamp(-180.0, 180.0);
        self.home_zoom = finite(self.home_zoom, d.home_zoom).clamp(3.0, 15.0);
        self.preload_radius_nm = self.preload_radius_nm.min(MAX_PRELOAD_NM);
        if !valid_tile_url(&self.tile_url) {
            self.tile_url = d.tile_url;
        }
    }

    fn dirs() -> Option<ProjectDirs> {
        ProjectDirs::from("", "", "skytrace")
    }

    pub fn path() -> Option<PathBuf> {
        Self::dirs().map(|d| d.config_dir().join("config.toml"))
    }

    pub fn cache_dir() -> PathBuf {
        Self::dirs()
            .map(|d| d.cache_dir().to_path_buf())
            .unwrap_or_else(|| std::env::temp_dir().join("skytrace-cache"))
    }

    /// Load the config; a missing or corrupt file yields defaults.
    pub fn load() -> Self {
        Self::path().map_or_else(Self::default, |p| Self::load_from(&p))
    }

    pub fn load_from(path: &Path) -> Self {
        let mut cfg = std::fs::read_to_string(path)
            .ok()
            .and_then(|s| toml::from_str::<Config>(&s).ok())
            .unwrap_or_default();
        cfg.sanitize();
        cfg
    }

    /// Write via a temp file + rename so a crash never leaves a half-written config.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, toml::to_string_pretty(self)?)
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
    }
}

/// A tile URL template must be http(s) and contain all of `{z}`, `{x}` and `{y}`.
pub fn valid_tile_url(url: &str) -> bool {
    let u = url.trim();
    (u.starts_with("https://") || u.starts_with("http://"))
        && ["{z}", "{x}", "{y}"].iter().all(|p| u.contains(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_defaults() {
        let cfg = Config::default();
        let s = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(cfg, back);
        let partial: Config = toml::from_str("poll_interval_secs = 1").unwrap();
        assert_eq!(partial.tile_url, DEFAULT_TILE_URL);
    }

    #[test]
    fn sanitize_clamps_poll() {
        let mut c = Config {
            poll_interval_secs: 0,
            ..Config::default()
        };
        c.sanitize();
        assert_eq!(c.poll_interval_secs, MIN_POLL_SECS);
        c.poll_interval_secs = 100_000;
        c.sanitize();
        assert_eq!(c.poll_interval_secs, MAX_POLL_SECS);
    }

    #[test]
    fn sanitize_rejects_nan_and_bad_tile_urls() {
        let mut c = Config {
            home_lat: f64::NAN,
            home_zoom: f64::INFINITY,
            tile_url: "ftp://x/{z}".into(),
            ..Config::default()
        };
        c.sanitize();
        assert_eq!(c.home_lat, Config::default().home_lat);
        assert_eq!(c.home_zoom, Config::default().home_zoom);
        assert_eq!(c.tile_url, DEFAULT_TILE_URL);
        assert!(valid_tile_url("https://a.tile.example.org/{z}/{x}/{y}.png"));
        assert!(!valid_tile_url("https://example.org/{z}/{x}.png"));
    }

    fn tmp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("skytrace-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn load_missing_file_gives_defaults() {
        let d = tmp_dir("missing");
        assert_eq!(Config::load_from(&d.join("nope.toml")), Config::default());
    }

    #[test]
    fn load_corrupt_file_gives_defaults() {
        let d = tmp_dir("corrupt");
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("config.toml");
        for junk in [
            "this is = = not toml",
            "\0\0\0",
            "units = 42",
            "poll_interval_secs = \"x\"",
        ] {
            std::fs::write(&p, junk).unwrap();
            assert_eq!(Config::load_from(&p), Config::default(), "{junk}");
        }
        std::fs::write(&p, [0xffu8, 0xfe, 0x00]).unwrap(); // not UTF-8
        assert_eq!(Config::load_from(&p), Config::default());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn save_then_load_roundtrips_and_creates_dirs() {
        let d = tmp_dir("save");
        let p = d.join("nested/dir/config.toml");
        let c = Config {
            home_lat: 51.5,
            home_lon: -0.12,
            poll_interval_secs: 9,
            units: Units::Metric,
            theme: Theme::Dark,
            source: SourceChoice::AdsbFi,
            opensky_enabled: true,
            ..Config::default()
        };
        c.save_to(&p).unwrap();
        assert_eq!(Config::load_from(&p), c);
        assert!(!p.with_extension("toml.tmp").exists());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn legacy_values_load() {
        let c: Config = toml::from_str("source = \"airplanes_live\"\nunits = \"metric\"").unwrap();
        assert_eq!(c.source, SourceChoice::AdsbFi);
        assert_eq!(c.units, Units::Metric);
    }

    #[test]
    fn preload_defaults_and_clamp() {
        let c: Config = toml::from_str("poll_interval_secs = 5").unwrap();
        assert_eq!(
            (c.preload_radius_nm, c.adaptive_refresh),
            (DEFAULT_PRELOAD_NM, true)
        );
        let mut c = Config {
            preload_radius_nm: 9000,
            ..Config::default()
        };
        c.sanitize();
        assert_eq!(c.preload_radius_nm, MAX_PRELOAD_NM);
        let off: Config =
            toml::from_str("preload_radius_nm = 0\nadaptive_refresh = false").unwrap();
        assert_eq!((off.preload_radius_nm, off.adaptive_refresh), (0, false));
    }

    #[test]
    fn theme_index_roundtrip() {
        for t in Theme::ALL {
            assert_eq!(Theme::from_index(t.index()), t);
        }
        assert_eq!(Theme::from_index(99), Theme::System);
        assert_eq!(Theme::from_index(-1), Theme::System);
        let c: Config = toml::from_str("theme = \"high_contrast\"").unwrap();
        assert_eq!(c.theme, Theme::HighContrast);
    }

    #[test]
    fn user_agent_identifies_app() {
        assert!(USER_AGENT.starts_with(concat!("skytrace/", env!("CARGO_PKG_VERSION"))));
        assert!(USER_AGENT.contains("+https://github.com/legendarylolo318-cloud/skytrace"));
    }
}
