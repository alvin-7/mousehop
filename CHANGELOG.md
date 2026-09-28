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
