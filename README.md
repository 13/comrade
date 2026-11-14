# comrade

A zero-config serial monitor for USB microcontrollers, in the terminal, for Linux.

Plug in an Arduino, ESP32, RP2040 or similar board and its output appears on screen. When you reset the board, replug it or flash it, comrade keeps running and picks the board up again. You never have to press a key to reconnect.

- Connects as soon as udev reports the device, so boot output is not lost.
- Survives unplug, replug, resets, USB re-enumeration and uploads.
- Steps aside on its own while `pio run -t upload`, esptool or avrdude uses the port, then comes back for the fresh boot log.
- Shows one tab per physical board, so you can watch several boards at once.
- Needs no config file, no wrapper command, no PlatformIO hook and no root.

Linux only. comrade needs udev, inotify and `/proc`.

## Install

```sh
cargo install --path .
```

The crate builds against libudev, so install the udev development headers if your distribution packages them separately (`libudev-dev` on Debian and Ubuntu, `systemd-devel` on Fedora; Arch ships them with `systemd-libs`).

## Usage

```sh
comrade
```

That is all you need. comrade lists the USB serial ports that are already present and watches for new ones. It opens each port that belongs to a known microcontroller vendor.

**Auto-connected vendors (by USB VID):** Espressif `303a`, Arduino `2341` and `2a03`, WCH/CH340 `1a86`, Silicon Labs CP210x `10c4`, FTDI `0403`, Raspberry Pi `2e8a`, Adafruit `239a`, SparkFun `1b4f`, STMicro `0483`, PJRC/Teensy `16c0` and Seeed `2886`.

comrade never opens any other USB serial device on its own. A phone that shows up as `/dev/ttyACM0` is one example. Such devices are listed under `c` (other ports), and you can connect to them from there. comrade ignores non-USB ttys (`ttyS*`).

### CLI overrides

You never need a flag. These two exist only to override the defaults.

| Flag | Effect |
|---|---|
| `--baud <rate>` | Sets the baud rate for boards that have no remembered rate. The default is 115200. |
| `--port <path>` | Opens this port at startup even if it is not a known board, for example `--port /dev/ttyS0` or a pty. You can repeat it. |

### Screen

```
 comrade  [1 ESP32-S3/C3 ●] [2 Uno ⏸] [3 CH340 ○]                    115200 8N1
──────────────────────────────────────────────────────────────────────────────
14:02:31.204 ESP-ROM:esp32s3-20210327
14:02:31.206 rst:0x1 (POWERON),boot:0x8 (SPI_FAST_FLASH_BOOT)
──── reset · 14:02:40 ────
14:02:40.118 ESP-ROM:esp32s3-20210327
──────────────────────────────────────────────────────────────────────────────
 >                                                           [Enter sends \n]
 ● /dev/ttyACM1  1.2 kB/s   ↑↓ scroll  End follow  / search  b baud  p pause  ? help
```

Each tab shows its board's state with one of these glyphs:

- `●` connected
- `⏸` paused, either for an upload or by hand with `p`
- `○` unplugged or waiting
- `!` error, for example no permission

When you unplug a board, its tab and scrollback stay. If you plug the same board back in, it reattaches to the same tab, even if the device path changed.

comrade adds separator lines to the scrollback:

- `──── reconnected · HH:MM:SS ────` when a port comes back.
- `──── reset · HH:MM:SS ────` when you press `r`, or when an ESP ROM banner line (`ESP-ROM:` or `rst:0x`) appears in the middle of a session.
- `──── upload · HH:MM:SS ────` when an uploader takes the port.

comrade never clears the scrollback on its own. It keeps the last 100 000 lines per tab in memory. Firmware ANSI colours are rendered. Each line has a timestamp, taken when its first byte arrived.

### Keys

**Log mode** (the default):

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

When you scroll up, the view freezes and shows `+N new`. Press `End` to follow again.

The saved log is plain text, with colour codes removed. Each line has an `HH:MM:SS.mmm` prefix while timestamps are on.

**Send box:**

- `Enter` sends the line plus the line ending.
- `Ctrl-E` cycles the line ending: `\n`, `\r\n` or none.
- `Up` and `Down` recall earlier lines from this session.
- `Esc` returns to log mode.

### What comrade remembers

comrade remembers the baud rate and line ending for each board, keyed by the board's USB identity. It saves them to `$XDG_STATE_HOME/comrade/boards.toml` (by default `~/.local/state/comrade/boards.toml`) whenever you change them.

If this file is missing or corrupt, comrade ignores it and writes a new one. A problem with the file never stops comrade from running.

## DTR/RTS per board family

When Linux opens a serial port, the kernel raises both DTR and RTS. Right after opening, comrade sets the lines to a final state that depends on the board family. It changes both lines in one ioctl, so they never pass through a state that resets an ESP32.

