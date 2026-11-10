# watchtuier

A system-tray watcher for **USB serial devices** and **IP neighbours**.

By default watchtuier lives in the tray: click the icon and get a live, compact
menu of every `/dev/ttyUSB*` / `/dev/ttyACM*` device and every IPv4 neighbour
grouped by subnet — click any device or neighbour row to copy the devnode or
IP address to the clipboard. Freshly plugged (or unplugged) serial adapters
raise a desktop notification. A legacy full-screen terminal UI (`--tui`) shows
the same data in two tables.

## Features

- **Tray app (default, Linux)** — StatusNotifierItem icon with a live menu that
  is rebuilt from live state every time you open it:
  - USB section: one line per serial device (`/dev/ttyUSB0 — FT232`), with an
    accurate short chip name derived from the USB VID/PID.
  - One section per subnet that currently has neighbours:
    `192.168.228.8 — vrx-ec1d925f.local · fa:84:66:74:a2:6a · STALE`.
  - Clicking a device or neighbour copies the bare devnode / IP to the
    clipboard (best-effort; never breaks the tray).
  - Desktop notifications for USB connect/disconnect. New-neighbour
    notifications are opt-in with `--notify-net` (DHCP and container churn
    make them noisy), announced at most once per IP per session.
- **TUI (`--tui`)** — two stacked ratatui panels (USB serial devices, network
  neighbours for the selected subnet) plus a status line.
- **Accurate chip identification** — OS-reported USB strings are often lies
  (CH340s claim "USB2.0-Serial", STM32 CDC gadgets claim "Linux Foundation"),
  so a VID/PID lookup table wins whenever the pair is recognised: FTDI,
  WCH CH340/341/9102, CP210x, PL2303, STM32 CDC/DFU, ESP32 USB-JTAG, J-Link,
  EDBG, Arduino, Adafruit, Raspberry Pi Pico, MCP2200. Unknown devices fall
  back to the OS descriptor strings, then to a vendor-name table.
- **Liveness-probed neighbours** — the kernel keeps `STALE` ARP entries long
  after a device is gone, so every ~10 s each STALE IPv4 neighbour gets one
  ICMP echo nudged at it. That makes the *kernel* re-verify the entry: present
  devices come back `REACHABLE`, unplugged ones drop out of the table.
- **Two-stage hostname resolution** — unicast reverse DNS first, then an mDNS
  (Avahi) fallback for zeroconf-only devices that no unicast resolver knows.

## Usage

```
watchtuier [OPTIONS]

OPTIONS:
    --notify-net   Also notify when a new network neighbour appears
                   (USB serial connect/disconnect always notifies)
    --tui          Run the legacy terminal UI instead of the tray app
    -h, --help     Print this help
```

`--notify-net` only applies to tray mode; combining it with `--tui` is a usage
error (exit code 2). Tray mode is Linux-only — on other platforms run `--tui`.

### TUI keys

| Key          | Action                          |
| ------------ | ------------------------------- |
| `q` / `Esc`  | quit                            |
| `Tab`        | switch focus between panels     |
| `j`/`k`, `↑`/`↓` | scroll the focused list    |
| `h`/`l`, `←`/`→` | cycle the selected subnet |

The latest-plugged device and the most recently appeared neighbour are
highlighted in bold yellow.

## Platform support

| Platform | Tray            | TUI                          |
| -------- | --------------- | ---------------------------- |
| Linux    | ✅ (default) | ✅ (full)                        |
| macOS    | ❌ (`bail`)  | ⚠️ (`arp -a`, no liveness probe; no macOS serial-port mapping yet, so the USB panel stays empty) |
| Windows  | ❌ (`bail`)  | ✅ (`arp -a`, no liveness probe, `COMx` ports) |

The tray needs a D-Bus session with a StatusNotifierItem host (KDE, or GNOME
with the AppIndicator extension) and a freedesktop notification daemon. The
tray icon uses the themed `drive-removable-media-usb` icon name.

## How it works

Both front-ends run the same two background watcher threads, feeding the same
`AppEvent` stream into the same `App` reducer — only one front-end runs at a
time, and the tray menu and the TUI can never disagree because they are
built on identical watchers and state:

- **USB watcher** — enumerates serial ports via the `serialport` crate every
  second (portable; there is no unified device-change API across platforms)
  and diffs snapshots into added/removed events. `/dev/ttyS*` and other
  non-USB-backed ports are filtered out.
- **Network watcher** — local IPv4 subnets come from `if-addrs` (link-local
  `169.254/16` is skipped); the neighbour table comes from `ip -j neigh` on
  Linux or parsed `arp -a` elsewhere, polled every 2 seconds. Reverse-DNS
  lookups run in detached threads and land in the next poll cycle.

## Build & run

Requires Rust ≥ 1.80 (edition 2021).

```sh
cargo build --release
cargo run                # tray app (Linux)
cargo run -- --tui       # terminal UI
```

## Tests

```sh
cargo test               # unit tests + TestBackend UI integration tests
cargo test -- --ignored  # opt-in tests that hit the live network / clipboard
```

Most logic is deliberately pure functions over injected collaborators (menu
rows, notification payloads, event-pump suppression rules, hostname resolution
strategy, probe cadence), so it is unit-testable without a D-Bus session,
a terminal, or a real network. See [AGENTS.md](AGENTS.md) for the architecture
and working conventions.
