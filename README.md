# ipwatch

[![build](https://github.com/joelmale/ipwatch/actions/workflows/build.yml/badge.svg)](https://github.com/joelmale/ipwatch/actions/workflows/build.yml)

Cross-platform rewrite of [IPmonitor](https://github.com/joelmale/IPmonitor) (macOS/Swift) — a system-tray utility that monitors your external IP address so you can verify your VPN is actually working, regardless of VPN type (WireGuard, NordVPN, OpenVPN, etc.).

## What it does

- **System tray** (Windows-first): current external IP and country at a glance, with the icon colour carrying status — green when online, amber once the country changes, grey when offline, and a distinct state before the first reading arrives
- **Details window**: external IP, internal IPs, DNS servers, hostname, ISP/ASN, geolocation, timezone, map
- **VPN-drop notification**: native toast when your country or ISP changes, or when connectivity is lost
- **DNS leak check**: on-demand test of which resolvers your queries actually exit through
- **History**: SQLite log of IP changes with timestamps
- **Settings**: poll interval, notifications, launch at startup, expected country

## Stack

Tauri 2 (Rust backend, web frontend). Windows is the primary target with full tray integration; macOS and Linux run as standard windowed apps.

All network access lives in the Rust backend — the webview makes no outbound
requests, and the CSP enforces it. Map tiles are fetched and cached by Rust and
served over a custom URI scheme rather than by the webview, which also lets
ipwatch honour OpenStreetMap's User-Agent and caching policy. The map is lazy: no
tile is requested until you open the pane, so a VPN-verification tool isn't
handing a third party your exit-node coordinates on every IP change.

## Install

Builds for all three platforms are produced on every push. Grab one from the
[latest build](https://github.com/joelmale/ipwatch/actions/workflows/build.yml)
— open the most recent green run and download the artifact for your platform:

| Platform | Artifact |
|----------|----------|
| Windows  | `.msi` or `-setup.exe` (NSIS) |
| macOS    | `.dmg` |
| Linux    | `.deb`, `.rpm`, or `.AppImage` |

The installers are **not code-signed**, so Windows SmartScreen and macOS
Gatekeeper will warn on first run.

## Development

```sh
# Prerequisites: Rust (rustup) and Node.js.
# Windows: MSVC build tools + WebView2 (preinstalled on Win 10/11).
# Linux:   libwebkit2gtk-4.1-dev, libayatana-appindicator3-dev, librsvg2-dev,
#          libxdo-dev, libssl-dev, patchelf (see .github/workflows/build.yml).
npm install
npm run tauri dev
```

```sh
# Backend tests
cargo test --manifest-path src-tauri/Cargo.toml
```

```sh
# One-shot probe against the real endpoints, printing what ipwatch sees.
# The unit tests are fully mocked; this is what catches upstream API drift.
cargo run --manifest-path src-tauri/Cargo.toml --example probe
```

Set `RUST_LOG=ipwatch=debug` for verbose backend logging.

**Note on notifications during development:** the notification plugin skips
setting the AppUserModelID for executables under `target/`, so toasts launched
via `tauri dev` appear branded as "Windows PowerShell". Content and timing are
testable in dev; correct branding requires an installed build.

## License

MIT
