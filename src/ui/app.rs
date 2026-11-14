//! Application state, Slint callbacks and rendering of the model into the UI.
//!
//! All state lives on the UI thread (thread-local); background tasks send
//! results back through `slint::invoke_from_event_loop`.

use super::{KV, MainWindow, PlaneItem, TileItem, format as fmt};
use crate::config::{Config, MIN_POLL_SECS, SourceChoice, Theme, Units, valid_tile_url};
use crate::map::coast::{Basemap, polyline};
use crate::map::projection::{MAX_ZOOM, PlacedTile, TileKey, View};
use crate::map::tiles::{Decoded, TileService};
use crate::model::aircraft::{Tracked, Trail};
use crate::model::filter::Filter;
use crate::model::geo::{NM_M, distance_m};
use crate::model::meta::{AircraftInfo, RouteInfo};
use crate::sources::adsbdb::AdsbDb;
use crate::sources::poller::{self, PollEvent, PollParams};
use anyhow::Result;
use lru::LruCache;
use slint::{
    Color, ComponentHandle, Image, ModelRc, SharedString, Timer, TimerMode, VecModel, Weak,
};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Notify, watch};

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Run `f` on the app state. Returns `None` if the state is unavailable or already borrowed.
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| {
        let mut guard = a.try_borrow_mut().ok()?;
        guard.as_mut().map(f)
    })
}

const GROUND_COLOR: [u8; 3] = [0x8a, 0x94, 0x9e];
const HIT_RADIUS: f64 = 16.0;
/// Decoded 256×256 RGBA tiles kept in memory (~256 KiB each → ~30 MiB max).
const TILE_CACHE: usize = 120;
const STALE_AFTER: Duration = Duration::from_secs(90);
/// UI housekeeping / dead-reckoning step. Only rows that actually moved are
/// pushed to Slint, so an idle map causes no redraw beyond the stats line.
const TICK: Duration = Duration::from_secs(1);
/// Screen movement (px) below which a plane's row is not updated.
const MOVE_EPS: f32 = 0.75;
const TILE_RETRY: Duration = Duration::from_secs(30);

struct Drag {
    last: (f64, f64),
    total: f64,
}

#[derive(Default)]
struct Meta {
    hex: String,
    loaded: bool,
    info: Option<AircraftInfo>,
    route: Option<RouteInfo>,
}

struct Status {
    last_update: Option<Instant>,
    source: String,
    ok: bool,
    message: String,
    retry_at: Option<Instant>,
}

struct App {
    win: Weak<MainWindow>,
    rt: tokio::runtime::Handle,
    cfg: Config,
    config_path: Option<PathBuf>,
    view: View,
    basemap: Basemap,
    planes: HashMap<String, Tracked>,
    trails: HashMap<String, Trail>,
    filter: Filter,
    selected: Option<String>,
    snapshot: Option<Tracked>,
    meta: Meta,
    follow: bool,
    hover: Option<String>,
    hits: Vec<(String, f64, f64)>,
    drag: Option<Drag>,
    status: Status,
    emergencies: HashSet<String>,
    system_scheme: slint::language::ColorScheme,
    last_planes: Vec<PlaneItem>,
    last_rows: Vec<KV>,
    last_tiles: Vec<TileKey>,
    last_trail: String,
    basemap_visible: bool,
    tiles_missing: bool,
    alert: Option<(String, Instant)>,
    alert_hex: Option<String>,
    // tiles
    tile_service: TileService,
    tile_cache: LruCache<TileKey, Image>,
    tile_failed: HashMap<TileKey, Instant>,
    tile_gen: u32,
    // polling
    params_tx: watch::Sender<PollParams>,
    refresh: Arc<Notify>,
    last_query: Option<crate::sources::Query>,
    adsbdb: Option<Arc<AdsbDb>>,
    // models
    tile_model: Rc<VecModel<TileItem>>,
    plane_model: Rc<VecModel<PlaneItem>>,
    row_model: Rc<VecModel<KV>>,
}

fn rgb(c: [u8; 3]) -> Color {
    Color::from_rgb_u8(c[0], c[1], c[2])
}

fn kv(k: &str, v: impl Into<String>) -> KV {
    KV {
        k: k.into(),
        v: SharedString::from(v.into()),
    }
}

/// The colour scheme the std-widgets actually render with. Slint reports
/// `Unknown` when the platform decides, but the widgets may still be dark; our
/// own `Theme` global must agree with them or text becomes unreadable, so infer
/// it from the palette background in that case.
fn detect_scheme(win: &MainWindow) -> slint::language::ColorScheme {
    use slint::language::ColorScheme;
    let palette = win.global::<super::Palette>();
    match palette.get_color_scheme() {
        ColorScheme::Unknown => {
            let c = palette.get_background().color();
            let luma = 0.2126 * f32::from(c.red())
                + 0.7152 * f32::from(c.green())
                + 0.0722 * f32::from(c.blue());
            if luma < 128.0 {
                ColorScheme::Dark
            } else {
                ColorScheme::Light
            }
        }
        known => known,
    }
}

fn opt_f(v: Option<f64>, f: impl Fn(f64) -> String) -> String {
    v.map_or_else(|| "—".to_string(), f)
}

/// Where the app reads/writes its state and whether it talks to the network.
struct Env {
    cfg: Config,
    config_path: Option<PathBuf>,
    cache_dir: PathBuf,
    /// Start the flight poller and adsbdb lookups (off in UI tests).
    online: bool,
}

pub fn run(rt: &tokio::runtime::Runtime, client: reqwest::Client) -> Result<()> {
    let env = Env {
        cfg: Config::load(),
        config_path: Config::path(),
        cache_dir: Config::cache_dir(),
        online: true,
    };
    let win = build(rt.handle(), client, env)?;
    // Wayland app_id / X11 WM_CLASS so the compositor matches skytrace.desktop.
    // Needs an initialised platform (after `MainWindow::new`) and must precede `run`.
    let _ = slint::set_xdg_app_id("skytrace");
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, TICK, || {
        with_app(|a| a.tick());
    });
    win.run()?;
    drop(timer);
    APP.with(|a| a.borrow_mut().take());
    Ok(())
}