| Family | Examples (VID:PID) | DTR | RTS | Why |
|---|---|---|---|---|
| UART bridge | CP210x `10c4`, CH340 `1a86`, FTDI `0403`, Uno/Mega 16U2 `2341:0043/0001/0010/0042/0243` and the same PIDs under `2a03`, ST-Link `0483` | off | off | On ESP32 boards with the two-transistor auto-reset circuit, DTR on with RTS off pulls IO0 low. The reset button would then start download mode. |
| ESP USB-Serial-JTAG | `303a:1001` (built into ESP32-S3, C3 and C6) | off | off | RTS on holds the chip in reset. |
| Native USB CDC | Leonardo, Micro, Zero and other `2341`/`2a03` CDC PIDs (bit 15 set, e.g. `8036`, `804d`); SparkFun `1b4f`, Adafruit `239a` and Seeed `2886` PIDs with bit 15 set; RP2040 `2e8a`; Teensy `16c0`; other Espressif `303a` PIDs (TinyUSB) | on | off | These USB stacks treat DTR as "host is listening". Without it, `while (!Serial)` sketches and TinyUSB print nothing. |
| Unknown / manual (`--port`, `c`) | anything else | on | off | Same as the pyserial and miniterm default. |

**comrade resets ESP boards when they are plugged in, so the boot log is captured.** When an ESP USB-Serial-JTAG board, or a board behind a UART bridge other than Arduino's own (`2341`, `2a03`), is plugged in while comrade runs, comrade opens the port and then sends one reset pulse (see `r` below), with a `reset` separator. This also happens when such a board comes back after an unplug. It does not happen for boards that are already plugged in when comrade starts, when a board comes back after an upload (the uploader already reset it), or on a reconnect retry.

**An Uno (or a Nano with a DTR reset capacitor) resets once when comrade connects.** The DTR edge on open does this, just as in the Arduino IDE serial monitor. You get the boot output from the start.

comrade never auto-connects bootloader PIDs. For Arduino (`2341`, `2a03`), SparkFun (`1b4f`), Adafruit (`239a`) and Seeed (`2886`), every PID with bit 15 clear counts as a bootloader (for example Caterina `2341:0036`, Arduino SAMD `2341:004d`, Adafruit UF2 `239a:000b`, Seeed `2886:002f`), except the Arduino UART-bridge PIDs listed above. The SparkFun Caterina PIDs `1b4f:9203/9205/9207` are bootloaders too. Bootloader ports appear only under `c`.

What `r` does depends on the family:

- **UART bridge:** comrade runs three steps, each one ioctl: DTR off and RTS on for 100 ms, then both off, then both on for 50 ms, then back to both off. On an ESP32 with the two-transistor auto-reset circuit, the first step holds EN low, and the second releases it with IO0 high, so the chip boots normally. Both lines on is neutral on that circuit. On an Uno or a CH340 Nano, the DTR-on edge in the third step resets the AVR through the DTR capacitor.
- **ESP USB-Serial-JTAG:** comrade turns RTS on for 100 ms while DTR stays off.
- **Native USB CDC:** `r` does nothing and the tab shows `reset not supported on native USB boards`. Lines do not reset these boards.
- **Unknown ports (`--port`, `c`):** comrade drops DTR for 50 ms.

Residual risk: an ESP board wired with DTR and RTS straight to EN and IO0, without the two transistors, sees "DTR on" as IO0 low. With such a board, the third step of the UART-bridge pulse can land the chip in download mode instead of a normal boot. Press the board's reset button if that happens.

## Upload pause

comrade opens ports without an exclusive lock (no `TIOCEXCL`, no `flock`), so an uploader can always open the port.

While comrade has a port open, it watches the device node with inotify. When another process opens the node, comrade finds that process in `/proc/*/fd`. comrade then does the following:

1. It closes its own file descriptor straight away.
2. It adds a `──── upload · HH:MM:SS ────` separator to the scrollback.
3. It shows `⏸ upload in progress (esptool, pid 4121)` in the status line. If it cannot read the process name, for example because the process belongs to root or another user, it shows `opened by another program` instead.

The pause follows the uploading process, not the board. While any watched port has a foreign holder, comrade auto-connects to no port. This also covers boards that re-enumerate as a bootloader device during the upload (the 1200-baud touch). comrade gives no tab to bootloader devices that appear during the pause.

When the last foreign holder closes the port, what happens next depends on the board family:

- **UART bridge and ESP USB-Serial-JTAG** (ESP32 dev boards, Uno, ESP32-S3 through its built-in USB-JTAG): if the same device node is still there, comrade reopens it at once. It does not wait for a quiet period, so it catches the boot log that follows the upload. A UART bridge may still drop a few bytes. comrade does not send a reset pulse after the upload, because the board would boot twice. Press `r` if you want one.
- **Native USB CDC** (Leonardo, Pro Micro, RP2040, TinyUSB): comrade waits for a **500 ms quiet period**, then reconnects when the board's identity reappears through udev. Reopening at once would raise DTR and change the line coding, which could cancel the 1200-baud bootloader touch. Manual and unknown ports follow the same rule.
- **Failed upload, unplug, or board left in the bootloader:** the pause still ends when the uploader exits, and the tab goes to `○`.
- **Safety net:** if a foreign holder never closes, for example a second monitor left open, that tab stays `⏸` until the holder closes. After 60 s, comrade lifts the global auto-connect block so that other boards can connect again. Press `p` to resume by hand at any time. comrade then accepts the programs that hold the port at that moment: they no longer pause the tab or block auto-connect. Any other program that opens the port later, such as the next upload, pauses the tab as usual.

