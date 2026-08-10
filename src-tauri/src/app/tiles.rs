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

use reqwest::Client;
use tauri::http::{header, Request, Response, StatusCode};
use tauri::{Builder, Manager, Runtime};

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
}
