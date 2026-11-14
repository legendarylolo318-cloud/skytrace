//! Tile downloading with an on-disk cache (expiry) and decoding.
//!
//! Follows the OSM tile usage policy: descriptive User-Agent (set on the shared
//! client), at most two concurrent connections, cache reuse, and only tiles
//! that are currently visible are ever requested (no prefetching).

use super::projection::{TileKey, tile_url};
use slint::{Rgba8Pixel, SharedPixelBuffer};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use tokio::sync::Semaphore;

/// How long a cached tile is used without re-fetching.
pub const TILE_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
/// Cached tiles not refreshed for this long are deleted at startup, keeping
/// the disk cache bounded to roughly the areas viewed in the last month.
pub const DISK_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 3600);
const MAX_CONCURRENT: usize = 2;

pub struct Decoded {
    pub generation: u32,
    pub key: TileKey,
    /// `None` when the tile could not be loaded.
    pub pixels: Option<SharedPixelBuffer<Rgba8Pixel>>,
    /// True when the download was skipped because the tile scrolled out of
    /// view while queued. That is not a failure and must not delay a retry.
    pub skipped: bool,
}

pub type TileSink = Arc<dyn Fn(Decoded) + Send + Sync>;

pub struct TileService {
    rt: tokio::runtime::Handle,
    client: reqwest::Client,
    cache_root: PathBuf,
    sem: Arc<Semaphore>,
    in_flight: Arc<Mutex<HashSet<(u32, TileKey)>>>,
    wanted: Arc<Mutex<HashSet<TileKey>>>,
    sink: TileSink,
}

fn fnv(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Delete cached files older than `max_age` below `dir` (recursively) and remove
/// directories left empty. Returns the number of files removed.
pub fn prune_disk_cache(dir: &std::path::Path, max_age: Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let now = SystemTime::now();
    let mut removed = 0;
    for e in entries.flatten() {
        let path = e.path();
        let Ok(meta) = e.metadata() else { continue };
        if meta.is_dir() {
            removed += prune_disk_cache(&path, max_age);
            let _ = std::fs::remove_dir(&path); // only succeeds when empty
        } else if meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age > max_age)
            && std::fs::remove_file(&path).is_ok()
        {
            removed += 1;
        }
    }
    removed
}

pub fn decode(bytes: &[u8]) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let img = image::load_from_memory(bytes).ok()?.to_rgba8();
    Some(SharedPixelBuffer::clone_from_slice(
        img.as_raw(),
        img.width(),
        img.height(),
    ))
}

impl TileService {
    pub fn new(
        rt: tokio::runtime::Handle,
        client: reqwest::Client,
        cache_root: PathBuf,
        sink: TileSink,
    ) -> Self {
        let tiles_dir = cache_root.join("tiles");
        rt.spawn_blocking(move || prune_disk_cache(&tiles_dir, DISK_MAX_AGE));
        Self {
            rt,
            client,
            cache_root,
            sem: Arc::new(Semaphore::new(MAX_CONCURRENT)),
            in_flight: Arc::default(),
            wanted: Arc::default(),
            sink,
        }
    }

    /// Replace the set of tiles currently on screen; queued work for others is dropped.
    pub fn set_wanted(&self, keys: impl IntoIterator<Item = TileKey>) {
        *lock(&self.wanted) = keys.into_iter().collect();
    }

    fn path(&self, template: &str, k: TileKey) -> PathBuf {
        self.cache_root
            .join("tiles")
            .join(format!("{:016x}", fnv(template)))
            .join(k.z.to_string())
            .join(k.x.to_string())
            .join(k.y.to_string())
    }

