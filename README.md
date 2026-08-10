# ipwatch

Cross-platform rewrite of [IPmonitor](https://github.com/joelmale/IPmonitor) (macOS/Swift) — a system-tray utility that monitors your external IP address so you can verify your VPN is actually working, regardless of VPN type (WireGuard, NordVPN, OpenVPN, etc.).

## What it does

- **System tray** (Windows-first): country flag + current external IP at a glance
- **Details window**: external IP, internal IP, DNS servers, hostname, ISP/ASN, geolocation, timezone, map
- **VPN-drop notification**: native toast when your external IP or country changes unexpectedly
- **DNS leak check**: verifies which resolvers your queries actually exit through
- **History**: SQLite log of IP changes with timestamps

## Stack

Tauri 2 (Rust backend, web frontend). Windows is the primary target with full tray integration; macOS and Linux run as standard windowed apps.

## Status

Early development. The Rust core (IP/geolocation providers with failover, local network info, SQLite event log, polling) is being built first; tray and UI follow.

## Development

```sh
# Prerequisites: Rust (rustup), Node.js, and on Windows the MSVC build tools + WebView2 (preinstalled on Win 10/11)
npm install
npm run tauri dev
```

## License

MIT
