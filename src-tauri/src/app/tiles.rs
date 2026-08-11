//! OpenStreetMap tile proxy, served to the webview over a custom `tiles://`
//! URI scheme.
//!
//! Why this exists instead of letting Leaflet fetch tiles directly (see
//! CLAUDE.md): all network access lives in Rust so the CSP can stay strict —
//! a webview that fetches third-party tile URLs directly needs `connect-src`
//! opened to that host, which this app's threat model treats as unacceptable
//! (it would also leak the user's VPN exit-node coordinates to a third party
//! on every map open). Rust fetches the tile and hands back only image bytes.
//!
//! Also satisfies OpenStreetMap's tile usage policy: an identifying
//! `User-Agent` and a disk cache so tiles are never bulk re-downloaded.
//! <https://operations.osmfoundation.org/policies/tiles/>
//!
//! # Request path validation
//!
//! `z`/`x`/`y` are parsed as unsigned integers and range-checked against the
//! valid slippy-map tile grid for that zoom level *before* anything is built
//! from them. Every downstream use (the upstream OSM URL, the on-disk cache
//! path) is built from those validated integers via `Display`/`join`, never
//! from the raw path string — so a malformed or hostile path (extra
//! segments, non-numeric input, `../` traversal attempts, out-of-range
//! zoom/x/y) is rejected by `TileCoord::parse` and never reaches either the
//! upstream request or the filesystem.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use reqwest::Client;
use tauri::http::{header, Request, Response, StatusCode};
use tauri::{AppHandle, Builder, Manager, Runtime};

/// The scheme name passed to `register_asynchronous_uri_scheme_protocol`.
/// On Windows/Android this is served at `http://tiles.localhost/...`; on
/// macOS/Linux/iOS at `tiles://localhost/...`.
pub const SCHEME: &str = "tiles";

/// OSM's tile usage policy requires an identifying User-Agent (ideally with
/// a contact URL) and explicitly blocks generic/default agents.
const TILE_USER_AGENT: &str = concat!(
    "ipwatch/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/joelmale/ipwatch)"
);

/// Slippy-map tiles are only defined up to zoom 19 on the standard OSM
/// raster layer.
const MAX_ZOOM: u8 = 19;

const TILE_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Registers the `tiles://` scheme on the builder.
///
/// Must run on `Builder` before `.build()`/`.run()` — scheme registration is
/// not available once the app is constructed, so this cannot live in
/// `setup()` alongside the rest of the app wiring.
pub fn register<R: Runtime>(builder: Builder<R>) -> Builder<R> {
    builder.register_asynchronous_uri_scheme_protocol(SCHEME, |ctx, request, responder| {
        let app_handle = ctx.app_handle().clone();
        tauri::async_runtime::spawn(async move {
            let response = handle_request(&app_handle, &request).await;
            responder.respond(response);
        });
    })
}

/// Resolves one tile request: parse + validate the path, serve from disk
/// cache if present, otherwise fetch from OSM and populate the cache.
///
/// Never panics — every fallible step here degrades to an HTTP error
/// response instead, because a panic inside a uri scheme handler can take
/// down the webview process (see module docs on the CSP/network rationale).
async fn handle_request<R: Runtime>(
    app_handle: &tauri::AppHandle<R>,
    request: &Request<Vec<u8>>,
) -> Response<Vec<u8>> {
    let Some(coord) = TileCoord::parse(request.uri().path()) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid tile path");
    };

    let cache_path = match app_handle.path().app_cache_dir() {
        Ok(dir) => Some(coord.cache_path(&dir)),
        Err(err) => {
            tracing::warn!(%err, "could not resolve app cache dir; tiles will not be cached this session");
            None
        }
    };

    if let Some(cache_path) = &cache_path {
        match tokio::fs::read(cache_path).await {
            Ok(bytes) => return png_response(bytes),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                tracing::warn!(%err, path = %cache_path.display(), "failed to read tile cache; fetching fresh");
            }
        }
    }

    fetch_and_respond(&coord, cache_path.as_deref()).await
}

