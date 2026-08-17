// ---------------------------------------------------------------------------
// Helpers shared between the two webview entry points (`main.ts` for the
// details window, `settings.ts` for the standalone settings window). Keep
// this file limited to things both pages actually use — anything specific
// to one page belongs in that page's own module, not here.
// ---------------------------------------------------------------------------

/**
 * Backend payload shape. Mirrors `src-tauri/src/settings/mod.rs::Settings`
 * exactly. Field names are snake_case to match serde's default
 * (de)serialization — Tauri does NOT rename struct fields, only command
 * *arguments* get the snake_case-to-camelCase treatment.
 *
 * Lives here rather than in either page because both entry points now send it:
 * `settings.ts` from the settings window, `main.ts` from the first-run card.
 * `set_settings` replaces the whole stored struct, so a page that sends an
 * object missing a field silently resets that field — one shared definition
 * means adding a field to the Rust struct produces a type error in every
 * sender, instead of a silent reset in whichever copy was forgotten.
 */
export interface Settings {
  poll_interval_secs: number;
  notifications_enabled: boolean;
  launch_at_startup: boolean;
  expected_country_code: string | null;
  start_minimised: boolean;
  onboarding_completed: boolean;
}

export const PLACEHOLDER = "—";

export function text(value: string | null | undefined): string {
  return value === null || value === undefined || value === "" ? PLACEHOLDER : value;
}

export function queryEl<T extends HTMLElement>(selector: string): T {
  const el = document.querySelector<T>(selector);
  if (!el) {
    throw new Error(`ipwatch: expected element ${selector} to exist`);
  }
  return el;
}

/** The app's recurring "scoped inline message" pattern: a hidden-by-default
 * element that shows `message` as its text content, or hides and clears
 * itself when `message` is `null`. Used for error banners, success
 * confirmations, and similar per-panel status text — never `alert()`. */
export function setScopedMessage(el: HTMLElement, message: string | null): void {
  if (message) {
    el.textContent = message;
    el.hidden = false;
  } else {
    el.hidden = true;
    el.textContent = "";
  }
}
