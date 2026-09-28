# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.17.13](https://github.com/alvin-7/mousehop/compare/v0.17.12...v0.17.13) - 2026-09-28

### Fixed

- Release Windows modifiers that were already held before crossing to a remote screen, preventing Ctrl from remaining active after return.
- Recover stalled reliable input on the existing Windows-to-macOS connection when the macOS desktop can provide a live display and cursor baseline. Recovery has a fixed two-second deadline.
- Stop macOS key repeat before recovery cleanup and report failed mouse-button or modifier injection instead of treating it as successful cleanup.

- Correct the cargo-bundle key for the macOS `LSUIElement` / TCC plist: it belongs under `[package.metadata.bundle.osx]` as `info_plist_exts`, so the bundled `.app` was previously built without it.

### Changed

- Fix the modifier release order on screen handover: reset the aggregate modifier mask before sending individual key-ups. macOS reads combined flags on each transition, so releasing a chord one key at a time could reclassify a just-released remote flag as locally held.

## [0.17.12] - 2026-09-23

Everything from 0.17.2 through 0.17.12 was developed on this fork without
per-release changelog entries. The list below covers that whole span; see the
`README.md` and `DOC.md` sections named after each version for the details.

### Added

- Experimental reliable input over DTLS: a KCP session carries keyboard and
  clipboard events while pointer motion stays on plain UDP, with per-client
  opt-in, connection recovery and separate connection-liveness tracking.
- Per-client scroll inertia and dual-axis scroll direction, configurable from
  the GTK preferences window and the config file.
- macOS keyboard injection via an IOKit `NXEventData` bridge, so system
  shortcuts and cross-platform modifier aliasing work on a macOS receiver.
- macOS trackpad pinch-to-zoom capture and steadier trackpad scrolling, using a
  vendored `core-graphics` that exposes the `CGEventTypeGesture` variant.
- A per-user Windows x64 installer (`scripts/package-windows.ps1` plus
  `packaging/windows.iss`) and an embedded application icon for `mousehop.exe`.
- Persistent panic and input-release logging for diagnosing crashes after the
  fact.

### Fixed

- Keep the Windows shared UDP listener receiving after `WSAECONNRESET` (10054).
  Windows surfaces a peer's ICMP Port Unreachable through `recv_from`, and
  tearing down the shared receive task left the port bound but every later DTLS
  handshake unanswered. Patched in the vendored `webrtc-util`.
- Bundle transitive dylib dependencies and prepare the icon correctly in the
  macOS packaging scripts.

## [0.11.3](https://github.com/jondkinney/mousehop/compare/v0.11.2...v0.11.3) - 2026-05-20

### Added

- *(desktop)* install desktop entry on first launch
- *(tray)* recolor tray glyph at runtime via currentColor

## [0.11.2] - 2026-05-19

### Fixed

- Flatpak icon validation: GdkPixbuf's SVG sniffer reads only the first
  ~256 bytes looking for the `<svg` tag, and the multi-line XML
  docstring above the root element pushed it past that window. Moved
  the docstring inside `<svg>` so Flatpak's icon validator accepts the
  app icon and the Flatpak bundle exports cleanly.

## [0.11.1] - 2026-05-19

### Fixed

- Typo "occured" → "occurred" in the `IpcError::Io` thiserror message
  (user-visible in logs and CLI output), a doc comment on
  `FrontendEvent::Error`, and the `capture_event_occured` local
  variable in `input-capture`'s libei capture loop.
