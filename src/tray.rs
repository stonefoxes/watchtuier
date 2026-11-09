//! System-tray front-end: a StatusNotifierItem icon with a live menu and
//! desktop notifications for freshly connected USB serial devices.
//!
//! The tray owns no state of its own — everything is read from a shared
//! [`App`] updated by the same watchers the TUI uses. Menu layout and
//! notification text are pure functions over that state (see [`menu_rows`]
//! and the `*_note` helpers) so they can be tested without a D-Bus session.

use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use ksni::blocking::TrayMethods;
use ksni::menu::{MenuItem, StandardItem};
use ksni::ToolTip;

use crate::app::App;
use crate::types::{AppEvent, IpNet, Neighbor, UsbDevice};
use crate::{network, usb};

/// Freedesktop icon name; present in mainstream themes (Papirus, Adwaita…).
const ICON_NAME: &str = "drive-removable-media-usb";

type SharedState = Arc<Mutex<App>>;

// ---------------------------------------------------------------------------
// Menu model — plain data, fully unit-testable
// ---------------------------------------------------------------------------

/// One row of the tray menu, independent of any tray-toolkit types.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuRow {
    /// Disabled group header, e.g. `USB serial (2)`.
    Header(String),
    /// Disabled informational row, e.g. `none`.
    Info(String),
    /// Clickable row: copies `payload` (a devnode or IP address) to the
    /// clipboard when activated.
    Copy {
        label: String,
        payload: String,
    },
    Separator,
    /// The clickable quit entry.
    Quit,
}

/// Build the compact menu: a USB section, then one section per subnet that
/// currently has neighbours, then Quit. Mirrors the TUI's data in one
/// click — one compact line per device with MAC and liveness state, and
/// devnodes / IP addresses copy to the clipboard when clicked.
pub fn menu_rows(app: &App) -> Vec<MenuRow> {
    let mut rows = Vec::new();

    rows.push(MenuRow::Header(format!(
        "USB serial ({})",
        app.usb_devices.len()
    )));
    if app.usb_devices.is_empty() {
        rows.push(MenuRow::Info("none".into()));
    } else {
        for d in &app.usb_devices {
            let label = match d.chip.is_empty() {
                true => d.devnode.clone(),
                false => format!("{} — {}", d.devnode, d.chip),
            };
            rows.push(MenuRow::Copy {
                label,
                payload: d.devnode.clone(),
            });
        }
    }

    for (net, neighbors) in neighbors_by_network(app) {
        rows.push(MenuRow::Separator);
        rows.push(MenuRow::Header(format!(
            "{} ({})",
            net.label,
            neighbors.len()
        )));
        for n in neighbors {
            rows.push(MenuRow::Copy {
                label: neighbor_label(n),
                payload: n.ip.to_string(),
            });
        }
    }

    rows.push(MenuRow::Separator);
    rows.push(MenuRow::Quit);
    rows
}

/// One compact line per neighbour:
/// `192.168.228.8 — vrx-ec1d925f.local · fa:84:66:74:a2:6a · STALE`
/// (hostname omitted when unknown).
fn neighbor_label(n: &Neighbor) -> String {
    match n.hostname.as_deref().filter(|h| !h.is_empty()) {
        Some(host) => format!("{} — {} · {} · {}", n.ip, host, n.mac, n.state),
        None => format!("{} · {} · {}", n.ip, n.mac, n.state),
    }
}

