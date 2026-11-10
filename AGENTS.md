# AGENTS.md

Working notes for AI coding agents (and humans) in this repository.

## What this is

`watchtuier` — a Rust binary that watches USB serial devices
(`/dev/ttyUSB*`, `/dev/ttyACM*`, Windows `COMx`) and IP neighbours, exposed
through two front-ends built on the *same* watchers and the *same* `App`
reducer (only one front-end runs at a time): a Linux system-tray app
(default) and a ratatui TUI (`--tui`).

## Commands

```sh
cargo build --release          # release build (lto = "thin")
cargo test                     # unit + integration tests; must stay green
cargo test <name>              # single test
cargo test --test ui_render    # UI integration tests only
cargo test -- --ignored        # opt-in tests that touch the live network / clipboard — see caveat below
cargo clippy                   # keep clean before pushing
cargo fmt                      # run before committing
cargo check --target x86_64-pc-windows-msvc   # compile-check the Windows cfg arms
cargo check --target aarch64-apple-darwin     # …and the macOS ones
```

There is no CI. `cargo test`, `cargo clippy`, and `cargo fmt --check` are the
gate. Rust 1.80 minimum, edition 2021.

**`--ignored` caveat:** those three tests fail on any machine that isn't the
author's dev box — `live_avahi_only_device_gets_hostname` asserts a hardcoded
zeroconf peer `192.168.228.8 → vrx-ec1d925f.local` on the LAN, and the two
live-clipboard tests (Linux-only, they live in the tray module) need a display
session; one of them additionally requires `WATCHTUIER_EXPECT_CLIP`. Treat
their failure as environmental, not as a regression. The non-Linux `cfg` arms
(COM classification, the `arp -a` parsers, the probe no-op) are never compiled
on this box — use the cross `cargo check`s above to honour the cross-platform
rule.

## Architecture

```
usb::spawn_watcher ──┐  mpsc<AppEvent>   ┌─ main.rs ──> ui::draw (src/ui.rs)      [TUI]
 (1 s poll)          ├───────────────────┤   owns a plain `App`, apply() no lock
                     │                   │
network::spawn_watcher┘                   └─ tray.rs  ──> ksni menu + notify-rust  [Linux only]
 (2 s poll, DNS + probes)                   owns `Arc<Mutex<App>>`
```

- `src/lib.rs` declares the modules `pub` (no re-export `pub use`s) so
  integration tests under `tests/` can reach them; the entry point is
  `src/main.rs`. **Don't move `main` into the lib.**
- `src/types.rs` — the domain vocabulary (`UsbDevice`, `Neighbor`, `IpNet`,
  `AppEvent`). Watchers never touch UI types; `ui.rs` renders `&App` and
  touches nothing else.
- `src/app.rs` — all state + reducers (`App::apply`) + key handling. Cursor
  clamping and subnet-selection preservation live here.
- `src/ui.rs` — pure ratatui rendering of `&App` and nothing else (the
  syscalls live in `main.rs` and `tray.rs`). Asserted against in
  `tests/ui_render.rs` with `TestBackend`; don't add state to the renderer.
- `src/tray.rs` — Linux-only. The `cfg(target_os = "linux")` gates are at
  `lib.rs:12` and the `main.rs` match arms, plus target-gated deps in
  `Cargo.toml`; the file itself carries no cfg. Menu rows, tooltip,
  notification texts, and the
  `EventPump` are pure functions/structs over `App` + injected `Notifier`,
  unit-tested without D-Bus. `ksni::blocking`; the menu is rebuilt on open via
  `menu_about_to_show` — the empty override *is* the implementation: ksni only
  rebuilds when the hook is user-overridden, so deleting this "dead" function
  (`tray.rs:368`) silently freezes a stale menu. Every menu label must pass
  through `escape_mnemonics` or `_` disappears (DBusMenu mnemonics).
- `src/usb.rs` — enumeration via `serialport::available_ports` polled at 1 s;
  diffing into `UsbAdded`/`UsbRemoved` lives in `refresh`. The VID/PID chip
  table (`chip_short_name`) **takes precedence over OS descriptor strings** —
  descriptors on CH340/STM32/Arduino clones are unreliable. Keep that order.
  Beware the module doc's "works identically on Linux, macOS, and Windows":
  `classify()` returns `None` on non-Linux/non-Windows, so the **macOS TUI
  lists no serial ports at all** (its `cu.usb*` names never match).