    /// Request a tile (no-op if already being loaded).
    pub fn request(&self, generation: u32, template: &str, key: TileKey) {
        if !lock(&self.in_flight).insert((generation, key)) {
            return;
        }
        let path = self.path(template, key);
        let url = tile_url(template, key);
        let (client, sem, sink) = (self.client.clone(), self.sem.clone(), self.sink.clone());
        let (in_flight, wanted) = (self.in_flight.clone(), self.wanted.clone());
        self.rt.spawn(async move {
            let (pixels, skipped) = load(&client, &sem, &wanted, key, &url, &path).await;
            lock(&in_flight).remove(&(generation, key));
            sink(Decoded {
                generation,
                key,
                pixels,
                skipped,
            });
        });
    }
}

async fn load(
    client: &reqwest::Client,
    sem: &Semaphore,
    wanted: &Mutex<HashSet<TileKey>>,
    key: TileKey,
    url: &str,
    path: &PathBuf,
) -> (Option<SharedPixelBuffer<Rgba8Pixel>>, bool) {
    let cached = tokio::fs::read(path).await.ok();
    let fresh = tokio::fs::metadata(path)
        .await
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age < TILE_TTL);
    if let (Some(bytes), true) = (&cached, fresh)
        && let Some(px) = decode_blocking(bytes.clone()).await
    {
        return (Some(px), false);
    }
    let Ok(permit) = sem.acquire().await else {
        return (None, true);
    };
    if !lock(wanted).contains(&key) {
        // scrolled away while queued: serve stale data if any, never hit the network
        drop(permit);
        let stale = match cached {
            Some(b) => decode_blocking(b).await,
            None => None,
        };
        return (stale, true);
    }
    let fetched = async {
        let resp = client.get(url).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.bytes().await.ok()
    }
    .await;
    drop(permit);
    if let Some(bytes) = fetched
        && let Some(px) = decode_blocking(bytes.to_vec()).await
    {
        if let Some(dir) = path.parent() {
            let _ = tokio::fs::create_dir_all(dir).await;
        }
        // temp + rename: a partially written tile must never be served later
        let tmp = path.with_extension("part");
        if tokio::fs::write(&tmp, &bytes).await.is_ok() {
            let _ = tokio::fs::rename(&tmp, path).await;
        }
        return (Some(px), false);
    }
    // network failed: a stale tile beats no tile
    let stale = match cached {
        Some(b) => decode_blocking(b).await,
        None => None,
    };
    (stale, false)
}

async fn decode_blocking(bytes: Vec<u8>) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    tokio::task::spawn_blocking(move || decode(&bytes))
        .await
        .ok()
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_rejects_garbage() {
        assert!(decode(b"not an image").is_none());
    }

    #[test]
    fn decode_png() {
        let mut buf = Vec::new();
        image::DynamicImage::new_rgba8(2, 3)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        let px = decode(&buf).unwrap();
        assert_eq!((px.width(), px.height()), (2, 3));
    }

    #[test]
    fn prune_removes_only_old_files() {
        let d = std::env::temp_dir().join(format!("skytrace-test-tiles-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("t/3/1")).unwrap();
        std::fs::write(d.join("t/3/1/2"), b"x").unwrap();
        assert_eq!(prune_disk_cache(&d, Duration::from_secs(3600)), 0);
        assert!(d.join("t/3/1/2").exists());
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(prune_disk_cache(&d, Duration::from_millis(1)), 1);
        assert!(!d.join("t").exists(), "empty dirs are removed");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn cache_path_layout() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let root = std::env::temp_dir().join("skytrace-test-layout-nonexistent");
        let svc = TileService::new(
            rt.handle().clone(),
            crate::sources::test_client(),
            root.clone(),
            Arc::new(|_| {}),
        );
        let p = svc.path("https://t/{z}/{x}/{y}.png", TileKey { z: 5, x: 6, y: 7 });
        assert!(p.starts_with(root.join("tiles")) && p.ends_with("5/6/7"));
    }

    #[test]
    fn hash_differs_per_template() {
        assert_ne!(fnv("a/{z}"), fnv("b/{z}"));
    }
}
