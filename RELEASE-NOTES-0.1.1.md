# ipwatch 0.1.1 — Usability fixes

This release fixes three bugs in 0.1.0 that affect day-to-day use on Windows.

## Single instance

If you clicked the Start Menu shortcut while ipwatch was already running, a second copy would start. This meant:
- Two tray icons instead of one
- The app wasting API requests (and hitting rate limits sooner)
- Duplicate entries in the history table

0.1.1 prevents the second launch. Clicking the shortcut now shows the existing window instead.

## Silent start

With 0.1.0, enabling *Start ipwatch at login* in Settings also meant a window popping up every time you log in. 0.1.1 adds a *Start minimised* option: you can now have ipwatch run at login and stay quietly in the tray.

## First-run setup

0.1.0 shipped with no setup flow. If you wanted to enable *Start at login*, you had to dig into Settings yourself. 0.1.1 shows a simple card on first run offering both options:
- Start ipwatch when Windows starts
- Start minimised to the tray

(If you're upgrading from 0.1.0, you'll see this card once. You were never asked, so it's correct to ask now — dismiss it if you'd rather keep the current setup.)

---

## Installation note

ipwatch installers are unsigned, so Windows SmartScreen will warn on first run. This is expected and does not mean the software is malicious. Click "Run anyway" to proceed. Signing requires a paid certificate; this may change in a future release.

Similarly, if you're on macOS, Gatekeeper will ask for permission the first time you run it.
