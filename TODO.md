# TODO

## Hardware check

- [ ] Run comrade against the FT232R + Arduino Pro Mini on a real `/dev/ttyUSB0`. Not tested yet: the running kernel (7.2.11) had no `ftdi_sio` module after an upgrade to 7.3.4 without a reboot. Check: boot log on plug, `r` reset over DTR, baud picker, unplug/replug, `pio run -t upload` pause and resume.

## Ideas taken from mqtt-zapp

- [ ] Copy to clipboard via OSC 52 (for example the current line or a search hit), with a size cap.
- [ ] Type-to-search inside the help popup: highlight matches, dim the rest.
- [ ] Remappable keymap from a config file, plus a test that keeps the README key table in sync with the defaults.
- [ ] "Terminal too small" screen below a minimum size, instead of a cramped layout.
- [ ] Short-lived status notes with severity (info / error) instead of plain strings.
- [ ] VHS tape (`docs/demo/demo.tape`) that records a demo GIF for the README.
- [ ] Release workflow: static musl build, `.deb` (cargo-deb), Arch PKGBUILD, `SHA256SUMS`, build provenance, tag-vs-Cargo version check.

## Repo

- [ ] Choose a license, then set `license` in `Cargo.toml`, drop `publish = false` and the `[licenses] private` ignore in `deny.toml`.