/// Create the window and the app state and wire all callbacks (no event loop).
fn build(rt: &tokio::runtime::Handle, client: reqwest::Client, env: Env) -> Result<MainWindow> {
    let Env {
        cfg,
        config_path,
        cache_dir,
        online,
    } = env;
    let win = MainWindow::new()?;
    let view = View {
        lat: cfg.home_lat,
        lon: cfg.home_lon,
        zoom: cfg.home_zoom,
        width: 1000.0,
        height: 700.0,
    };

    let (params_tx, params_rx) = watch::channel(PollParams {
        query: view.query(),
        interval: Duration::from_secs(cfg.poll_interval_secs),
        source: cfg.source,
        opensky: cfg.opensky_enabled,
    });
    let refresh = Arc::new(Notify::new());
    if online {
        poller::spawn(
            rt,
            client.clone(),
            params_rx,
            refresh.clone(),
            Arc::new(|ev: PollEvent| {
                let _ = slint::invoke_from_event_loop(move || {
                    with_app(|a| a.on_poll(ev));
                });
            }),
        );
    }

    let tile_service = TileService::new(
        rt.clone(),
        client.clone(),
        cache_dir.clone(),
        Arc::new(|d: Decoded| {
            let _ = slint::invoke_from_event_loop(move || {
                with_app(|a| a.on_tile(d));
            });
        }),
    );

    let tile_model = Rc::new(VecModel::default());
    let plane_model = Rc::new(VecModel::default());
    let row_model = Rc::new(VecModel::default());
    win.set_tiles(ModelRc::from(tile_model.clone()));
    win.set_planes(ModelRc::from(plane_model.clone()));
    win.set_panel_rows(ModelRc::from(row_model.clone()));

    let app = App {
        win: win.as_weak(),
        rt: rt.clone(),
        cfg,
        config_path,
        view,
        basemap: Basemap::load(),
        planes: HashMap::new(),
        trails: HashMap::new(),
        filter: Filter::default(),
        selected: None,
        snapshot: None,
        meta: Meta::default(),
        follow: false,
        hover: None,
        hits: Vec::new(),
        drag: None,
        status: Status {
            last_update: None,
            source: "—".into(),
            ok: false,
            message: "Connecting…".into(),
            retry_at: None,
        },
        emergencies: HashSet::new(),
        system_scheme: detect_scheme(&win),
        last_planes: Vec::new(),
        last_rows: Vec::new(),
        last_tiles: Vec::new(),
        last_trail: String::new(),
        basemap_visible: true,
        tiles_missing: true,
        alert: None,
        alert_hex: None,
        tile_service,
        tile_cache: LruCache::new(NonZeroUsize::new(TILE_CACHE).unwrap_or(NonZeroUsize::MIN)),
        tile_failed: HashMap::new(),
        tile_gen: 0,
        params_tx,
        refresh,
        last_query: None,
        adsbdb: online.then(|| Arc::new(AdsbDb::new(client, cache_dir))),
        tile_model,
        plane_model,
        row_model,
    };
    APP.with(|a| *a.borrow_mut() = Some(app));
    wire(&win);
    with_app(|a| a.init_ui());
    Ok(win)
}

fn wire(win: &MainWindow) {
    win.on_map_resized(|w, h| {
        with_app(|a| a.resized(f64::from(w), f64::from(h)));
    });
    win.on_map_down(|x, y| {
        with_app(|a| {
            a.drag = Some(Drag {
                last: (f64::from(x), f64::from(y)),
                total: 0.0,
            })
        });
    });
    win.on_map_move(|x, y| {
        with_app(|a| a.pointer_moved(f64::from(x), f64::from(y)));
    });
    win.on_map_up(|x, y| {
        with_app(|a| a.pointer_up(f64::from(x), f64::from(y)));
    });
    win.on_map_scroll(|x, y, dy| {
        with_app(|a| {
            let dz = (f64::from(dy) / 240.0).clamp(-0.6, 0.6);
            a.zoom_by(f64::from(x), f64::from(y), dz);
        });
    });
    win.on_zoom_in(|| {
        with_app(|a| {
            let (cx, cy) = (a.view.width / 2.0, a.view.height / 2.0);
            a.zoom_by(cx, cy, 1.0);
        });
    });
    win.on_zoom_out(|| {
        with_app(|a| {
            let (cx, cy) = (a.view.width / 2.0, a.view.height / 2.0);
            a.zoom_by(cx, cy, -1.0);
        });
    });
    win.on_go_home(|| {
        with_app(|a| {
            a.set_follow(false);
            a.view.lat = a.cfg.home_lat;
            a.view.lon = a.cfg.home_lon;
            a.view.zoom = a.cfg.home_zoom;
            a.view_changed();
        });
    });
    win.on_search_edited(|t| {
        with_app(|a| {
            a.filter.search = t.to_string();
            a.render_planes();
        });
    });
    win.on_search_accepted(|_| {
        with_app(|a| a.jump_to_first_match());
    });
    win.on_filters_changed(|| {
        with_app(|a| a.read_filters());
    });
    win.on_follow_toggled(|on| {
        with_app(|a| a.set_follow(on && a.selected.is_some()));
    });
    win.on_close_panel(|| {
        with_app(|a| a.deselect());
    });
    win.on_alert_clicked(|| {
        with_app(|a| {
            if let Some(hex) = a.alert_hex.clone() {
                a.select(&hex, true);
            }
        });
    });
    win.on_alert_dismissed(|| {
        with_app(|a| {
            a.alert = None;
            a.apply_alert();
        });
    });
    win.on_use_center(|| {
        with_app(|a| {
            if let Some(win) = a.win.upgrade() {
                win.set_s_lat(format!("{:.5}", a.view.lat).into());
                win.set_s_lon(format!("{:.5}", a.view.lon).into());
            }
        });
    });
    win.on_open_settings(|| {
        with_app(|a| {
            // Discard edits from a previously cancelled dialog: show the saved values.
            a.fill_settings_form();
            if let Some(win) = a.win.upgrade() {
                win.set_settings_open(true);
            }
        });
    });
    win.on_save_settings(|| {
        with_app(|a| a.save_settings());
    });
}

impl App {
    // ------------------------------------------------------------ setup

    fn init_ui(&mut self) {
        self.apply_attribution();
        self.apply_units_ui();
        self.fill_settings_form();
        self.apply_theme();
        self.update_status();
        self.view_changed();
    }

    fn apply_units_ui(&self) {
        let Some(win) = self.win.upgrade() else {
            return;
        };
        let labels: Vec<SharedString> = fmt::legend_labels(self.cfg.units)
            .into_iter()
            .map(Into::into)
            .collect();
        win.set_legend_labels(ModelRc::from(Rc::new(VecModel::from(labels))));
        win.set_legend_title(format!("Altitude ({})", fmt::alt_unit(self.cfg.units)).into());
        win.set_alt_unit(fmt::alt_unit(self.cfg.units).into());
    }

