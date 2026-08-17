import { invoke } from "@tauri-apps/api/core";
import { queryEl, setScopedMessage, type Settings } from "./shared";

// `onboarding_completed` is not surfaced in this UI (that's the first-run
// card in `main.ts`), but `set_settings` replaces the whole stored struct, so
// omitting it here would silently reset it to `false` on every save.
// `readSettingsForm` carries the last known value through unchanged.

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/** The settings baseline: the last value returned by `get_settings` or
 * `set_settings`. Dirty-checking compares the live form against this, never
 * against what was last typed — that's what lets the save button reflect
 * "does the form match what's actually persisted/applied" rather than
 * "has the user touched anything". `null` until the initial load resolves. */
let currentSettings: Settings | null = null;
/** True while a save is in flight, so a stray double-submit can't fire twice. */
let settingsSaving = false;

/** UI-only fallback used when the poll interval field can't be parsed as a
 * number at all (e.g. emptied). Mirrors `settings::DEFAULT_POLL_INTERVAL_SECS`
 * for display purposes only — the backend is the sole source of truth for
 * clamping and defaults; this just keeps the dirty-check from throwing NaN
 * around before the round trip corrects it. */
const FALLBACK_POLL_INTERVAL_SECS = 60;

// ---------------------------------------------------------------------------
// DOM handles
// ---------------------------------------------------------------------------

let settingsForm: HTMLFormElement;
let settingsErrorEl: HTMLElement;
let settingsSuccessEl: HTMLElement;
let settingsPollIntervalInput: HTMLInputElement;
let settingsNotificationsInput: HTMLInputElement;
let settingsLaunchStartupInput: HTMLInputElement;
let settingsStartMinimisedInput: HTMLInputElement;
let settingsCountryCodeInput: HTMLInputElement;
let settingsSaveBtn: HTMLButtonElement;
let settingsClampNoteEl: HTMLElement;

function bindDom(): void {
  settingsForm = queryEl("#settings-form");
  settingsErrorEl = queryEl("#settings-error");
  settingsSuccessEl = queryEl("#settings-success");
  settingsPollIntervalInput = queryEl("#settings-poll-interval");
  settingsNotificationsInput = queryEl("#settings-notifications");
  settingsLaunchStartupInput = queryEl("#settings-launch-startup");
  settingsStartMinimisedInput = queryEl("#settings-start-minimised");
  settingsCountryCodeInput = queryEl("#settings-country-code");
  settingsSaveBtn = queryEl("#settings-save-btn");
  settingsClampNoteEl = queryEl("#settings-clamp-note");
}

// ---------------------------------------------------------------------------
// Settings form
//
// Explicit Save button, enabled only when the form differs from
// `currentSettings` (the last value the backend actually confirmed via
// `get_settings`/`set_settings`) — never save-on-change. Four fields with a
// mix of immediate-effect semantics is exactly the case where silently
// firing a backend call per keystroke would make it unclear what's live, so
// a single deliberate action with an unambiguous enabled/disabled affordance
// was chosen instead. `set_settings` returns the *effective* (post-clamp)
// settings, and the form is always re-rendered from that response, never
// from what was typed — see `onSettingsFormSubmit`.
// ---------------------------------------------------------------------------

function setSettingsError(message: string | null): void {
  setScopedMessage(settingsErrorEl, message);
}

function setSettingsSuccess(show: boolean): void {
  settingsSuccessEl.hidden = !show;
}

function hideSettingsClampNote(): void {
  settingsClampNoteEl.hidden = true;
  settingsClampNoteEl.textContent = "";
}

function showSettingsClampNote(requestedSecs: number, effectiveSecs: number): void {
  settingsClampNoteEl.textContent =
    `Poll interval adjusted to ${effectiveSecs}s — ${requestedSecs}s is outside the allowed ` +
    "10s–3600s range.";
  settingsClampNoteEl.hidden = false;
}

/** Renders the form from an authoritative `Settings` value (from
 * `get_settings` or `set_settings`'s response) and updates the dirty-check
 * baseline to match. Never call this with unconfirmed user input. */
function renderSettingsForm(settings: Settings): void {
  currentSettings = settings;
  settingsPollIntervalInput.value = String(settings.poll_interval_secs);
  settingsNotificationsInput.checked = settings.notifications_enabled;
  settingsLaunchStartupInput.checked = settings.launch_at_startup;
  settingsStartMinimisedInput.checked = settings.start_minimised;
  settingsCountryCodeInput.value = settings.expected_country_code ?? "";
  updateSettingsSaveButton();
}

