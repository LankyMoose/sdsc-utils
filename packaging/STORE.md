# Partner Center checklist (out-of-band)

These steps cannot be done from the repo. Do them in your browser / Partner Center.

## Account

1. Open [storedeveloper.microsoft.com](https://storedeveloper.microsoft.com) and create a **free Individual** developer account (government ID + selfie).
2. Reserve the app name **SDSC Utils** (exact Store title).

## Listing copy (v1.4.0)

Paste into Partner Center. Do **not** expand SDSC as “Sony DualSense Controller Utilities” anywhere in the listing.

### Title

SDSC Utils

### Short description

System tray DualSense battery, lightbar, toasts, and start-screen game launcher.

### Description (full)

SDSC Utils is a Windows system-tray utility for DualSense battery status, lightbar colors, Identify flash, overlay toasts, an optional DualSense start-screen game launcher, and optional local remaining-time estimates.

What you get
• Tray icon and popup for connected controllers, with optional nicknames and Remember for disconnected pads
• Start screen quick-launch when the first pad connects: Steam games and manual shortcuts, DualSense navigation, Games and Controllers tabs
• Overlay toasts for connect, disconnect, low battery, and charge complete
• Battery-driven lightbar spectrum (customizable; can be turned off) and Identify to find the right pad
• Soft power-off over Bluetooth from the popup or start screen
• Optional launch at Windows sign-in
• Opt-in battery analytics stored only on your PC (off by default)

Compatible hardware
• DualSense wireless controller
• DualSense Edge wireless controller

Works over USB and Bluetooth.

Unofficial third-party utility — not affiliated with, endorsed by, or sponsored by Sony Interactive Entertainment. DualSense, DualSense Edge, PlayStation, and related marks are trademarks of Sony Interactive Entertainment Inc.

### Other listing fields

- **Compatibility (body):** Works with DualSense wireless controllers and DualSense Edge wireless controllers.
- **Disclaimer:** Unofficial. DualSense, DualSense Edge, PlayStation, and related marks are trademarks of Sony Interactive Entertainment Inc. This app is not affiliated with, endorsed by, or sponsored by Sony.
- **Privacy policy URL:** the Privacy policy section of the GitHub README (`https://github.com/LankyMoose/sdsc-utils#privacy-policy`).
- **Screenshots:** use `assets/screenshots/` (controller popup, start screen Games / edit / Controllers, Settings lightbar / notifications / toasts / analytics).
- **Age rating:** complete the questionnaire (expect Everyone / suitable for all ages for this utility).

## Capabilities justification

When asked about `runFullTrust`: tray icon + raw DualSense HID (`hidapi`) for battery, lightbar, identify, and start-screen input.

## First upload / update

1. Download `sdsc-utils.msix` from the [v1.4.0 GitHub Release](https://github.com/LankyMoose/sdsc-utils/releases/tag/v1.4.0) (or pack locally — see [README.md](README.md)).
2. Confirm the package identity matches Partner Center (`LankyMoose.SDSCUtils` / publisher CN in [`AppxManifest.xml`](AppxManifest.xml)). Identity Version is stamped from `Cargo.toml` (`1.4.0` → `1.4.0.0`).
3. Submit for certification.