    /// Visible credits required by the OSM tile policy, the ODbL (adsb.lol),
    /// adsb.fi / OpenSky terms, adsbdb and Natural Earth.
    fn apply_attribution(&self) {
        let Some(win) = self.win.upgrade() else {
            return;
        };
        let tiles = if self.cfg.tile_url.contains("openstreetmap.org") {
            "Map © OpenStreetMap contributors".to_string()
        } else {
            let host = self.cfg.tile_url.split('/').nth(2).unwrap_or("custom");
            format!("Tiles: {host}")
        };
        let flights = match self.status.source.as_str() {
            "adsb.lol" => "Flights: adsb.lol (ODbL)",
            "adsb.fi" => "Flights: adsb.fi",
            "OpenSky" => "Flights: The OpenSky Network",
            _ => "Flights: adsb.lol (ODbL) / adsb.fi",
        };
        win.set_attribution(
            format!("{tiles} · {flights} · Aircraft info: adsbdb.com · Natural Earth").into(),
        );
    }

    fn apply_theme(&self) {
        use slint::language::ColorScheme;
        let Some(win) = self.win.upgrade() else {
            return;
        };
        let (scheme, variant) = match self.cfg.theme {
            Theme::System if self.system_scheme == ColorScheme::Dark => (ColorScheme::Dark, 1),
            Theme::System | Theme::Light => (ColorScheme::Light, 0),
            Theme::Dark => (ColorScheme::Dark, 1),
            Theme::Midnight => (ColorScheme::Dark, 2),
            Theme::Sepia => (ColorScheme::Light, 3),
            Theme::HighContrast => (ColorScheme::Light, 4),
        };
        // Widgets and our own colours must always agree on light vs dark.
        win.global::<super::Palette>().set_color_scheme(scheme);
        win.global::<super::AppTheme>().set_variant(variant);
    }

    fn fill_settings_form(&self) {
        let Some(win) = self.win.upgrade() else {
            return;
        };
        win.set_s_poll(self.cfg.poll_interval_secs as f32);
        win.set_s_units(match self.cfg.units {
            Units::Imperial => 0,
            Units::Metric => 1,
        });
        win.set_s_theme(self.cfg.theme.index());
        win.set_s_source(match self.cfg.source {
            SourceChoice::Auto => 0,
            SourceChoice::AdsbLol => 1,
            SourceChoice::AdsbFi => 2,
            SourceChoice::OpenSky => 3,
        });
        win.set_s_opensky(self.cfg.opensky_enabled);
        win.set_s_tile_url(self.cfg.tile_url.clone().into());
        win.set_s_lat(format!("{:.5}", self.cfg.home_lat).into());
        win.set_s_lon(format!("{:.5}", self.cfg.home_lon).into());
        win.set_s_error("".into());
    }

    fn save_settings(&mut self) {
        let Some(win) = self.win.upgrade() else {
            return;
        };
        let (Ok(lat), Ok(lon)) = (
            win.get_s_lat().trim().parse::<f64>(),
            win.get_s_lon().trim().parse::<f64>(),
        ) else {
            win.set_s_error("Home latitude/longitude must be numbers.".into());
            return;
        };
        if !(-85.0..=85.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
            win.set_s_error("Latitude must be within ±85° and longitude within ±180°.".into());
            return;
        }
        let tile_url = win.get_s_tile_url().trim().to_string();
        if !valid_tile_url(&tile_url) {
            win.set_s_error("Tile URL must be http(s) and contain {z}, {x} and {y}.".into());
            return;
        }
        let old_tiles = self.cfg.tile_url.clone();
        let old_units = self.cfg.units;
        self.cfg.home_lat = lat;
        self.cfg.home_lon = lon;
        self.cfg.poll_interval_secs = (win.get_s_poll().round() as u64).max(MIN_POLL_SECS);
        self.cfg.units = if win.get_s_units() == 1 {
            Units::Metric
        } else {
            Units::Imperial
        };
        self.cfg.theme = Theme::from_index(win.get_s_theme());
        self.cfg.source = match win.get_s_source() {
            1 => SourceChoice::AdsbLol,
            2 => SourceChoice::AdsbFi,
            3 => SourceChoice::OpenSky,
            _ => SourceChoice::Auto,
        };
        self.cfg.opensky_enabled = win.get_s_opensky();
        self.cfg.tile_url = tile_url;
        self.cfg.sanitize();
        let saved = match &self.config_path {
            Some(p) => self.cfg.save_to(p),
            None => Err(anyhow::anyhow!("no platform config directory")),
        };
        if let Err(e) = saved {
            win.set_s_error(format!("Could not save config: {e}").into());
            return;
        }
        win.set_settings_open(false);
        self.apply_units_ui();
        self.apply_theme();
        if old_units != self.cfg.units {
            // filter inputs are typed in display units: re-interpret them
            self.read_filters();
        }
        if old_tiles != self.cfg.tile_url {
            self.tile_gen += 1;
            self.tile_cache.clear();
            self.tile_failed.clear();
            self.last_tiles.clear();
            self.apply_attribution();
        }
        self.push_params(true);
        self.render_all();
    }

    // ------------------------------------------------------- view / polling

    fn resized(&mut self, w: f64, h: f64) {
        if w < 1.0 || h < 1.0 {
            return;
        }
        self.view.width = w;
        self.view.height = h;
        self.view_changed();
    }

    /// After any viewport change: redraw and tell the poller about the new area.
    fn view_changed(&mut self) {
        self.render_all();
        self.maybe_refresh();
    }

    fn push_params(&mut self, notify: bool) {
        let q = self.view.query();
        self.last_query = Some(q.clone());
        self.params_tx.send_replace(PollParams {
            query: q,
            interval: Duration::from_secs(self.cfg.poll_interval_secs),
            source: self.cfg.source,
            opensky: self.cfg.opensky_enabled,
        });
        if notify {
            self.refresh.notify_one();
        }
    }

