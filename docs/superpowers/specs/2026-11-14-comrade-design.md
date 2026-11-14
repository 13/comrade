# comrade — design

Date: 2026-11-14
Status: draft, builder-checked, awaiting approval

## 1. Intent

`comrade` is a zero-config Rust TUI serial monitor for Linux. Plug in a microcontroller (Arduino, ESP32, RP2040, …) and its output streams to the screen. Reset, replug, or flash it and the monitor keeps working without any keypress.

### Why

The user currently uses `pio device monitor` and has three concrete pains:

1. It breaks when the board resets.
2. It does not show data immediately on connect (boot output is lost).
3. After every reset or replug, they must reconnect manually.

comrade exists to remove all three. Success: plug in → boot log visible; press reset → boot log visible again; unplug/replug → logging resumes in the same tab; run `pio run -t upload` in another terminal → comrade steps aside and comes back with the fresh boot log. All without touching the keyboard.

## 2. Goals, non-goals, anti-goals

### Goals

- Zero config: run `comrade`, no arguments, no config file required.
- Auto-detect USB microcontroller serial ports and connect within milliseconds of the device node appearing.
- Survive unplug, replug, board reset, USB re-enumeration, and uploads without exiting or needing user action.
- Pause automatically while another program (esptool, avrdude, pio, …) uses the port, then reattach.
- Multiple boards at once, one tab per physical board.
- Fast: handle sustained high baud rates (≥ 2 Mbaud) with no dropped bytes and a responsive UI.

### Non-goals (v1)

- Windows / macOS support. Linux only.
- Plotting, hex view, protocol decoders, macros.
- Automatic file logging (explicit save only).
- Baud-rate auto-detection (impossible to do reliably; default + remembered per board instead).
- Network / RFC2217 / Bluetooth serial.

### Anti-goals (failure even if it "works")

