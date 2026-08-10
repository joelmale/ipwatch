import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

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

function setErrorBanner(message: string | null): void {
  if (message) {
    errorBanner.textContent = message;
    errorBanner.hidden = false;
  } else {
    errorBanner.hidden = true;
    errorBanner.textContent = "";
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
  } catch (err) {
    hasLoadedOnce = true;
    setErrorBanner(`Could not load details: ${String(err)}`);
    render();
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

  void (async () => {
    await registerListeners();
    await loadDetails();
  })();
});
