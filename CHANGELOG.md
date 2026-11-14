# Changelog

## Unreleased

- Raw mode (`I`): every key goes straight to the board; `Ctrl-]` leaves.
- `y` copies the bottom visible line with OSC 52. `m` turns mouse capture on or off.
- A note in the log when the output does not look like text (wrong baud rate).
- `--log-dir <dir>` appends every tab's lines to a file as they arrive.
- `f` filters the log to the lines that match the kept search.
- Data bits, parity and stop bits per board, set with `←` `→` in the baud picker.
- Prolific PL2303, Nordic, SEGGER and NXP ports are connected automatically.
- A note instead of a cramped layout when the terminal is smaller than 40x8.
- `--completions <shell>` and `--man` print a completion script and the man page.
- `rust-version` is 1.89.

## 0.1.0

First tagged version.