- comrade crashes, exits, or leaves the terminal broken on any device event or I/O error.
- comrade corrupts or blocks an upload (holding an exclusive lock, or reading bytes while esptool/avrdude is talking to the bootloader).
- comrade auto-opens a non-microcontroller port (e.g. the user's Samsung phone currently exposed as `/dev/ttyACM0`, VID `04e8`).
- comrade requires a config file, a wrapper command, a pio `extra_script`, or root.

## 3. Constraints and environment facts

- Target: Arch Linux, kernel 7.x, Rust 1.99 (edition 2024). Must work on any systemd/udev Linux.
- User is in `uucp`; ports are readable without sudo. Other users may not be — see §6.8.
- `/dev/serial/by-id/` currently contains a Samsung Android phone (`04e8:6860`) as `ttyACM0`. Detection must filter by USB vendor.
- Verified: inotify delivers `IN_OPEN` / `IN_CLOSE_*` events for device nodes in `/dev` without root. This is the basis of upload detection.
- ModemManager and brltty are inactive on this machine (both are known to grab fresh `ttyACM`/CH340 ports on other distros; see §6.5).
- Not a git repo yet; the project directory `/home/ben/repo/comrade` is empty apart from this doc.

## 4. Approaches considered

| | Approach | Trade-off | Verdict |
|---|---|---|---|
| A | **Threads + channels**: udev monitor thread, inotify thread, one blocking reader thread per open port, UI thread with `ratatui` + `crossterm`, all events through one channel | Simple, no runtime, errors surface where they happen, small binary. Slightly more manual plumbing. | **Chosen** |
| B | Async: `tokio` + `tokio-serial` + `tokio-udev` | Same capability; wraps the same blocking termios calls; thinner, less-maintained crates; cancellation on unplug harder to reason about. Only pays off at dozens of ports. | Rejected |
| C | Polling `/dev` + `/proc` every ~250 ms | Simplest, but loses up to 250 ms of boot output on replug and wakes constantly. Defeats pain #2. | Rejected |

Also rejected: an upload wrapper (`comrade upload -- pio run -t upload`) or pio `extra_script` hook. Deterministic, but violates zero-config. The manual pause key (§6.4) is the fallback instead.

The user said "do best practice"; the approach choice is Claude's call, stated to the user without objection.

## 5. Architecture

```
 ┌──────────────┐  DeviceAdded/Removed   ┌────────────────────────────┐
 │ udev monitor │ ─────────────────────▶ │                            │
 │  (thread)    │                        │      App / UI thread       │
 └──────────────┘                        │                            │
 ┌──────────────┐  ForeignOpen/Close     │  state machine per board   │
 │ inotify +    │ ─────────────────────▶ │  tabs, scrollback, input   │
 │ /proc scan   │                        │  ratatui render @ ≤60 fps  │
 └──────────────┘                        │                            │
 ┌──────────────┐  Data(bytes)/Error     │                            │
 │ reader per   │ ─────────────────────▶ │                            │
 │ open port    │ ◀──── write / close ── │                            │
 └──────────────┘                        └────────────────────────────┘
 ┌──────────────┐  Key/Resize/Paste            ▲
 │ terminal in  │ ─────────────────────────────┘
 └──────────────┘
```

- One `crossbeam-channel` (or `std::sync::mpsc`) carries an `Event` enum to the app thread. The app thread is the only owner of state; worker threads hold no shared mutable state.
- Each reader thread owns its port's read half. Writes (send box, reset pulse) go through a per-port command channel or a cloned fd owned by the app — builder's call.
- Core logic (board identity, session state machine, line assembly, upload-pause logic) lives in pure modules with no I/O, so it is unit-testable by feeding `Event`s.

### Suggested crates (builder picks current versions)

- `ratatui`, `crossterm` — TUI.
- `udev` — libudev bindings for enumeration + monitor. Use libudev (not raw kernel netlink) because udev emits events **after** its rules run, so permissions/group are set and `open()` will not race into `EACCES`.
- `inotify` — foreign-open detection.
- `serialport` (with `default-features = false` if it lets us skip its own libudev enumeration) or direct `nix`/`rustix` termios. Builder's call; requirements in §6.3.
- `ansi-to-tui` (or equivalent) — render firmware ANSI colors.
- `serde` + `toml`, `directories` — small state file.
- `anyhow`/`thiserror` — errors.

## 6. Behavior

### 6.1 Detection

- On start: enumerate `tty` subsystem devices via libudev whose parent is USB. Then subscribe to the udev monitor for `tty` add/remove.
- **Auto-connect allowlist by USB VID** (builder finalizes list): Espressif `303a`, Arduino `2341` / `2a03`, WCH `1a86`, Silicon Labs `10c4`, FTDI `0403`, Raspberry Pi `2e8a`, Adafruit `239a`, SparkFun `1b4f`, STMicro `0483`, PJRC/Teensy `16c0`, Seeed `2886`.
- Ports outside the allowlist (e.g. Samsung `04e8`) are never auto-opened. They appear in a "other ports" list (`c` key) from which the user can connect manually; a manually connected port then behaves like any board.
- Non-USB ttys (`ttyS*`) are ignored entirely.

### 6.2 Board identity and tabs

- A **board** is a physical device, keyed by: `ID_SERIAL_SHORT` (USB serial number) + VID:PID when a serial exists; otherwise (CH340 and some clones have no serial) VID:PID + `ID_PATH` (physical USB port path).
- One tab per board. Tab label: friendly name from VID:PID/product string (`ESP32-S3`, `Uno`, `CH340`…), with a number for `1`–`9` switching.
- Tab status glyphs: `●` connected, `⏸` paused (upload/manual), `○` unplugged/waiting, `!` error (e.g. permission).
- Unplugged boards keep their tab and scrollback. Replug of the same identity reattaches to the same tab (device path may differ, e.g. `ttyACM0` → `ttyACM1`).
- A board with a new identity creates a new tab. Focus switches to it **only if** the currently focused tab is not connected; otherwise the new tab is highlighted.
- Closing a tab: `x` on an unplugged tab removes it (board forgotten for this session). On a connected tab, `x` disconnects and hides it until next plug.

### 6.3 Opening and reading a port

- Open **non-blocking-create, no exclusive lock**: never set `TIOCEXCL`, never `flock`. An exclusive lock would make uploaders fail.
- Configure raw mode, 8N1, no flow control, baud from per-board memory or default **115200**.
- **DTR/RTS line policy (per board family, Claude's call):** the Linux kernel raises DTR and RTS on open. Immediately after open comrade sets the final state from a family table keyed by VID:PID:
  - **UART-bridge boards** (CP210x `10c4`, CH340 `1a86`, FTDI `0403`, Uno/Mega `2341`/`2a03` with 16U2): **DTR and RTS both deasserted.** Required on ESP32 two-transistor auto-reset boards: DTR asserted + RTS deasserted pulls IO0 low, so pressing the board's reset button would drop it into download mode. Uno/Nano reset once on open (capacitor on DTR edge), matching Arduino IDE.
  - **Espressif USB-Serial-JTAG** (`303a:1001`, ESP32-S3/C3/C6 built-in): **both deasserted.** RTS asserted holds the chip in reset.
  - **Native USB CDC firmware** (Leonardo/Micro/Pro Micro `2341`/`1b4f` CDC PIDs, RP2040 `2e8a`, Adafruit `239a`, Seeed `2886`, Teensy `16c0`, Espressif TinyUSB CDC PIDs other than `303a:1001`): **DTR asserted, RTS deasserted.** These stacks treat DTR as "host listening"; sketches with `while(!Serial)` or TinyUSB would otherwise print nothing.
  - Unknown/manual ports: DTR asserted, RTS deasserted (pyserial/miniterm default behavior).
  - Builder finalizes the PID table; the policy is stored per identity in `boards.toml` so a future override is possible, but no UI for it in v1.
- **Reset hotkey `r`:** pulse to reset the board into normal run mode (ESP32: RTS-reset with IO0 high; AVR: DTR pulse). Lets the user grab a fresh boot log on demand.
- Reader thread: blocking `read` into a reusable buffer (≥ 4 KiB), forwards `Data(Vec<u8>)` batches. `EIO`, `ENXIO`, `ENODEV`, or a 0-length read on a vanished device → `PortGone`; reader exits cleanly. All other errors → reported to the tab, port closed, retry via the normal reconnect path.
- Connect latency target: open within **≤ 20 ms** of the udev add event.
- Data that arrives before the port is opened cannot be recovered (UART bridges drop it; native USB CDC buffers a little). This is why open latency matters.

### 6.4 Upload pause ("step aside")

1. While a board is connected, comrade holds an inotify watch on its device node for `IN_OPEN | IN_CLOSE_WRITE | IN_CLOSE_NOWRITE`.
2. comrade's own open/close generate events too. On any `IN_OPEN`, scan `/proc/*/fd` for links to that device node owned by a PID other than comrade's.
3. If a foreign holder is found: close comrade's fd immediately, set tab to `⏸ upload in progress (esptool, pid 4121)` using `/proc/<pid>/comm`. If the opener is unreadable (different user / root, e.g. ModemManager), still pause and label it `opened by another program`.
4. While **any** board is in upload-pause, comrade auto-connects to **no** port. This covers Leonardo / Pro Micro / RP2040 / ESP32-S3 flows where the board re-enumerates as a different bootloader device mid-upload (1200-baud touch). Bootloader devices appearing during the pause are not given new tabs.
5. **Pause is tied to the uploading process, not to the board.** The pause ends when no foreign process holds any allowlisted port. While paused, comrade watches (inotify) every allowlisted node that exists or appears, so bootloader nodes are covered.
   - **Same device node still present** when the last foreign holder closes (UART-bridge ESP32/Uno: esptool/avrdude reset the chip into the app just before closing): reopen **immediately** on `IN_CLOSE_*`, target ≤ 10 ms, so the post-upload boot log is caught. A few bytes may still be lost on UART bridges; builder measures on hardware. No automatic reset pulse (would boot the firmware twice); `r` remains available.
   - **Node vanished / board re-enumerating** (Leonardo, RP2040, ESP32-S3 native USB): resume auto-connect after a **500 ms** quiet period (builder may tune) and connect when the board's identity reappears via udev.
   - **Upload failed / board unplugged / left in bootloader:** pause still ends when the uploader exits. The board's tab goes to `○ waiting`. Global auto-connect resumes. No timeout needed because the trigger is the process, not the board.
   - **Safety net:** if a foreign holder never closes (e.g. a second monitor left open), the tab stays `⏸ opened by <comm>` indefinitely, but the **global** auto-connect block lifts after 60 s so other boards are not starved; `p` resumes manually.
   - **Known bootloader PIDs** (e.g. Caterina `2341:0036`, `1b4f:9205`; Espressif ROM download mode is the same `303a:1001` node and needs no entry) are never auto-connected, during or after a pause; they appear only in the `c` list. RP2040 UF2 mode is mass storage, not a tty, so it never appears.
6. A separator line is appended: `──── upload · 14:02:31 ────`. Scrollback is not cleared.
7. Uploaders retry their sync handshake, so the few ms where both comrade and the uploader have the port open are tolerated. Builder should verify with real esptool + avrdude if hardware is available, and with a fake opener in tests.
8. Watching foreign opens while paused: inotify watches stay on the board's last known node; also watch `/dev` for `IN_CREATE` is **not** required since udev already reports new nodes.
9. **Manual pause `p`:** toggles pause on the focused board (closes/reopens the port). Fallback when auto-detect misses something.

### 6.5 Known interferers

- ModemManager probes new `ttyACM` devices with AT commands; brltty grabs CH340 on some distros. Both are inactive here. If comrade detects a foreign opener at the moment it tries to connect, it shows the opener name. Help text mentions disabling ModemManager for these VIDs. No automatic workaround in v1.

### 6.6 Reset / boot markers

- Disconnect→reconnect (native USB resets re-enumerate) inserts `──── reconnected · HH:MM:SS ────`.
- UART-bridge boards don't re-enumerate on reset, so the port never closes. comrade inserts `──── reset · HH:MM:SS ────` when it sees an ESP ROM banner line (`ESP-ROM:` or `rst:0x`) or after an `r` reset pulse. No other heuristic in v1.

### 6.7 Display

```
 comrade  [1 ESP32-S3 ●] [2 Uno ⏸ upload] [3 Nano ○ unplugged]         115200 8N1
──────────────────────────────────────────────────────────────────────────────
 14:02:31.204  ESP-ROM:esp32s3-20210327
 14:02:31.206  rst:0x1 (POWERON),boot:0x8 (SPI_FAST_FLASH_BOOT)
 14:02:31.512  I (312) wifi: connected, ip=192.168.1.40
 ──── reset · 14:02:40 ────
 14:02:40.118  ESP-ROM:esp32s3-20210327
──────────────────────────────────────────────────────────────────────────────
 > _                                                              [Enter sends \n]
 ● /dev/ttyACM1  1.2 kB/s   ↑↓ scroll  End follow  / search  b baud  p pause  ? help
```

- **Line assembly:** split on `\n`; treat `\r\n` as one break; a bare `\r` is treated as a line break (builder may refine for progress bars). A partial line (no newline yet) is displayed immediately and updated in place — prints without newline must not wait.
- Invalid UTF-8 → lossy decode with replacement char. Control chars other than ANSI SGR are shown escaped (e.g. `␀`).
- ANSI SGR colors are rendered; other escape sequences are stripped.
- Timestamps per line (time of the line's first byte, ms precision), toggle `t`, on by default.
- **Follow mode** by default. Scrolling up (↑/↓/PgUp/PgDn/mouse wheel) freezes the view and shows `+N new`. `End` resumes follow.
- Scrollback: ring buffer, **100 000 lines per tab**, in memory. Oldest dropped.
- **Search `/`:** incremental, highlights matches in the focused tab, `n`/`N` next/prev.
- Line wrapping on by default; `w` toggles.

### 6.8 Input / keys

Two focus modes: **log** (default) and **send box**.

| Key (log mode) | Action |
|---|---|
| `1`–`9`, `Tab`/`Shift-Tab` | switch tab |
| `↑↓ PgUp PgDn Home End`, wheel | scroll / follow |
| `/` `n` `N` | search |
| `i` or `Enter` | focus send box |
| `b` | baud picker (common rates 300…2 000 000 + custom) |
| `p` | manual pause/resume |
| `r` | reset board |
| `t` | toggle timestamps |
| `w` | toggle wrap |
| `c` | list other (non-allowlisted) ports, connect manually |
| `s` | save tab scrollback to `./comrade-<board>-<YYYYMMDD-HHMMSS>.log` |
| `C` (shift) | clear tab scrollback |
| `x` | close tab |
| `?` | help overlay |
| `q` / `Ctrl-C` | quit |

Send box: `Enter` sends the line + `\n` (line ending selectable in the send box: `\n`, `\r\n`, none — key `Ctrl-E` cycles). `Esc` returns to log mode. Up/Down recall send history (session-only).

### 6.9 Persistence (zero-config, but remembers)

- `$XDG_STATE_HOME/comrade/boards.toml` (default `~/.local/state/comrade/boards.toml`): per board identity → baud, line ending, friendly-name override if ever added. Written on change. Missing/corrupt file → ignored and rewritten; never fatal.
- No config file in v1. CLI flags are allowed for overrides only (e.g. `--baud`, `--port`), never required. Builder decides the minimal set.

### 6.10 Errors and robustness

- **Permission denied:** tab shows `! no permission for /dev/ttyACM0 — add yourself to group 'uucp' (Arch) / 'dialout' (Debian) and re-login`. Group name read from the device node's gid.
- **Panic safety:** install a panic hook that restores the terminal (leave alternate screen, disable raw mode, show cursor) before printing the panic. Worker thread panics must not take down the UI; the app treats a dead worker like `PortGone` and reconnects.
- No `unwrap` on I/O paths.
- `SIGTERM`/`SIGHUP` → clean exit with terminal restored.
- Terminal resize handled at any time.

### 6.11 Performance

- Render only on dirty state, capped at 60 fps; coalesce multiple `Data` events between frames.
- Render only the visible window of the scrollback; never re-layout the full buffer per frame. Wrapping computed for visible lines only.
- Target: sustained 2 Mbaud (~200 kB/s) on one tab with < 5 % CPU on a modern laptop and no byte loss (reader thread never blocks on the UI).
- Idle: zero busy-waiting; all threads block on fds/channels.

## 7. Testing strategy (builder fills details)

- Pure state-machine tests: feed `Event` sequences (add, data, foreign open, remove, re-add with new path, bootloader device during pause, etc.) and assert tab states and separators.
- Line-assembly tests: `\r\n`, bare `\r`, partial lines, invalid UTF-8, split ANSI sequences across reads.
- Integration: pseudo-terminals (`openpty`) as fake serial ports for reader + foreign-open detection (a second process opening the pty slave triggers pause).
- Manual hardware checklist in README: ESP32 (UART bridge), ESP32-S3 native USB, Uno, Leonardo/Pro Micro, with `pio run -t upload` while comrade runs.

## 8. Decided by the user

- Linux only, Rust, TUI.
- Zero config; auto-detect, auto-connect, auto-reconnect.
- comrade runs during uploads and must pause automatically; "do best practice" for how.
- Multiple boards supported via **tabs**.
- Accepted the proposed display and behavior in §6.2, §6.7, §6.8 (replied "go").

## 9. Claude's calls (correctable)

- Approach A (threads + channels), libudev for hotplug, inotify + `/proc` for upload detection.
- VID allowlist for auto-connect; others manual only.
- Default 115200; per-family DTR/RTS policy (§6.3); `r` reset key.
- Pause tied to the uploading process; instant reopen on same node, 500 ms quiet period on re-enumeration, 60 s global safety net; bootloader PIDs never auto-connected (§6.4).
- No automatic reset after upload; UART-bridge boards may lose a few boot bytes (measured on hardware).
- Reset separator heuristic limited to ESP ROM banner + reconnect + `r`.
- 100k-line in-memory scrollback; save only on `s`.
- Board state file in `$XDG_STATE_HOME`.

## 10. Left to the builder

- Exact crate versions and whether to use `serialport` vs raw termios.
- Final VID allowlist and friendly-name table.
- Module layout, channel type, thread naming.
- Exact quiet-period tuning and DTR/RTS sequencing, verified on hardware where possible.
- CLI flag set (override-only).