/// Fetches the tile from OpenStreetMap, writes it to `cache_path` (best
/// effort — a cache write failure still serves the tile), and returns the
/// PNG response.
async fn fetch_and_respond(coord: &TileCoord, cache_path: Option<&Path>) -> Response<Vec<u8>> {
    let url = coord.upstream_url();

    let upstream = match tile_client().get(&url).send().await {
        Ok(resp) => resp,
        Err(err) => {
            tracing::warn!(%err, url, "tile fetch failed");
            return error_response(StatusCode::BAD_GATEWAY, "tile fetch failed");
        }
    };

    if !upstream.status().is_success() {
        tracing::warn!(status = %upstream.status(), url, "tile upstream returned an error status");
        return error_response(StatusCode::BAD_GATEWAY, "tile upstream returned an error");
    }

    let bytes = match upstream.bytes().await {
        Ok(bytes) => bytes.to_vec(),
        Err(err) => {
            tracing::warn!(%err, url, "failed to read tile response body");
            return error_response(StatusCode::BAD_GATEWAY, "failed to read tile body");
        }
    };

    if let Some(cache_path) = cache_path {
        if let Some(parent) = cache_path.parent() {
            if let Err(err) = tokio::fs::create_dir_all(parent).await {
                tracing::warn!(%err, dir = %parent.display(), "failed to create tile cache directory");
            }
        }
        if let Err(err) = tokio::fs::write(cache_path, &bytes).await {
            tracing::warn!(%err, path = %cache_path.display(), "failed to write tile to cache");
        }
    }

    png_response(bytes)
}

/// Builds (once) the dedicated HTTP client used only for tile fetches.
///
/// Deliberately not `providers::http_client()`: that client's User-Agent is
/// just `ipwatch/<version>`, with no contact URL, which does not meet OSM's
/// policy as clearly as the one used here. Falls back to a default client on
/// the (practically unreachable) chance the configured builder fails, rather
/// than unwrapping.
fn tile_client() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        Client::builder()
            .timeout(TILE_REQUEST_TIMEOUT)
            .user_agent(TILE_USER_AGENT)
            .build()
            .unwrap_or_else(|err| {
                tracing::error!(%err, "failed to build tile http client with custom config; falling back to reqwest defaults");
                Client::new()
            })
    })
}

fn png_response(bytes: Vec<u8>) -> Response<Vec<u8>> {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/png")
        .header(header::CACHE_CONTROL, "public, max-age=604800, immutable")
        .body(bytes)
        .unwrap_or_else(|err| {
            tracing::error!(%err, "failed to build tile success response");
            Response::new(Vec::new())
        })
}

fn error_response(status: StatusCode, message: &str) -> Response<Vec<u8>> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain")
        .body(message.as_bytes().to_vec())
        .unwrap_or_else(|err| {
            tracing::error!(%err, "failed to build tile error response");
            Response::new(Vec::new())
        })
}

/// A validated `{z}/{x}/{y}` slippy-map tile coordinate.
///
/// The only way to construct one is `parse`, which range-checks every field.
/// Everything downstream (`upstream_url`, `cache_path`) formats these
/// integers back out — no raw path segment from the request ever reaches
/// either the outbound URL or the filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TileCoord {
    z: u8,
    x: u32,
    y: u32,
}

