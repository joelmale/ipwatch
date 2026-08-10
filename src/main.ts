import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import L from "leaflet";
import "leaflet/dist/leaflet.css";

// ---------------------------------------------------------------------------
// Backend payload shapes. These mirror the Rust structs in
// src-tauri/src/{poller,providers,netinfo,app}/mod.rs exactly. Every field
// that is `Option<T>` on the Rust side is nullable here, and every place that
// value is rendered must fall back to a placeholder — free geo API tiers omit
// fields unpredictably.
// ---------------------------------------------------------------------------

interface GeoInfo {
  ip: string | null;
  country: string | null;
  country_code: string | null;
  region: string | null;
  city: string | null;
  lat: number | null;
  lon: number | null;
  timezone: string | null;
  isp: string | null;
  org: string | null;
  asn: string | null;
}

interface Snapshot {
  ip: string;
  geo: GeoInfo;
  /** Unix seconds, not milliseconds. */
  observed_at: number;
}

interface NetInfo {
  internal_ips: string[];
  dns_servers: string[];
  hostname: string | null;
}

interface Details {
  snapshot: Snapshot | null;
  netinfo: NetInfo;
  online: boolean;
}

type ChangeReason =
  | "initial"
  | "ip_changed"
  | "country_changed"
  | "isp_changed"
  | "offline";

interface Change {
  previous: Snapshot | null;
  current: Snapshot | null;
  reason: ChangeReason;
}