/// Group neighbours under their subnet, keeping the watcher's network order.
/// Subnets with no neighbours are omitted to keep the menu compact.
fn neighbors_by_network(app: &App) -> Vec<(&IpNet, Vec<&Neighbor>)> {
    app.networks
        .iter()
        .map(|net| {
            (
                net,
                app.neighbors
                    .iter()
                    .filter(|n| match n.ip {
                        IpAddr::V4(v4) => net.network.contains(v4.octets()),
                        IpAddr::V6(_) => false,
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .filter(|(_, ns)| !ns.is_empty())
        .collect()
}

/// Tooltip title and description summarising current state.
pub fn tray_tooltip(app: &App) -> (String, String) {
    (
        "watchtuier".into(),
        format!(
            "{} USB serial · {} neighbour(s)",
            app.usb_devices.len(),
            app.neighbors.len()
        ),
    )
}

// ---------------------------------------------------------------------------
// Notification payloads — plain data, unit-testable
// ---------------------------------------------------------------------------

/// A desktop notification to fire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    pub summary: String,
    pub body: String,
}

/// "USB serial connected" for a freshly plugged (or re-enumerated) adapter.
pub fn usb_added_note(dev: &UsbDevice) -> Note {
    Note {
        summary: "USB serial connected".into(),
        body: match dev.chip.is_empty() {
            true => dev.devnode.clone(),
            false => format!(
                "{} — {} ({}:{})",
                dev.devnode, dev.chip, dev.vendor_id, dev.product_id
            ),
        },
    }
}

pub fn usb_removed_note(devnode: &str) -> Note {
    Note {
        summary: "USB serial disconnected".into(),
        body: devnode.to_string(),
    }
}

/// "Network neighbour appeared" — used only with `--notify-net`.
pub fn neighbor_note(n: &Neighbor) -> Note {
    Note {
        summary: "Network neighbour appeared".into(),
        body: format!("{} ({})", n.ip, n.iface),
    }
}

// ---------------------------------------------------------------------------
// Notifier abstraction
// ---------------------------------------------------------------------------

/// Where notifications go. Implemented by the desktop backend and by a
/// recorder in tests.
pub trait Notifier: Send + Sync + 'static {
    fn show(&self, note: &Note);
}

/// Fires freedesktop notifications via `notify-rust`.
struct DesktopNotifier;

impl Notifier for DesktopNotifier {
    fn show(&self, note: &Note) {
        let _ = notify_rust::Notification::new()
            .appname("watchtuier")
            .icon(ICON_NAME)
            .summary(&note.summary)
            .body(&note.body)
            .show();
    }
}

// ---------------------------------------------------------------------------
// Event pump — applies watcher events to the shared App and fires
// notifications. Pure logic on injected collaborators, so the suppression
// rules are testable.
// ---------------------------------------------------------------------------

/// Decides which watcher events become notifications.
///
/// Both watchers describe their *initial* inventory as a burst of
/// "added" events before the first snapshot, so nothing is notified until
/// each stream has primed with its first snapshot — otherwise every launch
/// would notify for every already-plugged device.
pub struct EventPump<N: Notifier> {
    notifier: N,
    /// Also notify for new network neighbours (opt-in: DHCP and container
    /// churn make these noisy).
    notify_net: bool,
    primed_usb: bool,
    primed_net: bool,
    /// Neighbour IPs already announced once this session; neighbour-table
    /// flapping (STALE → gone → back) must not re-notify.
    announced_ips: HashSet<IpAddr>,
}

impl<N: Notifier> EventPump<N> {
    pub fn new(notify_net: bool, notifier: N) -> Self {
        Self {
            notifier,
            notify_net,
            primed_usb: false,
            primed_net: false,
            announced_ips: HashSet::new(),
        }
    }

    /// Apply `event` to `app`, notifying as configured along the way.
    pub fn handle(&mut self, app: &mut App, event: AppEvent) {
        // Snapshot the previous neighbour IPs before `apply` overwrites
        // them — needed for the new-neighbour diff.
        let prev_ips: HashSet<IpAddr> = app.neighbors.iter().map(|n| n.ip).collect();

        match &event {
            AppEvent::UsbAdded(dev) => {
                if self.primed_usb {
                    self.notifier.show(&usb_added_note(dev));
                }
            }
            AppEvent::UsbRemoved(devnode) => {
                if self.primed_usb {
                    self.notifier.show(&usb_removed_note(devnode));
                }
            }
            AppEvent::UsbSnapshot(_) => self.primed_usb = true,
            AppEvent::NeighborSnapshot(neighbors) => {
                if self.primed_net && self.notify_net {
                    for n in neighbors.iter().filter(|n| !prev_ips.contains(&n.ip)) {
                        if self.announced_ips.insert(n.ip) {
                            self.notifier.show(&neighbor_note(n));
                        }
                    }
                }
                // Every observed IP counts as "seen" — including the initial
                // inventory and anything notified now — so it can never be
                // announced twice, even across neighbour-table flapping.
                for n in neighbors {
                    self.announced_ips.insert(n.ip);
                }
                self.primed_net = true;
            }
            AppEvent::NetworksSnapshot(_) => {}
        }

        app.apply(event);
    }
}

// ---------------------------------------------------------------------------
// ksni tray
// ---------------------------------------------------------------------------

struct WatchtuierTray {
    state: SharedState,
    quit: Arc<AtomicBool>,
    /// Clipboard connection for menu copy actions, created lazily on the
    /// first click and recreated after a failure (the session's clipboard
    /// service may restart under us).
    clipboard: Option<arboard::Clipboard>,
}

impl WatchtuierTray {
    /// Copy `text` to the system clipboard. Best-effort: on a headless or
    /// broken session this is a no-op — copying must never take the tray
    /// down with it.
    fn copy_to_clipboard(&mut self, text: String) {
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        if let Some(clipboard) = &mut self.clipboard {
            if clipboard.set_text(text).is_err() {
                // Stale connection — drop it so the next click reconnects.
                self.clipboard = None;
            }
        }
    }
}

impl ksni::Tray for WatchtuierTray {
    /// Left click on the icon opens the menu (the menu *is* the UI).
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        "watchtuier".into()
    }

    fn title(&self) -> String {
        "watchtuier".into()
    }

    fn icon_name(&self) -> String {
        ICON_NAME.into()
    }

    fn tool_tip(&self) -> ToolTip {
        let (title, description) = tray_tooltip(&self.state.lock().unwrap());
        ToolTip {
            icon_name: ICON_NAME.into(),
            icon_pixmap: Vec::new(),
            title,
            description,
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let app = self.state.lock().unwrap();
        menu_rows(&app)
            .into_iter()
            .map(|row| match row {
                MenuRow::Header(label) | MenuRow::Info(label) => MenuItem::Standard(StandardItem {
                    label: escape_mnemonics(&label),
                    enabled: false,
                    ..StandardItem::default()
                }),
                MenuRow::Copy { label, payload } => MenuItem::Standard(StandardItem {
                    label: escape_mnemonics(&label),
                    activate: Box::new(move |tray: &mut WatchtuierTray| {
                        tray.copy_to_clipboard(payload.clone())
                    }),
                    ..StandardItem::default()
                }),
                MenuRow::Separator => MenuItem::Separator,
                MenuRow::Quit => MenuItem::Standard(StandardItem {
                    label: "Quit".into(),
                    activate: Box::new(|tray: &mut WatchtuierTray| {
                        tray.quit.store(true, Ordering::Release)
                    }),
                    ..StandardItem::default()
                }),
            })
            .collect()
    }

    /// Overridden (without the default body) so ksni rebuilds the menu from
    /// live state every time it is opened.
    fn menu_about_to_show(&mut self) {}
}

/// DBusMenu treats `_` as a mnemonic marker; double it so labels show a
/// literal underscore.
fn escape_mnemonics(label: &str) -> String {
    label.replace('_', "__")
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Run the tray app until the user picks Quit in the menu.
pub fn run(notify_net: bool) -> anyhow::Result<()> {
    let state: SharedState = Arc::new(Mutex::new(App::new()));
    let quit = Arc::new(AtomicBool::new(false));
    let shutdown = Arc::new(AtomicBool::new(false));

    let (tx, rx) = mpsc::channel::<AppEvent>();
    let usb_handle = usb::spawn_watcher(tx.clone(), shutdown.clone());
    let net_handle = network::spawn_watcher(tx, shutdown.clone());

    let tray_handle = WatchtuierTray {
        state: Arc::clone(&state),
        quit: Arc::clone(&quit),
        clipboard: None,
    }
    .spawn()
    .map_err(|e| anyhow::anyhow!("failed to register tray icon: {e}"))?;

    // Event pump: apply watcher events to the shared state, notify, and
    // refresh the tray tooltip.
    let pump_handle = {
        let state = Arc::clone(&state);
        let tray_handle = tray_handle.clone();
        let quit_flag = Arc::clone(&quit);
        thread::Builder::new()
            .name("watchtuier-pump".into())
            .spawn(move || {
                let mut pump = EventPump::new(notify_net, DesktopNotifier);
                while let Ok(ev) = rx.recv() {
                    {
                        let Ok(mut app) = state.lock() else { break };
                        pump.handle(&mut app, ev);
                    }
                    // Refresh title/tooltip; the menu itself is rebuilt on
                    // open via `menu_about_to_show`.
                    let _ = tray_handle.update(|_tray| {});
                    if quit_flag.load(Ordering::Acquire) {
                        break;
                    }
                }
            })
            .expect("spawn event pump")
    };

    // Park the main thread until Quit is picked in the tray menu.
    while !quit.load(Ordering::Acquire) {
        thread::sleep(Duration::from_millis(200));
    }

    shutdown.store(true, Ordering::Release);
    let _ = usb_handle.join();
    let _ = net_handle.join();
    let _ = pump_handle.join();
    tray_handle.shutdown().wait();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records every notification instead of showing it.
    #[derive(Default)]
    struct RecordingNotifier(std::sync::Mutex<Vec<Note>>);

    impl Notifier for RecordingNotifier {
        fn show(&self, note: &Note) {
            self.0.lock().unwrap().push(note.clone());
        }
    }

    fn usb_dev(devnode: &str, chip: &str) -> UsbDevice {
        UsbDevice {
            devnode: devnode.into(),
            kind: crate::types::UsbKind::TtyUsb,
            chip: chip.into(),
            vendor_id: "0403".into(),
            product_id: "6001".into(),
            serial: String::new(),
        }
    }

    fn net_228() -> IpNet {
        IpNet {
            network: crate::types::Ipv4Network {
                addr: [192, 168, 228, 0],
                prefix: 24,
            },
            iface: "eth0".into(),
            label: "eth0 192.168.228.0/24".into(),
        }
    }

    fn neighbor(ip: &str, hostname: Option<&str>, iface: &str) -> Neighbor {
        Neighbor {
            ip: ip.parse().unwrap(),
            mac: "aa:bb:cc:dd:ee:ff".into(),
            hostname: hostname.map(String::from),
            iface: iface.into(),
            state: "STALE".into(),
        }
    }

    fn app_with_fixture() -> App {
        let mut app = App::new();
        app.apply(AppEvent::UsbSnapshot(vec![
            usb_dev("/dev/ttyUSB0", "FT232"),
            usb_dev("/dev/ttyACM0", "Pico"),
        ]));
        app.apply(AppEvent::NetworksSnapshot(vec![net_228()]));
        app.apply(AppEvent::NeighborSnapshot(vec![
            neighbor("192.168.228.8", Some("vrx-ec1d925f.local"), "eth0"),
            neighbor("192.168.228.53", None, "eth0"),
        ]));
        app
    }

    // ---- menu_rows ----

    #[test]
    fn menu_lists_usb_devices_compactly() {
        let rows = menu_rows(&app_with_fixture());
        assert_eq!(
            rows[0],
            MenuRow::Header("USB serial (2)".into()),
            "first row is the USB header with a count"
        );
        assert!(rows.contains(&MenuRow::Copy {
            label: "/dev/ttyUSB0 — FT232".into(),
            payload: "/dev/ttyUSB0".into(),
        }));
        assert!(rows.contains(&MenuRow::Copy {
            label: "/dev/ttyACM0 — Pico".into(),
            payload: "/dev/ttyACM0".into(),
        }));
        assert!(
            !rows
                .iter()
                .any(|r| matches!(r, MenuRow::Copy { label, .. } if label.contains("0403"))),
            "VID:PID stays out of the compact menu"
        );
    }

    #[test]
    fn menu_shows_none_when_no_usb_devices() {
        let rows = menu_rows(&App::new());
        assert_eq!(rows[0], MenuRow::Header("USB serial (0)".into()));
        assert_eq!(rows[1], MenuRow::Info("none".into()));
    }

    #[test]
    fn menu_groups_neighbours_by_subnet_with_host_mac_and_state() {
        let rows = menu_rows(&app_with_fixture());
        assert!(rows.contains(&MenuRow::Header("eth0 192.168.228.0/24 (2)".into())));
        assert!(rows.contains(&MenuRow::Copy {
            label: "192.168.228.8 — vrx-ec1d925f.local · aa:bb:cc:dd:ee:ff · STALE".into(),
            payload: "192.168.228.8".into(),
        }));
        assert!(rows.contains(&MenuRow::Copy {
            label: "192.168.228.53 · aa:bb:cc:dd:ee:ff · STALE".into(),
            payload: "192.168.228.53".into(),
        }));
    }

    #[test]
    fn copy_payload_is_the_bare_address_not_the_whole_label() {
        let rows = menu_rows(&app_with_fixture());
        for row in &rows {
            if let MenuRow::Copy { label, payload } = row {
                if label.starts_with("192.168.") {
                    assert_eq!(
                        payload.parse::<std::net::IpAddr>().unwrap().to_string(),
                        *payload,
                        "neighbour payloads must be the bare IP, not the label"
                    );
                } else {
                    assert!(
                        payload.starts_with("/dev/"),
                        "USB payloads must be the bare devnode: {payload}"
                    );
                }
            }
        }
    }

    #[test]
    fn menu_skips_empty_subnets_and_ends_with_quit() {
        let mut app = App::new();
        app.apply(AppEvent::NetworksSnapshot(vec![
            net_228(),
            IpNet {
                network: crate::types::Ipv4Network {
                    addr: [172, 17, 0, 0],
                    prefix: 16,
                },
                iface: "docker0".into(),
                label: "docker0 172.17.0.0/16".into(),
            },
        ]));
        app.apply(AppEvent::NeighborSnapshot(vec![neighbor(
            "192.168.228.8",
            Some("vrx-ec1d925f.local"),
            "eth0",
        )]));

        let rows = menu_rows(&app);
        assert!(
            !rows
                .iter()
                .any(|r| matches!(r, MenuRow::Header(h) if h.contains("docker0"))),
            "subnets without neighbours are omitted"
        );
        assert_eq!(*rows.last().unwrap(), MenuRow::Quit);
    }

    #[test]
    fn tooltip_summarises_counts() {
        let (title, desc) = tray_tooltip(&app_with_fixture());
        assert_eq!(title, "watchtuier");
        assert_eq!(desc, "2 USB serial · 2 neighbour(s)");
    }

    #[test]
    fn mnemonics_are_escaped() {
        assert_eq!(escape_mnemonics("weird_host_name"), "weird__host__name");
    }

    // ---- notification payloads ----

    #[test]
    fn usb_notes_carry_identity() {
        let note = usb_added_note(&usb_dev("/dev/ttyUSB0", "FT232"));
        assert_eq!(note.summary, "USB serial connected");
        assert_eq!(note.body, "/dev/ttyUSB0 — FT232 (0403:6001)");

        assert_eq!(
            usb_removed_note("/dev/ttyUSB0"),
            Note {
                summary: "USB serial disconnected".into(),
                body: "/dev/ttyUSB0".into(),
            }
        );
    }

    #[test]
    fn neighbor_note_carries_ip_and_iface() {
        let note = neighbor_note(&neighbor("192.168.228.8", None, "eth0"));
        assert_eq!(note.summary, "Network neighbour appeared");
        assert_eq!(note.body, "192.168.228.8 (eth0)");
    }

    // ---- EventPump: suppression and diffing rules ----

    fn new_pump(notify_net: bool) -> (EventPump<RecordingNotifier>, App) {
        (
            EventPump::new(notify_net, RecordingNotifier::default()),
            App::new(),
        )
    }

    fn recorded(pump: &EventPump<RecordingNotifier>) -> Vec<Note> {
        pump.notifier.0.lock().unwrap().clone()
    }

    #[test]
    fn startup_usb_burst_is_suppressed_until_first_snapshot() {
        let (mut pump, mut app) = new_pump(false);
        // The watcher emits UsbAdded for every plugged device *before* its
        // first snapshot — none of those may notify.
        pump.handle(
            &mut app,
            AppEvent::UsbAdded(usb_dev("/dev/ttyUSB0", "FT232")),
        );
        assert!(recorded(&pump).is_empty(), "startup burst must not notify");

        pump.handle(
            &mut app,
            AppEvent::UsbSnapshot(vec![usb_dev("/dev/ttyUSB0", "FT232")]),
        );
        assert!(recorded(&pump).is_empty());

        // A genuinely fresh device afterwards does notify.
        pump.handle(
            &mut app,
            AppEvent::UsbAdded(usb_dev("/dev/ttyACM1", "CP2102")),
        );
        assert_eq!(recorded(&pump).len(), 1);
        assert_eq!(recorded(&pump)[0].summary, "USB serial connected");
    }

    #[test]
    fn usb_removal_notifies() {
        let (mut pump, mut app) = new_pump(false);
        pump.handle(
            &mut app,
            AppEvent::UsbSnapshot(vec![usb_dev("/dev/ttyUSB0", "FT232")]),
        );
        pump.handle(&mut app, AppEvent::UsbRemoved("/dev/ttyUSB0".into()));
        let notes = recorded(&pump);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].summary, "USB serial disconnected");
        assert_eq!(notes[0].body, "/dev/ttyUSB0");
    }

    #[test]
    fn neighbor_notifications_are_opt_in_and_once_per_ip() {
        // Opted out (default): no notifications even for new neighbours.
        let (mut pump, mut app) = new_pump(false);
        pump.handle(&mut app, AppEvent::NeighborSnapshot(vec![]));
        pump.handle(
            &mut app,
            AppEvent::NeighborSnapshot(vec![neighbor("192.168.228.8", None, "eth0")]),
        );
        assert!(recorded(&pump).is_empty());

        // Opted in: first snapshot primes, new IP notifies once…
        let (mut pump, mut app) = new_pump(true);
        pump.handle(
            &mut app,
            AppEvent::NeighborSnapshot(vec![neighbor("192.168.228.8", None, "eth0")]),
        );
        assert!(
            recorded(&pump).is_empty(),
            "initial inventory must not notify"
        );

        pump.handle(
            &mut app,
            AppEvent::NeighborSnapshot(vec![
                neighbor("192.168.228.8", None, "eth0"),
                neighbor("192.168.228.53", None, "eth0"),
            ]),
        );
        assert_eq!(recorded(&pump).len(), 1);

        // …and table flapping (gone → back) does not re-notify.
        pump.handle(&mut app, AppEvent::NeighborSnapshot(vec![]));
        pump.handle(
            &mut app,
            AppEvent::NeighborSnapshot(vec![
                neighbor("192.168.228.8", None, "eth0"),
                neighbor("192.168.228.53", None, "eth0"),
            ]),
        );
        assert_eq!(recorded(&pump).len(), 1, "flapping must not re-notify");
    }

    // ---- live session clipboard (opt-in) ----

    #[test]
    #[ignore = "uses the live session clipboard; run with cargo test -- --ignored"]
    fn live_clipboard_roundtrip() {
        let mut cb = arboard::Clipboard::new().unwrap();
        cb.set_text("watchtuier-roundtrip").unwrap();
        assert_eq!(cb.get_text().unwrap(), "watchtuier-roundtrip");
    }

    /// Asserts the session clipboard equals `$WATCHTUIER_EXPECT_CLIP` —
    /// used to verify a real tray-menu click copied the right payload.
    #[test]
    #[ignore = "reads the live session clipboard; run with WATCHTUIER_EXPECT_CLIP=<value> cargo test -- --ignored"]
    fn live_clipboard_contains_expected() {
        let expected = std::env::var("WATCHTUIER_EXPECT_CLIP").expect("set WATCHTUIER_EXPECT_CLIP");
        let mut cb = arboard::Clipboard::new().unwrap();
        assert_eq!(cb.get_text().unwrap(), expected);
    }
}