impl TileCoord {
    /// Parses and validates a request path of the form `/{z}/{x}/{y}.png`.
    ///
    /// Rejects (returns `None` rather than panicking or guessing):
    /// - wrong segment count (too few, too many, e.g. traversal-style
    ///   `/../../x.png` which splits into empty/`..` segments that fail the
    ///   numeric parse below)
    /// - non-numeric or missing `.png` suffix
    /// - zoom above `MAX_ZOOM`
    /// - x/y outside `0..2^z`, the valid tile grid for that zoom
    fn parse(path: &str) -> Option<Self> {
        let path = path.strip_prefix('/').unwrap_or(path);
        let mut segments = path.split('/');

        let z_str = segments.next()?;
        let x_str = segments.next()?;
        let y_str = segments.next()?;
        if segments.next().is_some() {
            return None;
        }

        let y_str = y_str.strip_suffix(".png")?;
        if z_str.is_empty() || x_str.is_empty() || y_str.is_empty() {
            return None;
        }

        let z: u8 = z_str.parse().ok()?;
        let x: u32 = x_str.parse().ok()?;
        let y: u32 = y_str.parse().ok()?;

        if z > MAX_ZOOM {
            return None;
        }
        // 2^z tiles per axis at this zoom; z <= MAX_ZOOM (19) so this never
        // overflows u32.
        let bound: u32 = 1u32 << z;
        if x >= bound || y >= bound {
            return None;
        }

        Some(Self { z, x, y })
    }

    fn upstream_url(&self) -> String {
        format!(
            "https://tile.openstreetmap.org/{}/{}/{}.png",
            self.z, self.x, self.y
        )
    }

    /// `{cache_dir}/tiles/{z}/{x}/{y}.png`. Built entirely from validated
    /// integers, so this cannot escape `cache_dir` regardless of what the
    /// original request path contained.
    fn cache_path(&self, cache_dir: &Path) -> PathBuf {
        cache_dir
            .join("tiles")
            .join(self.z.to_string())
            .join(self.x.to_string())
            .join(format!("{}.png", self.y))
    }
}

// --- Cache pruning ---
//
// Bounds the on-disk tile cache by age and total size, so a long-lived
// install does not grow without limit. See module docs for the OSM tile
// usage policy this is written against: caching is wanted, needless
// re-fetching is not, so the age limit here is deliberately generous rather
// than a knob to tune down.

/// Tiles not modified within this window are pruned regardless of the size
/// budget. 30 days is intentionally generous — see module/section docs.
const MAX_TILE_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Total on-disk budget for the tile cache. This app's map only ever shows
/// the neighborhood of one location (the current external IP's geolocation),
/// not an arbitrarily panned/zoomed atlas, so 100 MiB is generous headroom
/// rather than a tight limit.
const MAX_CACHE_BYTES: u64 = 100 * 1024 * 1024;

/// One cached tile file as fed into `plan_prune`: just enough to decide
/// whether it should be removed, deliberately decoupled from any actual
/// filesystem access so the decision is unit-testable on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CacheEntry {
    path: PathBuf,
    size_bytes: u64,
    modified: SystemTime,
}

/// Pure decision: which entries `plan_prune` should remove, given the
/// current contents of the cache, "now", and the age/size limits.
///
/// Two passes:
/// 1. Any entry older than `max_age` (relative to `now`) is removed
///    unconditionally.
/// 2. Of what survives that pass, if the summed size still exceeds
///    `max_total_bytes`, the oldest-modified survivors are removed first
///    until the total is at or under budget.
///
/// No filesystem access of any kind — everything needed to test the policy
/// exhaustively is plain data passed in, which is the point: the walking and
/// deleting (`prune_cache`) stays a thin, hard-to-get-wrong shell around
/// this.
fn plan_prune(
    entries: &[CacheEntry],
    now: SystemTime,
    max_age: Duration,
    max_total_bytes: u64,
) -> Vec<PathBuf> {
    let mut to_remove = Vec::new();
    let mut survivors: Vec<&CacheEntry> = Vec::new();

    for entry in entries {
        // A `modified` time after `now` (clock skew, a restored backup)
        // yields Err from `duration_since`; treated as age zero rather than
        // propagated, so such an entry is judged only by the size pass.
        let age = now.duration_since(entry.modified).unwrap_or(Duration::ZERO);
        if age > max_age {
            to_remove.push(entry.path.clone());
        } else {
            survivors.push(entry);
        }
    }

    let mut total: u64 = survivors.iter().map(|e| e.size_bytes).sum();
    if total > max_total_bytes {
        survivors.sort_by_key(|e| e.modified);
        for entry in survivors {
            if total <= max_total_bytes {
                break;
            }
            to_remove.push(entry.path.clone());
            total = total.saturating_sub(entry.size_bytes);
        }
    }

    to_remove
}