- `src/network.rs` — subnets via `if-addrs` (skip `169.254/16`), neighbours
  via `ip -j neigh` (Linux, JSON) or `arp -a` parsing (fallback, handles
  Windows dash-MACs and macOS/BSD parenthesised lines). Poll 2 s; STALE IPv4
  entries get one `ping -c 1 -w 1` per 10 s — Linux only: `spawn_probes` is a
  no-op elsewhere (`arp -a` exposes no state). `due_for_probe` does no I/O,
  takes an injected `Instant`, and only mutates the `ProbeLog` handed to it
  (`&mut` by design) — keep it that way. Hostname resolution goes through
  `resolve_hostname_using(ip, unicast, mdns)` — a seam for tests;
  mDNS shells out to `avahi-resolve` and tolerates its absence.
- `src/cli.rs` — hand-rolled arg parsing, no clap. `--tui` + `--notify-net` is
  a deliberate conflict; usage errors exit 2.

## Conventions

- Threads are named `watchtuier-*` (`watchtuier-usb`, `watchtuier-net`,
  `watchtuier-dns-<ip>`, `watchtuier-probe-<ip>`, `watchtuier-pump`).
- Errors: `anyhow::Result` only at the two entry points (`main`, `tray::run`);
  `parse_args` returns `Result<Mode, String>`, the data APIs return plain
  `Vec`, `spawn_watcher` uses `.expect()` at spawn. Watchers swallow OS errors
  and emit empty snapshots rather than killing the UI. Copy-to-clipboard is
  best-effort and recreates the `arboard` connection after a failure.
- Shutdown is a shared `Arc<AtomicBool>` checked with `Acquire`/`Release`;
  TUI exit must flip it before joining watcher threads. Do not `join()` the
  DNS/probe detached threads — they are fire-and-forget by design.
- **Tray quit sequencing is a hidden invariant:** the tray has *two* flags —
  `quit` (set by the menu item) and `shutdown` (watchers). The event pump
  exits only via channel disconnect, which works because the watcher joins
  drop the last `Sender`s *before* the pump join (`tray.rs:431–433`; the
  pump's quit check only runs after it receives an event). Never reorder
  those joins or keep an extra `Sender` clone alive — Quit would hang.
- Stale in-code comments to distrust: `main.rs:95–96` says "Don't block on
  join" directly above actual `join()` calls; `Cargo.toml:21` says the tray
  deps are "Linux/BSD" but they are gated to Linux only.
- **Notification suppression rules are load-bearing** (`EventPump`): nothing
  is notified until each stream's first snapshot ("primed"), and each
  neighbour IP is announced at most once per session (`announced_ips`) so
  STALE→gone→back flapping never re-notifies. If you change `apply`/pump
  ordering, update `src/tray.rs`'s tests, not just the code.
- Keep new decision logic as pure functions with injected inputs (clocks,
  lookup closures, `Notifier`) and unit-test it in the same module's `tests`
  submodule.

## Tests

- Unit tests exist only in `cli.rs`, `network.rs`, and `tray.rs` (tray's only
  exist on Linux); integration UI tests in `tests/ui_render.rs` (ratatui
  `TestBackend`). `usb.rs`, `app.rs`, `ui.rs`, `types.rs`, `main.rs` have no
  test module — notably the VID/PID chip table and `app.rs` cursor clamping /
  subnet-selection preservation are currently **untested**, so treat
  "load-bearing" claims about them as conventions, not verified behaviour,
  and consider adding tests before touching them.
- `ENV_MUTEX` serialises the PATH-shimming tests, but it is a private static
  inside `network.rs`'s `mod tests` — new PATH/env tests belong in that
  module and must take it (each test binary has its own copy; the lock does
  not protect across binaries/processes) and restore the old value. Note the
  shim helper does not restore PATH if the body panics, and a panicked guard
  poisons the mutex. `std::env::set_var` becomes `unsafe` under edition 2024 —
  an edition bump breaks these tests.
- `#[ignore]` marks tests that hit the **live network** or a **live
  clipboard/display session**. Never un-ignore them or add new un-ignored
  tests that require hardware/network the CI-less dev box may not have. The
  ignored avahi test hardcodes a real zeroconf device on the author's LAN.

## Environment quirks

- `.cargo-home/` is a local `CARGO_HOME` used by some agent harnesses to keep
  the crates registry in-repo; it is gitignored — don't commit it. Same for
  `.omo/` and `.codegraph` (tooling state).
- `target/` is gitignored. `Cargo.lock` **is** committed (binary crate).

## Don't

- `git push`, force-push, amend/rebase pushed history, or destructive git —
  unless the human explicitly asks. Offer the command instead of running it.
- Don't add a clap dependency or restructure the CLI surface; the flags and
  the `--tui`/`--notify-net` conflict are the public contract.
- Don't move tray code out from behind `cfg(target_os = "linux")`; the
  Windows/macOS builds must keep compiling with the `--tui`-only path.
- Don't trust OS USB descriptor strings for chip naming; extend the VID/PID
  table instead.