    /// Always keeps the poller's query current; triggers an immediate poll
    /// only when the view moved far enough to leave the last queried circle.
    fn maybe_refresh(&mut self) {
        let q = self.view.query();
        let notify = match &self.last_query {
            None => true,
            Some(last) => {
                let moved = distance_m(last.lat, last.lon, q.lat, q.lon) / NM_M;
                let ratio = f64::from(q.radius_nm) / f64::from(last.radius_nm.max(1));
                moved > 0.25 * f64::from(last.radius_nm) || !(0.7..=1.4).contains(&ratio)
            }
        };
        if notify {
            self.push_params(true);
        } else {
            self.params_tx.send_modify(|p| p.query = q);
        }
    }

    fn zoom_by(&mut self, x: f64, y: f64, dz: f64) {
        let z = (self.view.zoom + dz).clamp(crate::map::projection::MIN_ZOOM, MAX_ZOOM);
        self.view.zoom_at(x, y, z);
        self.view_changed();
    }

    fn set_follow(&mut self, on: bool) {
        self.follow = on;
        if let Some(win) = self.win.upgrade() {
            win.set_follow(on);
        }
    }

    // ----------------------------------------------------------- pointer

    fn pointer_moved(&mut self, x: f64, y: f64) {
        if let Some(d) = &mut self.drag {
            let (dx, dy) = (x - d.last.0, y - d.last.1);
            d.last = (x, y);
            d.total += dx.abs() + dy.abs();
            if d.total > 4.0 {
                if self.follow {
                    self.set_follow(false);
                }
                self.view.pan(dx, dy);
                self.render_all();
            }
            return;
        }
        let hit = self.hit(x, y);
        if hit != self.hover {
            self.hover = hit;
            self.render_planes();
        }
    }

    fn pointer_up(&mut self, x: f64, y: f64) {
        let Some(d) = self.drag.take() else { return };
        if d.total <= 4.0 {
            if let Some(hex) = self.hit(x, y) {
                self.select(&hex, false);
            }
        } else {
            self.maybe_refresh();
        }
    }