/// Walks `tiles_dir` collecting a `CacheEntry` for every regular `.png` file
/// found, at any depth. Confined to `tiles_dir`: the traversal only ever
/// descends into directories discovered by reading `tiles_dir` itself, never
/// anything derived from a request path or other outside input.
///
/// Never follows symlinks — a symlinked file or directory found inside the
/// cache is skipped outright, neither read as a tile nor descended into —
/// and every per-entry filesystem error is logged at debug level and
/// skipped, never propagated. A missing `tiles_dir` (fresh install, nothing
/// cached yet) is not an error and yields an empty list silently.
async fn collect_cache_entries(tiles_dir: &Path) -> Vec<CacheEntry> {
    let mut entries = Vec::new();
    let mut dirs = vec![tiles_dir.to_path_buf()];

    while let Some(dir) = dirs.pop() {
        let mut read_dir = match tokio::fs::read_dir(&dir).await {
            Ok(read_dir) => read_dir,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                tracing::debug!(%err, dir = %dir.display(), "failed to read tile cache directory; skipping");
                continue;
            }
        };

        loop {
            let entry = match read_dir.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(err) => {
                    tracing::debug!(%err, dir = %dir.display(), "failed to read next tile cache directory entry; stopping this directory");
                    break;
                }
            };

            let file_type = match entry.file_type().await {
                Ok(file_type) => file_type,
                Err(err) => {
                    tracing::debug!(%err, path = %entry.path().display(), "failed to read tile cache entry type; skipping");
                    continue;
                }
            };

            // Symlinks are never followed, whether they point at a file or a
            // directory — `file_type()` reports the link itself here, not
            // its target, so this check alone is sufficient to keep the walk
            // confined to real files/directories under `tiles_dir`.
            if file_type.is_symlink() {
                continue;
            }

            if file_type.is_dir() {
                dirs.push(entry.path());
                continue;
            }

            if !file_type.is_file() {
                continue;
            }

            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("png") {
                continue;
            }

            let metadata = match entry.metadata().await {
                Ok(metadata) => metadata,
                Err(err) => {
                    tracing::debug!(%err, path = %path.display(), "failed to read tile cache entry metadata; skipping");
                    continue;
                }
            };

            let modified = match metadata.modified() {
                Ok(modified) => modified,
                Err(err) => {
                    tracing::debug!(%err, path = %path.display(), "platform does not support mtime; skipping from prune consideration");
                    continue;
                }
            };

            entries.push(CacheEntry {
                path,
                size_bytes: metadata.len(),
                modified,
            });
        }
    }

    entries
}

/// Prunes the on-disk tile cache once: lists everything under
/// `{app_cache_dir}/tiles`, decides what to remove via `plan_prune`, and
/// deletes it. Every step degrades to a logged skip rather than a
/// propagated error — see `collect_cache_entries` and the per-file removal
/// loop below — because a cache prune failing must never be able to affect
/// startup or an in-flight tile request.
async fn prune_cache<R: Runtime>(app_handle: &AppHandle<R>) {
    let cache_dir = match app_handle.path().app_cache_dir() {
        Ok(dir) => dir,
        Err(err) => {
            tracing::debug!(%err, "could not resolve app cache dir; skipping tile cache prune");
            return;
        }
    };
    let tiles_dir = cache_dir.join("tiles");

    let entries = collect_cache_entries(&tiles_dir).await;
    if entries.is_empty() {
        return;
    }

    let to_remove = plan_prune(&entries, SystemTime::now(), MAX_TILE_AGE, MAX_CACHE_BYTES);
    if to_remove.is_empty() {
        tracing::debug!(
            considered = entries.len(),
            "tile cache within limits; nothing pruned"
        );
        return;
    }

    let mut removed = 0usize;
    for path in &to_remove {
        match tokio::fs::remove_file(path).await {
            Ok(()) => removed += 1,
            Err(err) => {
                tracing::debug!(%err, path = %path.display(), "failed to remove stale tile; skipping")
            }
        }
    }
    tracing::debug!(
        removed,
        considered = entries.len(),
        planned = to_remove.len(),
        "tile cache prune complete"
    );
}

