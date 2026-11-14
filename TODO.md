# TODO

Items are grouped by priority. Each item has a check that tells you when it is done.

## P0: before anything is public

- [ ] **Hardware check.** Run comrade against the FT232R + Arduino Pro Mini on a real `/dev/ttyUSB0`. The `ftdi_sio` module for the running kernel (7.3.4) is now installed, so this is no longer blocked. It needs the board to be plugged in. `v0.1.0` is tagged without any real-hardware test.
  Check: boot log on plug, `r` reset over DTR, baud picker, unplug/replug, `pio run -t upload` pause and resume. Then go through the "Hardware checklist" in the README for the other boards you own.
- [ ] **Choose a license.** Set `license` in `Cargo.toml`, add the `LICENSE` file, drop `publish = false` and the `[licenses] private` ignore in `deny.toml`. The release workflow below depends on this.
  Check: `cargo deny check` passes with the private ignore removed.
- [ ] **Dependabot PR.** Merge or close the `actions/checkout` 5.1.0 to 7.0.1 bump (CI on it is green).
  Check: no open dependabot PR.
- [ ] **Set `rust-version`** in `Cargo.toml`. Edition 2024 needs a recent toolchain. Add a CI job that builds with exactly that version.
  Check: the MSRV job is green and fails if you lower the field.

## P1: features that matter most to users

- [x] **Raw pass-through mode.** Send every key straight to the port (arrows, Tab, Ctrl-C, history), with one escape key such as `Ctrl-]`. This is what you need for shells and REPLs on the board: the ESP-IDF console, MicroPython, Zephyr shell, bootloader prompts. The send box only sends whole lines today.
  Check: in a MicroPython REPL, Tab completion and Up-arrow history work.
- [x] **Text selection and copy.** comrade turns on mouse capture, which blocks the terminal's own selection (only Shift-drag works, and some terminals do not allow it). Add OSC 52 copy (already in the list below) and either a selection mode or a key that turns mouse capture off.
  Check: you can copy a line from the log into another program without leaving comrade.
- [x] **Wrong-baud hint.** When a connected port delivers mostly bytes that are not printable or not valid UTF-8, show a status note such as `looks like the wrong baud rate (try b)`. Zero-config tools should say so when the baud is wrong.
  Check: feed a pty with 9600-baud garbage while the tab is set to 115200; the note appears, and it goes away when good text arrives.
- [x] **Continuous logging to a file.** `s` saves a snapshot only. Add an optional `--log-dir <dir>` that appends every tab's lines to a file as they arrive, so a long run or a crash does not lose data.
  Check: kill comrade with SIGKILL; the log holds everything up to that moment.
- [x] **Filter view.** Show only the lines that match the search (like `grep`), with a key to turn it on and off. Search today only jumps between hits.
  Check: with the filter on, only matching lines show, and new matching lines keep arriving.
- [x] **Serial format.** The format is fixed at 8N1 (see `src/ui/mod.rs`). Add data bits, parity and stop bits to the baud picker and remember them per board.
  Check: unit-tested (`format_flags`). A pty forces 8N1, so the real effect still needs a hardware check: set 7E1 on a board that sends it and confirm the text is right.
- [x] **More vendors in the allowlist** (done: PL2303, Nordic, SEGGER, NXP; the dual-interface check is still open, see below). Add Nordic `1915`, SEGGER `1366`, NXP `1fc9` and similar, each with the DTR/RTS family that fits. Check that boards with two serial interfaces (FT2232, some probes) get one tab per interface.
  Open: plug in a dual-port device (FT2232, some probes) and confirm each interface gets its own tab and survives a replug. The code falls back to a path-based key when two ports share a serial, but nobody has run it on real hardware.

## P2: polish and distribution

- [x] Copy to clipboard via OSC 52 (for example the current line or a search hit), with a size cap.
- [ ] Type-to-search inside the help popup: highlight matches, dim the rest.
- [x] "Terminal too small" screen below a minimum size, instead of a cramped layout.
- [ ] Short-lived status notes with severity (info / error) instead of plain strings. The wrong-baud hint above needs this.
- [ ] Optional remappable keymap from a config file, plus a test that keeps the README key table in sync with the defaults. The config file must stay optional: the README promises "no config file needed".
- [ ] Optional config for extra allowlist VID:PID pairs, with the same rule.
- [ ] Per-tab rename that is remembered with the board.
- [ ] Hex view for binary data, and send as hex.
- [ ] Relative timestamps (time since the previous line) as an alternative to the clock time.
- [ ] ESP-IDF backtrace decoding with `addr2line`, given an ELF path. Only if there is demand; it is a lot of code for a niche.
- [ ] VHS tape (`docs/demo/demo.tape`) that records a demo GIF for the README.
- [ ] Release workflow: static musl build, `.deb` (cargo-deb), Arch PKGBUILD, `SHA256SUMS`, build provenance, tag-vs-Cargo version check. Needs the license first.
- [ ] Shell completions and a man page, generated by `clap_complete` and `clap_mangen`.
- [ ] A `CHANGELOG.md`.

## P3: internal quality

- [ ] Measure throughput and CPU at 2 000 000 baud with a pty flood. Check that the UI stays responsive and memory stays bounded with 100 000 lines in several tabs.
- [ ] Fuzz or property-test the line assembler in `src/lines.rs`: ANSI escapes and multi-byte UTF-8 split across reads, lone `\r`, long lines without a newline.
- [ ] Split `src/core/mod.rs` (about 2 500 lines, most of them tests) and `src/ui/mod.rs` (about 1 400 lines) into smaller modules.
- [ ] Review the non-test `unwrap()` and `expect()` calls (about 90 in `src`, many in tests) and make sure none can fire on device input.
- [ ] The ignored test `enumerates_real_system` in `src/io/udev_watch.rs` needs real hardware. Run it by hand before each release and list it in the checklist.
