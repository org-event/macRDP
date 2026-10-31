# Changelog

All notable changes to this project are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Changed

- The binary, app and agent are now `openrdp`, `~/Applications/OpenRDP.app` and `com.openrdp.server`. `install` deletes a previous install (agent, app, log and support directory) instead of keeping it, and macOS asks for Screen Recording and Accessibility again.

## [1.0.0] - 2026-10-31

First release.

### Changed

- The project is now called OpenRDP (github.com/org-event/OpenRDP). The binary, app bundle and agent label keep the macrdp names for now.

### Added

- RDP server on IronRDP: ScreenCaptureKit capture, VideoToolbox H.264, EGFX AVC420, NLA.
- A virtual display sized to the client, kept for a reconnect.
- Keyboard, mouse, scrolling; the cursor sent as RDP pointer updates.
- Clipboard: text and images, both ways.
- Power assertions; a connecting client wakes a sleeping display.
- `macrdp install` / `uninstall`: a LaunchAgent with a locally signed `macRDP.app`, so updates keep the macOS permissions.
- `scripts/macrdp-doctor`: checks the session, agent, port, permissions, signing, power settings and network, and can wake the display, restart the agent, bring up WireGuard or send Wake-on-LAN.
- The server logs the state of both macOS permissions on every start.
- CI on Apple Silicon runners, release builds, Dependabot and cargo-deny.
- Licenses (MIT OR Apache-2.0), English README, contributing guide, security policy.

[Unreleased]: https://github.com/org-event/OpenRDP/compare/v1.0.0...HEAD
[1.0.0]: https://github.com/org-event/OpenRDP/releases/tag/v1.0.0