/// Runs `prune_cache` once, on its own background task, so it never blocks
/// app startup or a concurrent tile request. Called once from `app::setup`.
pub fn spawn_cache_prune<R: Runtime>(app_handle: AppHandle<R>) {
    tauri::async_runtime::spawn(async move {
        prune_cache(&app_handle).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_path() {
        let coord = TileCoord::parse("/3/4/5.png").expect("valid path should parse");
        assert_eq!(coord, TileCoord { z: 3, x: 4, y: 5 });
    }

    #[test]
    fn parses_path_without_leading_slash() {
        let coord = TileCoord::parse("3/4/5.png").expect("valid path should parse");
        assert_eq!(coord, TileCoord { z: 3, x: 4, y: 5 });
    }

    #[test]
    fn rejects_missing_segments() {
        assert!(TileCoord::parse("/3/4.png").is_none());
        assert!(TileCoord::parse("/3.png").is_none());
        assert!(TileCoord::parse("/").is_none());
        assert!(TileCoord::parse("").is_none());
    }

    #[test]
    fn rejects_extra_segments() {
        assert!(TileCoord::parse("/3/4/5/6.png").is_none());
    }

    #[test]
    fn rejects_missing_png_suffix() {
        assert!(TileCoord::parse("/3/4/5").is_none());
        assert!(TileCoord::parse("/3/4/5.jpg").is_none());
    }

    #[test]
    fn rejects_non_numeric_segments() {
        assert!(TileCoord::parse("/z/4/5.png").is_none());
        assert!(TileCoord::parse("/3/x/5.png").is_none());
        assert!(TileCoord::parse("/3/4/y.png").is_none());
        assert!(TileCoord::parse("/3.5/4/5.png").is_none());
        assert!(TileCoord::parse("/-1/4/5.png").is_none());
    }

    #[test]
    fn rejects_zoom_above_max() {
        assert!(TileCoord::parse("/20/0/0.png").is_none());
        assert!(TileCoord::parse("/255/0/0.png").is_none());
        assert!(TileCoord::parse("/19/0/0.png").is_some());
    }

    #[test]
    fn rejects_x_y_out_of_range_for_zoom() {
        // At z=3 the grid is 8x8 (0..=7).
        assert!(TileCoord::parse("/3/8/0.png").is_none());
        assert!(TileCoord::parse("/3/0/8.png").is_none());
        assert!(TileCoord::parse("/3/7/7.png").is_some());
        // z=0 is the single whole-world tile: only (0,0) is valid.
        assert!(TileCoord::parse("/0/0/0.png").is_some());
        assert!(TileCoord::parse("/0/1/0.png").is_none());
    }

    #[test]
    fn rejects_path_traversal_attempts() {
        assert!(TileCoord::parse("/../../etc/passwd").is_none());
        assert!(TileCoord::parse("/3/../5.png").is_none());
        assert!(TileCoord::parse("/3/4/../5.png").is_none());
        assert!(TileCoord::parse("/3/4/..%2f..%2f5.png").is_none());
        assert!(TileCoord::parse("/3/4/5.png/../../secret").is_none());
    }

    #[test]
    fn cache_path_is_confined_to_cache_dir_and_matches_layout() {
        let coord = TileCoord::parse("/12/2050/1400.png").unwrap();
        let cache_dir = Path::new("/app/cache");
        let path = coord.cache_path(cache_dir);
        assert_eq!(path, Path::new("/app/cache/tiles/12/2050/1400.png"));
        assert!(path.starts_with(cache_dir));
    }

    #[test]
    fn upstream_url_uses_validated_integers() {
        let coord = TileCoord::parse("/12/2050/1400.png").unwrap();
        assert_eq!(
            coord.upstream_url(),
            "https://tile.openstreetmap.org/12/2050/1400.png"
        );
    }

    // --- plan_prune ---

    fn entry(name: &str, size_bytes: u64, age: Duration, now: SystemTime) -> CacheEntry {
        CacheEntry {
            path: PathBuf::from(name),
            size_bytes,
            modified: now - age,
        }
    }

    #[test]
    fn empty_input_prunes_nothing() {
        let now = SystemTime::now();
        let to_remove = plan_prune(&[], now, MAX_TILE_AGE, MAX_CACHE_BYTES);
        assert!(to_remove.is_empty());
    }

    #[test]
    fn nothing_over_either_limit_removes_nothing() {
        let now = SystemTime::now();
        let entries = vec![
            entry("a.png", 1024, Duration::from_secs(60), now),
            entry("b.png", 2048, Duration::from_secs(3600), now),
        ];
        let to_remove = plan_prune(&entries, now, MAX_TILE_AGE, MAX_CACHE_BYTES);
        assert!(to_remove.is_empty());
    }

    #[test]
    fn over_age_entry_is_removed_even_when_well_under_the_size_budget() {
        let now = SystemTime::now();
        let stale = entry(
            "stale.png",
            1024,
            Duration::from_secs(31 * 24 * 60 * 60),
            now,
        );
        let fresh = entry("fresh.png", 1024, Duration::from_secs(60), now);
        let entries = vec![stale.clone(), fresh];

        let to_remove = plan_prune(&entries, now, MAX_TILE_AGE, MAX_CACHE_BYTES);

        assert_eq!(to_remove, vec![stale.path]);
    }

    #[test]
    fn entry_exactly_at_the_age_limit_is_kept() {
        // `duration_since` == max_age is not `>` max_age, so this is a
        // boundary check that the comparison is strictly-greater-than, not
        // greater-or-equal.
        let now = SystemTime::now();
        let entries = vec![entry("boundary.png", 1024, MAX_TILE_AGE, now)];
        let to_remove = plan_prune(&entries, now, MAX_TILE_AGE, MAX_CACHE_BYTES);
        assert!(to_remove.is_empty());
    }

    #[test]
    fn over_size_removes_oldest_first_until_under_budget() {
        let now = SystemTime::now();
        // Three same-age-bracket (all well under MAX_TILE_AGE) entries whose
        // combined size exceeds a small budget; oldest-modified should go
        // first, and only as many as needed to get under budget.
        let oldest = entry("oldest.png", 40, Duration::from_secs(300), now);
        let middle = entry("middle.png", 40, Duration::from_secs(200), now);
        let newest = entry("newest.png", 40, Duration::from_secs(100), now);
        let entries = vec![newest.clone(), oldest.clone(), middle.clone()];

        // Budget of 50 bytes: total is 120, so entries must be dropped until
        // at or under 50 — dropping just `oldest` (leaving 80) isn't enough,
        // dropping `oldest` + `middle` (leaving 40) is.
        let to_remove = plan_prune(&entries, now, MAX_TILE_AGE, 50);

        assert_eq!(to_remove, vec![oldest.path, middle.path]);
        assert!(!to_remove.contains(&newest.path));
    }

    #[test]
    fn over_size_and_over_age_combine_without_double_counting() {
        let now = SystemTime::now();
        let stale = entry(
            "stale.png",
            1000,
            Duration::from_secs(40 * 24 * 60 * 60),
            now,
        );
        let big_fresh = entry("big_fresh.png", 1000, Duration::from_secs(60), now);
        let entries = vec![stale.clone(), big_fresh.clone()];

        // `stale` is removed by the age pass; the size pass then only has
        // `big_fresh` left, which alone is already under this budget, so it
        // must not also be removed.
        let to_remove = plan_prune(&entries, now, MAX_TILE_AGE, 1000);

        assert_eq!(to_remove, vec![stale.path]);
        assert!(!to_remove.contains(&big_fresh.path));
    }
}