The separator marks the gap in the scrollback. Pressing `p` toggles the pause on the focused board, as a fallback if detection ever misses something.

## ModemManager and brltty

On some distributions, these two services can grab a new board before comrade does:

- **ModemManager** probes new `ttyACM` devices with AT commands. This can garble the boot output and delay the open.
- **brltty** claims CH340 devices on some distributions.

If one of them holds the port, comrade shows its name, or `opened by another program`, and waits for it to close the port.

To stop the interference, do one of the following:

- Disable the service: `sudo systemctl disable --now ModemManager`.
- Tell ModemManager to ignore your boards with a udev rule, for example:

  ```
  # /etc/udev/rules.d/99-mm-ignore-mcu.rules
  ATTRS{idVendor}=="303a", ENV{ID_MM_DEVICE_IGNORE}="1"
  ATTRS{idVendor}=="2341", ENV{ID_MM_DEVICE_IGNORE}="1"
  ```

- Remove brltty if you do not use a braille display.

## Permissions

Serial ports belong to a group, usually `uucp` on Arch and `dialout` on Debian and Ubuntu. If you are not in that group, the tab shows an error like this:

```
! no permission for /dev/ttyACM0 — add yourself to group 'uucp' and re-login
```

comrade reads the group name from the device node. To fix the error, add yourself to the group, then log out and back in:

```sh
sudo usermod -aG uucp "$USER"     # Arch
sudo usermod -aG dialout "$USER"  # Debian / Ubuntu
```

comrade never needs root.

## Robustness

- comrade does not exit on device events or I/O errors. A port that fails shows `!` or `○`, and comrade keeps running.
- If a connection drops while the board is still plugged in (an I/O error, or a crashed port worker), the tab shows `connection lost: <reason>`, goes to `○` and comrade retries. The first retry comes after 1 s, and the gap doubles up to 5 s. A connection that delivers data, or a replug, starts the backoff again. comrade does not retry while the port is held by another program, while you paused the tab with `p`, or while an upload pause blocks auto-connect.
- A missing node (`ENOENT`, `ENODEV`, `ENXIO`) is not an error: the tab shows `○`. A `--port` path that does not exist yet is retried with the same backoff, so comrade connects once it appears.
- A write that cannot go out within 2 s (for example, a full output buffer) is dropped. The tab shows `write timed out, N bytes dropped`, and the port stays open, so reading goes on.
- On `q`, `Ctrl-C`, SIGTERM, SIGHUP or a panic, comrade restores the terminal. It leaves the alternate screen, turns off raw mode and mouse capture, and shows the cursor.
- comrade renders only when something changed, and at most 60 times a second. During a flood of data it draws one frame for many batches. When nothing happens, it does not busy-wait.

## Hardware checklist

Run this checklist before you release a new version. Use `pio run -t upload` in a second terminal while comrade runs in the first.

Boards:

- [ ] ESP32 with a UART bridge (CP210x or CH340)
- [ ] ESP32-S3 on native USB (`303a:1001`)
- [ ] Arduino Uno
- [ ] Leonardo or Pro Micro
- [ ] RP2040 (Pico), including `pio run -t upload` through the 1200-baud touch and UF2 drive

For each board, check that the boot log is visible in each of these cases, with no keypress:

- [ ] **Plug in:** the boot log appears.
- [ ] **Reset button:** the boot log appears again. On an ESP32 you also get a `reset` separator. The board must not stay in download mode.
- [ ] **`pio run -t upload`:** the upload succeeds while comrade is running. The tab shows `⏸ upload in progress (…)`, then one `upload` separator, then the new firmware's boot log.
- [ ] **Unplug and replug:** the board comes back in the same tab with a `reconnected` separator and its boot log.

Also check:

- [ ] `r` resets the board and shows the boot log.
- [ ] `r` on an Uno (and on a CH340 Nano) resets the AVR and shows the boot output.
- [ ] `r` on a native USB board (Leonardo, RP2040) shows `reset not supported on native USB boards`.
- [ ] Plugging in an ESP32 while comrade runs shows a `reset` separator and the full boot log.
- [ ] Two boards at once, one uploading: the other tab keeps streaming, and the uploading one comes back after the upload.
- [ ] Permission error: with the port's group removed from your user (or `chmod o-rw` on the node), the tab shows `! no permission for … — add yourself to group '…' and re-login`.
- [ ] `--port /dev/ttyUSB0` (or a `c` port): unplug and replug the board; the same tab reconnects.
- [ ] A second monitor holding the port: the tab shows `⏸`; `p` resumes it, and a later upload still pauses it.
- [ ] A phone or other non-allowlisted device is never opened on its own, and it appears under `c`.
- [ ] Measure how many boot bytes a UART-bridge ESP32 loses after an upload.