interface IpEvent {
  id: number | null;
  /** Unix seconds, not milliseconds. */
  ts: number;
  external_ip: string;
  country: string | null;
  country_code: string | null;
  isp: string | null;
  change_reason: ChangeReason;
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/** Last snapshot we have any knowledge of, live or stale. `null` = first run. */
let lastSnapshot: Snapshot | null = null;
let online = false;
/** True once `get_details` (or an `ip-changed` event) has resolved at least once. */
let hasLoadedOnce = false;
/** Guards `refresh-done` never arriving (e.g. dropped event) from wedging the button forever. */
let refreshWatchdog: ReturnType<typeof setTimeout> | null = null;

const PLACEHOLDER = "—";

/** How many rows to pull from `get_history`. This is a details window, not
 * a paged log — 50 is enough to see the recent pattern without the panel
 * growing unbounded. */
const HISTORY_LIMIT = 50;

const REASON_LABELS: Record<ChangeReason, string> = {
  initial: "Initial",
  ip_changed: "IP changed",
  country_changed: "Country changed",
  isp_changed: "ISP changed",
  offline: "Offline",
};

// ---------------------------------------------------------------------------
// Map pane state
//
// The map is lazy in the strongest sense that matters here: the Leaflet
// instance is not constructed, and no tile URL is ever built, until the user
// opens the pane at least once. Tiles are proxied through Rust (see
// src-tauri/src/app/tiles.rs) precisely so the webview never talks to a
// third party directly — this is a VPN-verification tool, so leaking the
// user's current exit-node coordinates to an OSM tile server on every
// background poll would defeat the point. Keeping map creation gated behind
// `mapPaneOpen` is what makes that guarantee hold for background IP changes.
// ---------------------------------------------------------------------------

/** Windows/Android serve custom schemes at `http://<scheme>.localhost/…`;
 * macOS/Linux/iOS serve them at `<scheme>://localhost/…`. Only the Windows
 * form is exercised in this app's CSP (see tauri.conf.json's `img-src`) —
 * Windows is the primary target — but branching here is cheap and keeps the
 * non-Windows path from being silently broken in an obviously wrong way. */
const TILE_URL_TEMPLATE = navigator.userAgent.includes("Windows")
  ? "http://tiles.localhost/{z}/{x}/{y}.png"
  : "tiles://localhost/{z}/{x}/{y}.png";

const MAP_ATTRIBUTION =
  '&copy; <a href="https://www.openstreetmap.org/copyright">OpenStreetMap</a> contributors';

let leafletMap: L.Map | null = null;
let mapMarker: L.CircleMarker | null = null;
let mapPaneOpen = false;

// ---------------------------------------------------------------------------
// DOM handles
// ---------------------------------------------------------------------------

let refreshBtn: HTMLButtonElement;
let statusPill: HTMLElement;
let statusText: HTMLElement;
let staleBanner: HTMLElement;
let errorBanner: HTMLElement;
let headlineIp: HTMLElement;
let headlineCountry: HTMLElement;
let headlineMeta: HTMLElement;

let fIp: HTMLElement;
let fCountry: HTMLElement;
let fRegion: HTMLElement;
let fIsp: HTMLElement;
let fOrg: HTMLElement;
let fAsn: HTMLElement;
let fTimezone: HTMLElement;
let fCoords: HTMLElement;
let fHostname: HTMLElement;
let fInternalIps: HTMLElement;
let fDnsServers: HTMLElement;

let mapToggleBtn: HTMLButtonElement;
let mapPaneEl: HTMLElement;
let mapUnavailableEl: HTMLElement;
let mapContainer: HTMLElement;

let historyErrorEl: HTMLElement;
let historyEmptyEl: HTMLElement;
let historyTableWrap: HTMLElement;
let historyBody: HTMLElement;

function queryEl<T extends HTMLElement>(selector: string): T {
  const el = document.querySelector<T>(selector);
  if (!el) {
    throw new Error(`ipwatch: expected element ${selector} to exist`);
  }
  return el;
}

function bindDom(): void {
  refreshBtn = queryEl("#refresh-btn");
  statusPill = queryEl("#status-pill");
  statusText = queryEl("#status-text");
  staleBanner = queryEl("#stale-banner");
  errorBanner = queryEl("#error-banner");
  headlineIp = queryEl("#headline-ip");
  headlineCountry = queryEl("#headline-country");
  headlineMeta = queryEl("#headline-meta");

  fIp = queryEl("#f-ip");
  fCountry = queryEl("#f-country");
  fRegion = queryEl("#f-region");
  fIsp = queryEl("#f-isp");
  fOrg = queryEl("#f-org");
  fAsn = queryEl("#f-asn");
  fTimezone = queryEl("#f-timezone");
  fCoords = queryEl("#f-coords");
  fHostname = queryEl("#f-hostname");
  fInternalIps = queryEl("#f-internal-ips");
  fDnsServers = queryEl("#f-dns-servers");

  mapToggleBtn = queryEl("#map-toggle-btn");
  mapPaneEl = queryEl("#map-pane");
  mapUnavailableEl = queryEl("#map-unavailable");
  mapContainer = queryEl("#map");

  historyErrorEl = queryEl("#history-error");
  historyEmptyEl = queryEl("#history-empty");
  historyTableWrap = queryEl("#history-table-wrap");
  historyBody = queryEl("#history-body");
}

// ---------------------------------------------------------------------------
// Rendering helpers
// ---------------------------------------------------------------------------

function text(value: string | null | undefined): string {
  return value === null || value === undefined || value === "" ? PLACEHOLDER : value;
}

function joinOrPlaceholder(values: string[]): string {
  return values.length > 0 ? values.join(", ") : PLACEHOLDER;
}

function formatRegionCity(geo: GeoInfo): string {
  const parts = [geo.city, geo.region].filter((p): p is string => !!p);
  return parts.length > 0 ? parts.join(", ") : PLACEHOLDER;
}

function formatCountry(geo: GeoInfo): string {
  if (geo.country && geo.country_code) return `${geo.country} (${geo.country_code})`;
  if (geo.country) return geo.country;
  if (geo.country_code) return geo.country_code;
  return PLACEHOLDER;
}

function formatCoords(geo: GeoInfo): string {
  if (geo.lat === null || geo.lon === null) return PLACEHOLDER;
  return `${geo.lat.toFixed(4)}, ${geo.lon.toFixed(4)}`;
}

function formatObservedAt(observedAtSeconds: number): string {
  return new Date(observedAtSeconds * 1000).toLocaleString();
}

/** Coarse "Xh ago" hint. This is a supplement to the absolute timestamp,
 * never a replacement — reviewing the log a day later, "2h ago" on its own
 * is meaningless. */
function formatRelativeTime(unixSeconds: number): string {
  const diffSec = Math.round((Date.now() - unixSeconds * 1000) / 1000);
  if (diffSec < 5) return "just now";
  if (diffSec < 60) return `${diffSec}s ago`;
  const diffMin = Math.round(diffSec / 60);
  if (diffMin < 60) return `${diffMin}m ago`;
  const diffHr = Math.round(diffMin / 60);
  if (diffHr < 24) return `${diffHr}h ago`;
  const diffDay = Math.round(diffHr / 24);
  return `${diffDay}d ago`;
}

function formatEventCountry(ev: IpEvent): string {
  if (ev.country && ev.country_code) return `${ev.country} (${ev.country_code})`;
  if (ev.country) return ev.country;
  if (ev.country_code) return ev.country_code;
  return PLACEHOLDER;
}

function setErrorBanner(message: string | null): void {
  if (message) {
    errorBanner.textContent = message;
    errorBanner.hidden = false;
  } else {
    errorBanner.hidden = true;
    errorBanner.textContent = "";
  }
}

/** Scoped to the history panel so a DB hiccup there (the backend degrades
 * to "no history" rather than failing startup) never clobbers the
 * top-level error banner or the rest of the window. */
function setHistoryError(message: string | null): void {
  if (message) {
    historyErrorEl.textContent = message;
    historyErrorEl.hidden = false;
  } else {
    historyErrorEl.hidden = true;
    historyErrorEl.textContent = "";
  }
}

function renderHistoryRows(events: IpEvent[]): void {
  historyBody.textContent = "";

  for (const ev of events) {
    const tr = document.createElement("tr");

    const tdTime = document.createElement("td");
    tdTime.className = "history-table__time";
    const abs = document.createElement("span");
    abs.className = "history-time-abs";
    abs.textContent = formatObservedAt(ev.ts);
    const rel = document.createElement("span");
    rel.className = "history-time-rel";
    rel.textContent = formatRelativeTime(ev.ts);
    tdTime.append(abs, rel);

    const tdIp = document.createElement("td");
    tdIp.className = "history-table__ip";
    tdIp.textContent = text(ev.external_ip);

    const tdCountry = document.createElement("td");
    tdCountry.textContent = formatEventCountry(ev);

    const tdIsp = document.createElement("td");
    tdIsp.textContent = text(ev.isp);

    const tdReason = document.createElement("td");
    const badge = document.createElement("span");
    badge.className = `reason-badge reason-badge--${ev.change_reason}`;
    badge.textContent = REASON_LABELS[ev.change_reason] ?? ev.change_reason;
    tdReason.appendChild(badge);

    tr.append(tdTime, tdIp, tdCountry, tdIsp, tdReason);
    historyBody.appendChild(tr);
  }
}

/** Renders the "no snapshot yet" first-run state instead of a wall of placeholders. */
function renderCheckingState(): void {
  headlineIp.textContent = "Checking…";
  headlineCountry.textContent = "Running the first lookup — this can take a few seconds.";
  headlineMeta.textContent = "";
  headlineIp.classList.add("headline__ip--pending");

  statusPill.className = "status-pill status-pill--checking";
  statusText.textContent = "Checking…";

  staleBanner.hidden = true;

  for (const el of [fIp, fCountry, fRegion, fIsp, fOrg, fAsn, fTimezone, fCoords]) {
    el.textContent = PLACEHOLDER;
  }
}

function renderSnapshot(snapshot: Snapshot, isOnline: boolean): void {
  headlineIp.classList.remove("headline__ip--pending");
  headlineIp.textContent = snapshot.ip;
  headlineCountry.textContent = formatCountry(snapshot.geo);
  headlineMeta.textContent = `Observed ${formatObservedAt(snapshot.observed_at)}`;

  fIp.textContent = text(snapshot.ip);
  fCountry.textContent = formatCountry(snapshot.geo);
  fRegion.textContent = formatRegionCity(snapshot.geo);
  fIsp.textContent = text(snapshot.geo.isp);
  fOrg.textContent = text(snapshot.geo.org);
  fAsn.textContent = text(snapshot.geo.asn);
  fTimezone.textContent = text(snapshot.geo.timezone);
  fCoords.textContent = formatCoords(snapshot.geo);

  if (isOnline) {
    statusPill.className = "status-pill status-pill--online";
    statusText.textContent = "Online";
    staleBanner.hidden = true;
  } else {
    statusPill.className = "status-pill status-pill--offline";
    statusText.textContent = "Offline";
    staleBanner.hidden = false;
    headlineMeta.textContent = `Last known-good reading: ${formatObservedAt(snapshot.observed_at)}`;
  }
}

function renderNetInfo(netinfo: NetInfo): void {
  fHostname.textContent = text(netinfo.hostname);
  fInternalIps.textContent = joinOrPlaceholder(netinfo.internal_ips);
  fDnsServers.textContent = joinOrPlaceholder(netinfo.dns_servers);
}

function render(): void {
  if (!hasLoadedOnce) {
    renderCheckingState();
    return;
  }

  if (lastSnapshot) {
    renderSnapshot(lastSnapshot, online);
  } else {
    // Loaded, but still no snapshot (e.g. first poll hasn't landed, or every
    // provider failed on the very first attempt): stay in the checking state
    // rather than showing placeholders that look like a broken UI.
    renderCheckingState();
    if (!online) {
      statusPill.className = "status-pill status-pill--offline";
      statusText.textContent = "Offline";
      staleBanner.hidden = false;
      headlineCountry.textContent = "No successful lookup yet.";
    }
  }
}

// ---------------------------------------------------------------------------
// Map pane
//
// `ensureLeafletMap` is the only place a `L.Map` gets constructed, and it is
// only ever called from `renderMap`, which itself bails out at the top when
// the pane is closed. That means: with the pane closed, this file never
// calls `L.map(...)`, never builds a tile URL, and never adds a tile layer —
// so no tile request can be issued. See the state-block comment above for
// why that property matters here.
// ---------------------------------------------------------------------------

function ensureLeafletMap(): L.Map {
  if (leafletMap) {
    return leafletMap;
  }

  const map = L.map(mapContainer).setView([0, 0], 2);
  L.tileLayer(TILE_URL_TEMPLATE, {
    maxZoom: 19,
    attribution: MAP_ATTRIBUTION,
  }).addTo(map);

  leafletMap = map;
  return map;
}

/**
 * Renders the map pane against `lastSnapshot`. Must only be called while the
 * pane is open — callers (the toggle handler, the `ip-changed` listener,
 * `loadDetails`) are responsible for checking `mapPaneOpen` first, but this
 * also re-checks itself so a stray call can never sneak a tile fetch in
 * while the pane is closed.
 */
function renderMap(): void {
  if (!mapPaneOpen) {
    return;
  }

  const geo = lastSnapshot?.geo;
  if (!geo || geo.lat === null || geo.lon === null) {
    mapUnavailableEl.hidden = false;
    mapContainer.hidden = true;
    return;
  }

  mapUnavailableEl.hidden = true;
  mapContainer.hidden = false;

  const map = ensureLeafletMap();
  const latLng: L.LatLngExpression = [geo.lat, geo.lon];
  const zoom = Math.max(map.getZoom(), 10);
  map.setView(latLng, zoom);

  // The container may have just gone from `hidden` to visible; Leaflet
  // measured a 0x0 box while it was hidden, so tiles need a recalculation
  // once the browser has actually laid the pane out.
  requestAnimationFrame(() => map.invalidateSize());

  if (mapMarker) {
    mapMarker.setLatLng(latLng);
  } else {
    mapMarker = L.circleMarker(latLng, {
      radius: 8,
      weight: 2,
      color: "#2f6fed",
      fillColor: "#2f6fed",
      fillOpacity: 0.85,
    }).addTo(map);
  }
}

function setMapPaneOpen(open: boolean): void {
  mapPaneOpen = open;
  mapPaneEl.hidden = !open;
  mapToggleBtn.textContent = open ? "Hide map" : "Show map";
  mapToggleBtn.setAttribute("aria-expanded", String(open));

  if (open) {
    renderMap();
  }
}

function onMapToggleClick(): void {
  setMapPaneOpen(!mapPaneOpen);
}

// ---------------------------------------------------------------------------
// Backend calls
// ---------------------------------------------------------------------------

async function loadDetails(): Promise<void> {
  try {
    const details = await invoke<Details>("get_details");
    lastSnapshot = details.snapshot;
    online = details.online;
    hasLoadedOnce = true;
    setErrorBanner(null);
    render();
    renderNetInfo(details.netinfo);
    if (mapPaneOpen) {
      renderMap();
    }
  } catch (err) {
    hasLoadedOnce = true;
    setErrorBanner(`Could not load details: ${String(err)}`);
    render();
  }
}

/** Loads the history panel. Called on startup and re-called whenever an
 * `ip-changed` event lands, since that's exactly when a new `ip_events`
 * row exists to show. Never polled. */
async function loadHistory(): Promise<void> {
  try {
    const events = await invoke<IpEvent[]>("get_history", { limit: HISTORY_LIMIT });
    setHistoryError(null);
    if (events.length === 0) {
      historyEmptyEl.hidden = false;
      historyTableWrap.hidden = true;
    } else {
      historyEmptyEl.hidden = true;
      historyTableWrap.hidden = false;
      renderHistoryRows(events);
    }
  } catch (err) {
    setHistoryError(`Could not load history: ${String(err)}`);
    historyEmptyEl.hidden = true;
    historyTableWrap.hidden = true;
  }
}

function clearRefreshWatchdog(): void {
  if (refreshWatchdog !== null) {
    clearTimeout(refreshWatchdog);
    refreshWatchdog = null;
  }
}

function setRefreshing(isRefreshing: boolean): void {
  refreshBtn.disabled = isRefreshing;
  refreshBtn.textContent = isRefreshing ? "Refreshing…" : "Refresh";
}

async function onRefreshClick(): Promise<void> {
  setRefreshing(true);
  clearRefreshWatchdog();
  // Guards against a dropped/never-arriving refresh-done event wedging the
  // button in "Refreshing…" forever.
  refreshWatchdog = setTimeout(() => {
    setRefreshing(false);
    refreshWatchdog = null;
  }, 15000);

  try {
    await invoke("refresh");
  } catch (err) {
    setErrorBanner(`Refresh failed: ${String(err)}`);
    setRefreshing(false);
    clearRefreshWatchdog();
  }
}

// ---------------------------------------------------------------------------
// Event listeners — registered before the first get_details() await so a
// change landing mid-startup is never missed.
// ---------------------------------------------------------------------------

async function registerListeners(): Promise<void> {
  await listen<Change>("ip-changed", (event) => {
    const change = event.payload;
    hasLoadedOnce = true;
    if (change.current) {
      lastSnapshot = change.current;
    }
    online = change.reason !== "offline";
    setErrorBanner(null);
    render();
    // Gated on mapPaneOpen inside renderMap() too, but checked here as well
    // so a closed pane never even reaches the function on a background
    // ip-changed event — that's the point in the state-block comment above.
    if (mapPaneOpen) {
      renderMap();
    }
    // This event is exactly when a new ip_events row exists — re-pull
    // rather than polling.
    void loadHistory();
  });

  await listen("refresh-started", () => {
    setRefreshing(true);
  });

  await listen("refresh-done", () => {
    clearRefreshWatchdog();
    setRefreshing(false);
    // The refresh may have produced a fresh snapshot without a Change event
    // (e.g. nothing worth reporting changed) — re-pull details so the
    // "observed at" timestamp still moves forward.
    void loadDetails();
  });
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

window.addEventListener("DOMContentLoaded", () => {
  bindDom();
  render();
  refreshBtn.addEventListener("click", () => void onRefreshClick());
  mapToggleBtn.addEventListener("click", onMapToggleClick);

  void (async () => {
    await registerListeners();
    await loadDetails();
    await loadHistory();
  })();
});