    fn hit(&self, x: f64, y: f64) -> Option<String> {
        self.hits
            .iter()
            .map(|(h, px, py)| (h, (px - x).powi(2) + (py - y).powi(2)))
            .filter(|(_, d2)| *d2 <= HIT_RADIUS * HIT_RADIUS)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(h, _)| h.clone())
    }

    // --------------------------------------------------------- selection

    fn select(&mut self, hex: &str, center: bool) {
        let Some(t) = self.planes.get(hex).cloned() else {
            return;
        };
        self.selected = Some(hex.to_string());
        self.snapshot = Some(t.clone());
        self.set_follow(false);
        self.meta = Meta {
            hex: hex.to_string(),
            ..Meta::default()
        };
        if center {
            let (lat, lon) = t.position_now(Instant::now());
            self.view.lat = lat;
            self.view.lon = lon;
            self.view.zoom = self.view.zoom.max(8.0);
        }
        let (hex, callsign) = (hex.to_string(), t.ac.callsign.clone());
        let Some(db) = self.adsbdb.clone() else {
            self.meta.loaded = true;
            self.view_changed();
            return;
        };
        self.rt.spawn(async move {
            let info = db.aircraft(&hex).await;
            let route = match callsign {
                Some(c) => db.route(&c).await,
                None => None,
            };
            let _ = slint::invoke_from_event_loop(move || {
                with_app(|a| a.on_meta(&hex, info, route));
            });
        });
        self.view_changed();
    }

    fn deselect(&mut self) {
        self.selected = None;
        self.snapshot = None;
        self.set_follow(false);
        self.render_all();
    }

    fn on_meta(&mut self, hex: &str, info: Option<AircraftInfo>, route: Option<RouteInfo>) {
        if self.meta.hex == hex {
            self.meta.loaded = true;
            self.meta.info = info;
            self.meta.route = route;
            self.render_planes();
        }
    }

    fn jump_to_first_match(&mut self) {
        if self.filter.search.trim().is_empty() {
            return;
        }
        let now = Instant::now();
        let mut matches: Vec<_> = self
            .planes
            .iter()
            .filter(|(_, t)| self.filter.matches(&t.ac))
            .map(|(h, t)| {
                (
                    h.clone(),
                    t.ac.display_name().to_string(),
                    t.position_now(now),
                )
            })
            .collect();
        matches.sort_by(|a, b| a.1.cmp(&b.1));
        if let Some((hex, _, _)) = matches.first() {
            let hex = hex.clone();
            self.select(&hex, true);
        } else {
            self.status.message = "No matching aircraft in the current area".into();
            self.update_status();
        }
    }

    fn read_filters(&mut self) {
        let Some(win) = self.win.upgrade() else {
            return;
        };
        let u = self.cfg.units;
        let parse = |s: SharedString| {
            s.trim()
                .parse::<f64>()
                .ok()
                .map(|v| fmt::altitude_input_to_ft(v, u))
        };
        self.filter.min_alt_ft = parse(win.get_f_min_alt());
        self.filter.max_alt_ft = parse(win.get_f_max_alt());
        self.filter.hide_ground = win.get_f_ground();
        self.filter.military_only = win.get_f_mil();
        self.filter.type_code = win.get_f_type().to_string();
        self.render_planes();
    }

    // ------------------------------------------------------ async results

    fn on_tile(&mut self, d: Decoded) {
        if d.generation != self.tile_gen {
            return;
        }
        match d.pixels {
            Some(buf) => {
                self.tile_cache.put(d.key, Image::from_rgba8(buf));
                self.tile_failed.remove(&d.key);
                self.render_tiles();
            }
            // Skipped because it left the view while queued: not an error. If it
            // is visible again by now, request it right away.
            None if d.skipped => {
                if self.view.visible_tiles().iter().any(|t| t.key == d.key) {
                    self.render_tiles();
                }
            }
            None => {
                let now = Instant::now();
                // bounded: forget failures old enough to be retried anyway
                self.tile_failed
                    .retain(|_, at| now.duration_since(*at) < TILE_RETRY);
                self.tile_failed.insert(d.key, now);
            }
        }
    }

    fn on_poll(&mut self, ev: PollEvent) {
        let now = Instant::now();
        match ev {
            PollEvent::Update {
                aircraft,
                source,
                fallback,
            } => {
                let mut seen = HashSet::with_capacity(aircraft.len());
                for ac in aircraft {
                    let (lat, lon) = ac.predicted(0.0);
                    self.trails
                        .entry(ac.hex.clone())
                        .or_default()
                        .push(lat, lon, now);
                    if ac.emergency && self.emergencies.insert(ac.hex.clone()) {
                        let code = ac.squawk.as_deref().unwrap_or("emergency");
                        self.alert = Some((
                            format!("⚠ EMERGENCY {code}: {}", ac.display_name()),
                            now + Duration::from_secs(30),
                        ));
                        self.alert_hex = Some(ac.hex.clone());
                    }
                    seen.insert(ac.hex.clone());
                    self.planes
                        .insert(ac.hex.clone(), Tracked { ac, received: now });
                }
                self.planes.retain(|k, _| seen.contains(k));
                self.trails.retain(|_, t| {
                    t.last_seen
                        .is_some_and(|s| now.duration_since(s) < Duration::from_secs(600))
                });
                // remember alerted emergencies only while we still know the aircraft
                let trails = &self.trails;
                self.emergencies.retain(|h| trails.contains_key(h));
                let source_changed = self.status.source != source;
                self.status = Status {
                    last_update: Some(now),
                    source: source.to_string(),
                    ok: true,
                    message: if fallback {
                        "Connected (fallback source)".into()
                    } else {
                        "Connected".into()
                    },
                    retry_at: None,
                };
                if source_changed {
                    self.apply_attribution();
                }
                self.apply_alert();
            }
            PollEvent::Failure { message, retry_in } => {
                self.status.ok = false;
                self.status.message = message;
                self.status.retry_at = Some(now + retry_in);
            }
        }
        self.render_planes();
    }

    fn tick(&mut self) {
        let now = Instant::now();
        // Retry tiles whose earlier failure has expired without waiting for the
        // user to move the map (requests are de-duplicated by the tile service).
        if self.tiles_missing
            && self.view.visible_tiles().iter().any(|t| {
                !self.tile_cache.contains(&t.key)
                    && self
                        .tile_failed
                        .get(&t.key)
                        .is_none_or(|at| now.duration_since(*at) > TILE_RETRY)
            })
        {
            self.render_tiles();
        }
        self.planes
            .retain(|_, t| now.duration_since(t.received) < STALE_AFTER);
        if self.alert.as_ref().is_some_and(|(_, until)| now >= *until) {
            self.alert = None;
            self.apply_alert();
        }
        if self.follow
            && let Some(t) = self.selected.as_ref().and_then(|h| self.planes.get(h))
        {
            let (lat, lon) = t.position_now(now);
            self.view.lat = lat;
            self.view.lon = lon;
            self.render_paths();
            self.render_tiles();
            self.maybe_refresh();
        }
        self.render_planes();
    }

    // --------------------------------------------------------- rendering

    fn render_all(&mut self) {
        self.render_paths();
        self.render_tiles();
        self.render_planes();
    }

    /// Offline basemap. Skipped while tiles cover the whole view: those vector
    /// paths would otherwise be rebuilt and re-tessellated under opaque tiles.
    fn render_paths(&self) {
        let Some(win) = self.win.upgrade() else {
            return;
        };
        if !self.basemap_visible {
            return;
        }
        win.set_coast_path(self.basemap.coast_path(&self.view).into());
        win.set_border_path(self.basemap.border_path(&self.view).into());
    }

    fn render_tiles(&mut self) {
        let placed = self.view.visible_tiles();
        self.tile_service.set_wanted(placed.iter().map(|t| t.key));
        let now = Instant::now();
        let mut items = Vec::with_capacity(placed.len());
        for t in &placed {
            if let Some(img) = self.tile_cache.get(&t.key) {
                items.push(TileItem {
                    x: t.x as f32,
                    y: t.y as f32,
                    size: t.size as f32,
                    image: img.clone(),
                });
            } else if self
                .tile_failed
                .get(&t.key)
                .is_none_or(|at| now.duration_since(*at) > TILE_RETRY)
            {
                self.tile_service
                    .request(self.tile_gen, &self.cfg.tile_url, t.key);
            }
        }
        let covered = !placed.is_empty() && items.len() == placed.len();
        self.tiles_missing = !covered;
        self.push_tiles(items, &placed);
        if covered == self.basemap_visible {
            self.basemap_visible = !covered;
            if let Some(win) = self.win.upgrade() {
                win.set_basemap_visible(self.basemap_visible);
            }
            self.render_paths();
        }
    }

    /// Update the tile model in place where possible: `set_vec` makes Slint
    /// destroy and recreate every tile element, which happened on each pan
    /// event and for every single tile that finished loading.
    fn push_tiles(&mut self, items: Vec<TileItem>, placed: &[PlacedTile]) {
        let keys: Vec<TileKey> = placed
            .iter()
            .filter(|t| self.tile_cache.contains(&t.key))
            .map(|t| t.key)
            .collect();
        if keys.len() != self.last_tiles.len() || keys != self.last_tiles {
            if keys.len() == self.last_tiles.len() {
                // same number of tiles, some replaced (e.g. after panning one column)
                for (i, item) in items.into_iter().enumerate() {
                    self.tile_model.set_row_data(i, item);
                }
            } else {
                self.tile_model.set_vec(items);
            }
            self.last_tiles = keys;
            return;
        }
        // same tiles, maybe moved: only touch rows whose geometry changed
        use slint::Model;
        for (i, item) in items.into_iter().enumerate() {
            if let Some(old) = self.tile_model.row_data(i)
                && (old.x != item.x || old.y != item.y || old.size != item.size)
            {
                self.tile_model.set_row_data(i, item);
            }
        }
    }

    fn render_planes(&mut self) {
        let Some(win) = self.win.upgrade() else {
            return;
        };
        let now = Instant::now();
        let (w, h) = (self.view.width, self.view.height);
        let mut items: Vec<(&str, PlaneItem)> = Vec::with_capacity(self.planes.len());
        let mut selected_item = None;
        self.hits.clear();
        for (hex, t) in &self.planes {
            let is_sel = self.selected.as_deref() == Some(hex.as_str());
            if !is_sel && !self.filter.matches(&t.ac) {
                continue;
            }
            let (lat, lon) = t.position_now(now);
            let (x, y) = self.view.project(lat, lon);
            if x < -30.0 || y < -30.0 || x > w + 30.0 || y > h + 30.0 {
                continue;
            }
            let color = if t.ac.on_ground {
                GROUND_COLOR
            } else {
                fmt::altitude_color(t.ac.alt_baro_ft.or(t.ac.alt_geom_ft))
            };
            let item = PlaneItem {
                x: x as f32,
                y: y as f32,
                heading: t.ac.track_deg.unwrap_or(0.0) as f32,
                color: rgb(color),
                ground: t.ac.on_ground,
                selected: is_sel,
                emergency: t.ac.emergency,
            };
            self.hits.push((hex.clone(), x, y));
            if is_sel {
                selected_item = Some(item);
            } else {
                items.push((hex.as_str(), item));
            }
        }
        let visible = self.hits.len();
        // Stable order (HashMap order changes on insert) so the diff below only
        // touches rows that really changed; the selected plane is drawn on top.
        items.sort_unstable_by(|a, b| a.0.cmp(b.0));
        let mut items: Vec<PlaneItem> = items.into_iter().map(|(_, i)| i).collect();
        items.extend(selected_item);
        self.push_planes(items);

        // hover tooltip
        let hover = self.hover.as_ref().and_then(|hx| {
            let (_, x, y) = self.hits.iter().find(|(h, _, _)| h == hx)?;
            let name = self.planes.get(hx)?.ac.display_name().to_string();
            Some((name, *x, *y))
        });
        match hover {
            Some((name, x, y)) => {
                win.set_hover_text(name.into());
                win.set_hover_x(x as f32);
                win.set_hover_y(y as f32);
                win.set_hovering(true);
            }
            None => {
                win.set_hover_text("".into());
                win.set_hovering(false);
            }
        }

        // trail of the selected aircraft, ending at its current position
        let trail = self
            .selected
            .as_ref()
            .and_then(|h| Some((self.trails.get(h)?, self.planes.get(h)?)))
            .map(|(tr, t)| {
                let mut pts: Vec<(f64, f64)> = tr.points.iter().copied().collect();
                pts.push(t.position_now(now));
                polyline(&pts, &self.view)
            })
            .unwrap_or_default();
        if trail != self.last_trail {
            win.set_trail_path(trail.as_str().into());
            self.last_trail = trail;
        }

        self.update_panel(&win, now);
        self.update_stats(&win, visible);
    }

    /// Update the plane model in place, skipping rows whose change is invisible.
    fn push_planes(&mut self, items: Vec<PlaneItem>) {
        use slint::Model;
        if items.len() != self.last_planes.len() {
            self.plane_model.set_vec(items.clone());
            self.last_planes = items;
            return;
        }
        for (i, (new, old)) in items.iter().zip(self.last_planes.iter_mut()).enumerate() {
            let moved = (new.x - old.x).abs() >= MOVE_EPS
                || (new.y - old.y).abs() >= MOVE_EPS
                || (new.heading - old.heading).abs() >= 1.0;
            let same_style = new.color == old.color
                && new.ground == old.ground
                && new.selected == old.selected
                && new.emergency == old.emergency;
            if moved || !same_style {
                *old = new.clone();
                self.plane_model.set_row_data(i, new.clone());
            }
        }
        debug_assert_eq!(self.plane_model.row_count(), self.last_planes.len());
    }

    fn apply_alert(&self) {
        if let Some(win) = self.win.upgrade() {
            win.set_alert_text(self.alert.as_ref().map_or("", |(t, _)| t.as_str()).into());
        }
    }

    fn update_status(&self) {
        if let Some(win) = self.win.upgrade() {
            self.update_stats(&win, self.hits.len());
        }
    }

    fn update_stats(&self, win: &MainWindow, visible: usize) {
        let now = Instant::now();
        let updated = self.status.last_update.map_or("never".to_string(), |t| {
            fmt::age(now.duration_since(t).as_secs_f64())
        });
        win.set_stats_text(
            format!(
                "✈ {visible} aircraft visible ({} tracked)  ·  Updated {updated}  ·  Source: {}",
                self.planes.len(),
                self.status.source
            )
            .into(),
        );
        let status =
            if self.status.ok && self.status.last_update.is_some() && self.planes.is_empty() {
                format!(
                    "{} · No aircraft in this area right now — zoom out to see more",
                    self.status.message
                )
            } else if self.status.ok {
                self.status.message.clone()
            } else if let Some(at) = self.status.retry_at {
                format!(
                    "{} — retry in {} s",
                    self.status.message,
                    at.saturating_duration_since(now).as_secs()
                )
            } else {
                self.status.message.clone()
            };
        win.set_status_ok(self.status.ok);
        win.set_status_text(status.into());
    }

    fn update_panel(&mut self, win: &MainWindow, now: Instant) {
        let Some(sel) = self.selected.clone() else {
            win.set_has_selection(false);
            self.last_rows.clear();
            return;
        };
        let live = self.planes.get(&sel).cloned();
        if let Some(t) = &live {
            self.snapshot = Some(t.clone());
        }
        let Some(t) = live.or_else(|| self.snapshot.clone()) else {
            win.set_has_selection(false);
            return;
        };
        let a = &t.ac;
        let u = self.cfg.units;
        let loaded = self.meta.loaded;
        let info = self.meta.info.as_ref();
        let route = self.meta.route.as_ref();
        let pending = |v: Option<String>| {
            v.unwrap_or_else(|| {
                if loaded {
                    "—".into()
                } else {
                    "loading…".into()
                }
            })
        };

        let reg = a
            .registration
            .clone()
            .or_else(|| info.and_then(|i| i.registration.clone()));
        let type_text = match (info.and_then(|i| i.type_name.clone()), a.type_code.clone()) {
            (Some(n), Some(c)) => Some(format!("{n} ({c})")),
            (Some(n), None) => Some(n),
            (None, Some(c)) => Some(c),
            (None, None) => None,
        };
        let operator = info
            .and_then(|i| i.operator.clone())
            .or_else(|| route.and_then(|r| r.airline.clone()));
        let age_s = a.pos_age_s + now.duration_since(t.received).as_secs_f64();
        let live_now = self.planes.contains_key(&sel);

        let mut rows = vec![
            kv("Callsign", a.callsign.clone().unwrap_or_else(|| "—".into())),
            kv("Registration", pending(reg.clone())),
            kv("ICAO hex", a.hex.to_uppercase()),
            kv("Type", pending(type_text)),
            kv("Operator", pending(operator)),
            kv("Route", pending(route.and_then(RouteInfo::summary))),
            kv(
                "Altitude (baro)",
                if a.on_ground {
                    "On ground".to_string()
                } else {
                    opt_f(a.alt_baro_ft, |v| fmt::altitude(v, u))
                },
            ),
            kv(
                "Altitude (geom)",
                opt_f(a.alt_geom_ft, |v| fmt::altitude(v, u)),
            ),
            kv("Ground speed", opt_f(a.gs_kt, |v| fmt::speed(v, u))),
            kv("Vertical rate", opt_f(a.vrate_fpm, |v| fmt::vrate(v, u))),
            kv("Heading", opt_f(a.track_deg, |v| format!("{:.0}°", v))),
            kv(
                "Squawk",
                match (&a.squawk, a.emergency) {
                    (Some(s), true) => format!("{s}  ⚠ EMERGENCY"),
                    (Some(s), false) => s.clone(),
                    (None, _) => "—".into(),
                },
            ),
        ];
        let (plat, plon) = t.position_now(now);
        rows.push(kv("Position", fmt::position(plat, plon)));
        rows.push(kv(
            "Last seen",
            if live_now {
                fmt::age(age_s)
            } else {
                format!("{} (lost)", fmt::age(age_s))
            },
        ));
        rows.push(kv("Data source", a.source));
        if a.military {
            rows.push(kv("Flags", "Military"));
        }
        if rows != self.last_rows {
            self.row_model.set_vec(rows.clone());
            self.last_rows = rows;
        }

        win.set_has_selection(true);
        win.set_panel_emergency(a.emergency);
        win.set_panel_title(a.display_name().to_string().into());
        let subtitle = [reg, a.type_code.clone()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
        win.set_panel_subtitle(subtitle.into());
    }
}