/** Reads the live form into a `Settings`-shaped payload. This is a *request*,
 * not a fact — the backend clamps `poll_interval_secs` and is the only
 * source of truth for what actually took effect. */
function readSettingsForm(): Settings {
  const rawInterval = Number(settingsPollIntervalInput.value);
  const pollIntervalSecs = Number.isFinite(rawInterval)
    ? Math.trunc(rawInterval)
    : (currentSettings?.poll_interval_secs ?? FALLBACK_POLL_INTERVAL_SECS);

  const countryCode = settingsCountryCodeInput.value.trim().toUpperCase();

  return {
    poll_interval_secs: pollIntervalSecs,
    notifications_enabled: settingsNotificationsInput.checked,
    launch_at_startup: settingsLaunchStartupInput.checked,
    start_minimised: settingsStartMinimisedInput.checked,
    // Empty means "no expectation" — must be `null`, not `""`, to match the
    // backend's `Option<String>` contract.
    expected_country_code: countryCode === "" ? null : countryCode,
    // `set_settings` takes the full `Settings` object and overwrites whatever
    // is currently stored, and there is no UI for this field here (brief
    // 6.3). Carry the last-known value through unchanged so a save from this
    // form can never silently reset it to `false`.
    onboarding_completed: currentSettings?.onboarding_completed ?? false,
  };
}

function settingsEqual(a: Settings, b: Settings): boolean {
  return (
    a.poll_interval_secs === b.poll_interval_secs &&
    a.notifications_enabled === b.notifications_enabled &&
    a.launch_at_startup === b.launch_at_startup &&
    a.start_minimised === b.start_minimised &&
    a.expected_country_code === b.expected_country_code &&
    a.onboarding_completed === b.onboarding_completed
  );
}

function isSettingsFormDirty(): boolean {
  return currentSettings !== null && !settingsEqual(readSettingsForm(), currentSettings);
}

function updateSettingsSaveButton(): void {
  settingsSaveBtn.disabled = settingsSaving || !isSettingsFormDirty();
}

/** Bound to `input`/`change` on every settings field. Saved confirmation and
 * the clamp note both describe a *specific* past save — the moment the form
 * changes again they no longer describe the live form, so both clear here
 * rather than lingering next to edits they no longer apply to. */
function onSettingsFieldChange(): void {
  setSettingsSuccess(false);
  hideSettingsClampNote();
  updateSettingsSaveButton();
}

async function onSettingsFormSubmit(event: SubmitEvent): Promise<void> {
  event.preventDefault();
  if (settingsSaving) {
    return;
  }

  const payload = readSettingsForm();

  settingsSaving = true;
  settingsSaveBtn.disabled = true;
  settingsSaveBtn.textContent = "Saving…";
  setSettingsError(null);
  setSettingsSuccess(false);
  hideSettingsClampNote();

  try {
    const effective = await invoke<Settings>("set_settings", { newSettings: payload });
    renderSettingsForm(effective);
    setSettingsSuccess(true);
    if (effective.poll_interval_secs !== payload.poll_interval_secs) {
      showSettingsClampNote(payload.poll_interval_secs, effective.poll_interval_secs);
    }
  } catch (err) {
    // Deliberately do not re-render the form here: the user's edits stay in
    // place so a failed save doesn't also cost them their input.
    setSettingsError(`Could not save settings: ${String(err)}`);
  } finally {
    settingsSaving = false;
    settingsSaveBtn.textContent = "Save";
    updateSettingsSaveButton();
  }
}

async function loadSettings(): Promise<void> {
  try {
    const settings = await invoke<Settings>("get_settings");
    setSettingsError(null);
    renderSettingsForm(settings);
  } catch (err) {
    setSettingsError(`Could not load settings: ${String(err)}`);
  }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

window.addEventListener("DOMContentLoaded", () => {
  bindDom();

  settingsForm.addEventListener("submit", (event) => void onSettingsFormSubmit(event));
  for (const input of [
    settingsPollIntervalInput,
    settingsNotificationsInput,
    settingsLaunchStartupInput,
    settingsStartMinimisedInput,
    settingsCountryCodeInput,
  ]) {
    input.addEventListener("input", onSettingsFieldChange);
    input.addEventListener("change", onSettingsFieldChange);
  }

  void loadSettings();
});