/// End-to-end UI tests: the real window and callbacks on Slint's headless
/// testing backend, fed with fixture data instead of the network.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::adsbx::parse_response;
    use slint::Model;

    const SAMPLE: &str = include_str!("../../tests/fixtures/adsblol_sample.json");

    fn rows(win: &MainWindow) -> Vec<(String, String)> {
        let m = win.get_panel_rows();
        (0..m.row_count())
            .filter_map(|i| m.row_data(i))
            .map(|kv| (kv.k.to_string(), kv.v.to_string()))
            .collect()
    }

    fn row(win: &MainWindow, key: &str) -> String {
        rows(win)
            .into_iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
            .unwrap_or_default()
    }

    fn plane_count(win: &MainWindow) -> usize {
        win.get_planes().row_count()
    }

    fn screen_pos(hex: &str) -> (f32, f32) {
        with_app(|a| {
            let (_, x, y) = a
                .hits
                .iter()
                .find(|(h, _, _)| h == hex)
                .expect("plane on screen");
            (*x as f32, *y as f32)
        })
        .unwrap()
    }

    fn feed(source: &'static str, fallback: bool) {
        let aircraft = parse_response(SAMPLE, source).unwrap();
        with_app(|a| {
            a.on_poll(PollEvent::Update {
                aircraft,
                source,
                fallback,
            })
        });
    }

    #[test]
    fn ui_end_to_end() {
        i_slint_backend_testing::init_no_event_loop();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let dir = std::env::temp_dir().join(format!("skytrace-test-ui-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let config_path = dir.join("config/config.toml");
        let env = Env {
            cfg: Config {
                // unroutable: no real tile traffic from tests
                tile_url: "http://127.0.0.1:9/{z}/{x}/{y}.png".into(),
                ..Config::default()
            },
            config_path: Some(config_path.clone()),
            cache_dir: dir.join("cache"),
            online: false,
        };
        let win = build(rt.handle(), crate::sources::test_client(), env).unwrap();
        assert!(win.get_attribution().contains("Natural Earth"));
        assert!(win.get_attribution().contains("adsbdb"));

        // --- planes load
        feed("adsb.lol", false);
        assert_eq!(plane_count(&win), 5, "5 positioned aircraft in the fixture");
        assert!(win.get_stats_text().contains("5 aircraft visible"));
        assert!(win.get_status_ok());
        assert!(win.get_attribution().contains("adsb.lol (ODbL)"));
        assert!(
            win.get_alert_text().contains("EMERGENCY 7700"),
            "{}",
            win.get_alert_text()
        );

        // empty response: explain instead of showing a silent empty map
        with_app(|a| {
            a.on_poll(PollEvent::Update {
                aircraft: vec![],
                source: "adsb.lol",
                fallback: false,
            })
        });
        assert_eq!(plane_count(&win), 0);
        assert!(
            win.get_status_text().contains("zoom out"),
            "{}",
            win.get_status_text()
        );
        feed("adsb.lol", false);
        assert!(!win.get_status_text().contains("zoom out"));

        // same data again: nothing is re-pushed, model stays stable
        feed("adsb.lol", false);
        assert_eq!(plane_count(&win), 5);

        // --- hover then click -> detail panel
        let (x, y) = screen_pos("a1b2c3");
        win.invoke_map_move(x + 3.0, y);
        assert!(win.get_hovering());
        assert_eq!(win.get_hover_text(), "SWA1234");
        win.invoke_map_down(x, y);
        win.invoke_map_up(x, y);
        assert!(win.get_has_selection());
        assert_eq!(win.get_panel_title(), "SWA1234");
        assert_eq!(row(&win, "ICAO hex"), "A1B2C3");
        assert_eq!(row(&win, "Registration"), "N8620H");
        assert_eq!(row(&win, "Altitude (baro)"), "34,000 ft");
        assert_eq!(row(&win, "Data source"), "adsb.lol");
        assert!(win.get_trail_path().starts_with('M'));
        // a drag is a pan, not a click
        let before = with_app(|a| (a.view.lat, a.view.lon)).unwrap();
        win.invoke_close_panel();
        assert!(!win.get_has_selection());
        win.invoke_map_down(500.0, 350.0);
        win.invoke_map_move(560.0, 380.0);
        win.invoke_map_up(560.0, 380.0);
        let after = with_app(|a| (a.view.lat, a.view.lon)).unwrap();
        assert!(
            after.1 < before.1 && after.0 > before.0,
            "{before:?} -> {after:?}"
        );
        assert!(!win.get_has_selection());

        // --- search
        win.invoke_search_edited("rch".into());
        assert_eq!(plane_count(&win), 1);
        win.invoke_search_accepted("rch".into());
        assert_eq!(win.get_panel_title(), "RCH401");
        assert!(row(&win, "Squawk").contains("EMERGENCY"));
        assert!(row(&win, "Flags").contains("Military"));
        win.invoke_search_edited("".into());
        win.invoke_close_panel();
        assert_eq!(plane_count(&win), 5);

        // --- filters
        win.set_f_ground(true);
        win.invoke_filters_changed();
        assert_eq!(plane_count(&win), 4, "ground aircraft hidden");
        win.set_f_ground(false);
        win.set_f_min_alt("10000".into());
        win.invoke_filters_changed();
        assert_eq!(plane_count(&win), 2, "only 34,000 ft and 12,000 ft remain");
        win.set_f_min_alt("".into());
        win.set_f_type("c17".into());
        win.invoke_filters_changed();
        assert_eq!(plane_count(&win), 1);
        win.set_f_type("".into());
        win.invoke_filters_changed();

        // --- settings: cancel discards edits
        win.invoke_open_settings();
        assert!(win.get_settings_open());
        let saved_lat = win.get_s_lat();
        win.set_s_lat("12.5".into());
        win.set_settings_open(false); // Cancel
        win.invoke_open_settings();
        assert_eq!(win.get_s_lat(), saved_lat);

        // invalid input is rejected and nothing is written
        win.set_s_tile_url("https://tiles.example.org/{z}/{x}.png".into());
        win.invoke_save_settings();
        assert!(win.get_settings_open() && !win.get_s_error().is_empty());
        assert!(!config_path.exists());

        // valid settings persist and apply
        win.set_s_tile_url("http://127.0.0.1:9/{z}/{x}/{y}.png".into());
        win.set_s_poll(12.0);
        win.set_s_units(1);
        win.set_s_theme(2);
        win.set_s_source(2);
        win.set_s_lat("51.47".into());
        win.set_s_lon("-0.4543".into());
        win.invoke_save_settings();
        assert!(!win.get_settings_open(), "{}", win.get_s_error());
        let on_disk = Config::load_from(&config_path);
        assert_eq!(on_disk.poll_interval_secs, 12);
        assert_eq!(on_disk.units, Units::Metric);
        assert_eq!(on_disk.theme, Theme::Dark);
        assert_eq!(win.global::<super::super::AppTheme>().get_variant(), 1);
        assert!(win.global::<super::super::AppTheme>().get_dark());
        assert_eq!(
            win.global::<super::super::Palette>().get_color_scheme(),
            slint::language::ColorScheme::Dark
        );
        assert_eq!(on_disk.source, SourceChoice::AdsbFi);
        assert_eq!((on_disk.home_lat, on_disk.home_lon), (51.47, -0.4543));
        assert_eq!(win.get_alt_unit(), "m");
        assert!(win.get_attribution().contains("Tiles: 127.0.0.1:9"));
        // the watch channel carries the new interval/source to the poller
        let p = with_app(|a| a.params_tx.borrow().clone()).unwrap();
        assert_eq!(
            (p.interval, p.source),
            (Duration::from_secs(12), SourceChoice::AdsbFi)
        );

        // metric filter input is interpreted in metres after the units change
        win.set_f_min_alt("3000".into()); // ≈ 9,843 ft
        win.invoke_filters_changed();
        assert_eq!(plane_count(&win), 2);
        win.set_f_min_alt("".into());
        win.invoke_filters_changed();

        // --- fallback source is reported
        feed("adsb.fi", true);
        assert!(win.get_status_text().contains("fallback"));
        assert!(win.get_attribution().contains("adsb.fi"));
        with_app(|a| {
            a.on_poll(PollEvent::Failure {
                message: "adsb.lol: HTTP status 503".into(),
                retry_in: Duration::from_secs(5),
            })
        });
        assert!(!win.get_status_ok());
        assert!(win.get_status_text().contains("retry in"));

        // --- every theme keeps widgets and panels on the same light/dark side
        for t in Theme::ALL {
            win.invoke_open_settings();
            win.set_s_theme(t.index());
            win.invoke_save_settings();
            let at = win.global::<super::super::AppTheme>();
            let widgets_dark = win.global::<super::super::Palette>().get_color_scheme()
                == slint::language::ColorScheme::Dark;
            assert_eq!(at.get_dark(), widgets_dark, "{t:?}");
            // panel text must contrast with the panel background
            let luma = |c: slint::Color| {
                0.2126 * f32::from(c.red())
                    + 0.7152 * f32::from(c.green())
                    + 0.0722 * f32::from(c.blue())
            };
            assert!(
                (luma(at.get_panel()) - luma(at.get_text())).abs() > 120.0,
                "{t:?}"
            );
        }

        // --- tiles: once every visible tile is loaded the basemap is hidden, and
        // panning updates rows in place instead of rebuilding the model
        let fill_tiles = || {
            with_app(|a| {
                let gen_ = a.tile_gen;
                for t in a.view.visible_tiles() {
                    let px = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(4, 4);
                    a.on_tile(Decoded {
                        generation: gen_,
                        key: t.key,
                        skipped: false,
                        pixels: Some(px),
                    });
                }
            })
        };
        assert!(win.get_basemap_visible());
        // a tile skipped while panning is not treated as failed; a real failure is
        with_app(|a| {
            let gen_ = a.tile_gen;
            let keys: Vec<_> = a.view.visible_tiles().iter().map(|t| t.key).collect();
            a.on_tile(Decoded {
                generation: gen_,
                key: keys[0],
                pixels: None,
                skipped: true,
            });
            assert!(!a.tile_failed.contains_key(&keys[0]));
            a.on_tile(Decoded {
                generation: gen_,
                key: keys[1],
                pixels: None,
                skipped: false,
            });
            assert!(a.tile_failed.contains_key(&keys[1]));
            a.tile_failed.clear();
        });
        fill_tiles();
        assert!(!win.get_basemap_visible(), "tiles cover the view");
        let n_tiles = win.get_tiles().row_count();
        assert!(n_tiles > 0);
        win.invoke_map_down(400.0, 300.0);
        win.invoke_map_move(410.0, 305.0);
        win.invoke_map_move(420.0, 310.0);
        win.invoke_map_up(420.0, 310.0);
        assert_eq!(win.get_tiles().row_count(), n_tiles);
        // pan a long way: uncovered area -> basemap comes back with fresh paths
        win.invoke_map_down(0.0, 0.0);
        win.invoke_map_move(900.0, 600.0);
        win.invoke_map_up(900.0, 600.0);
        assert!(win.get_basemap_visible());
        fill_tiles();
        assert!(!win.get_basemap_visible());

        // --- home button returns to the saved home
        win.invoke_go_home();
        let v = with_app(|a| a.view).unwrap();
        assert_eq!((v.lat, v.lon), (51.47, -0.4543));

        APP.with(|a| a.borrow_mut().take());
        let _ = std::fs::remove_dir_all(dir);
    }
}
