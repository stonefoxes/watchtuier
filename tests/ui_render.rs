//! Integration tests for the rendered UI surface.
//!
//! Uses ratatui's `TestBackend` so we can assert on actual cell content without
//! spawning a real terminal.

use std::net::IpAddr;

use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::Terminal;

use watchtuier::app::{App, Focus};
use watchtuier::types::{AppEvent, IpNet, Ipv4Network, Neighbor, UsbDevice, UsbKind};
use watchtuier::ui;

fn render(app: &App, w: u16, h: u16) -> Buffer {
    let backend = TestBackend::new(w, h);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| ui::draw(f, app)).unwrap();
    term.backend().buffer().clone()
}

fn line_contains(buf: &Buffer, row: u16, needle: &str) -> bool {
    let line: String = (0..buf.area.width)
        .map(|x| buf[(x, row)].symbol().chars().next().unwrap_or(' '))
        .collect();
    line.contains(needle)
}

fn anywhere_contains(buf: &Buffer, needle: &str) -> bool {
    for row in 0..buf.area.height {
        if line_contains(buf, row, needle) {
            return true;
        }
    }
    false
}

#[test]
fn both_panels_render_with_titles() {
    let app = App::new();
    let buf = render(&app, 120, 30);
    assert!(anywhere_contains(&buf, "USB serial devices"));
    assert!(anywhere_contains(&buf, "Network neighbours"));
}

#[test]
fn usb_panel_lists_devices_with_chip_info() {
    let mut app = App::new();
    app.apply(AppEvent::UsbSnapshot(vec![
        UsbDevice {
            devnode: "/dev/ttyUSB0".into(),
            kind: UsbKind::TtyUsb,
            chip: "FTDI FT232 Serial (UART) IC".into(),
            vendor_id: "0403".into(),
            product_id: "6001".into(),
            serial: "ABCD1234".into(),
        },
        UsbDevice {
            devnode: "/dev/ttyACM0".into(),
            kind: UsbKind::TtyAcm,
            chip: "WCH (QinHeng) CH340".into(),
            vendor_id: "1a86".into(),
            product_id: "7523".into(),
            serial: String::new(),
        },
    ]));
    let buf = render(&app, 140, 30);
    assert!(anywhere_contains(&buf, "/dev/ttyUSB0"));
    assert!(anywhere_contains(&buf, "FT232"));
    assert!(anywhere_contains(&buf, "/dev/ttyACM0"));
    assert!(anywhere_contains(&buf, "CH340"));
    assert!(anywhere_contains(&buf, "0403:6001"));
}

#[test]
fn latest_usb_device_is_marked_yellow() {
    let mut app = App::new();
    app.apply(AppEvent::UsbSnapshot(vec![UsbDevice {
        devnode: "/dev/ttyUSB1".into(),
        kind: UsbKind::TtyUsb,
        chip: "Silicon Labs CP210x".into(),
        vendor_id: "10c4".into(),
        product_id: "ea60".into(),
        serial: String::new(),
    }]));
    // Now signal that /dev/ttyUSB1 was just added (the latest).
    app.apply(AppEvent::UsbAdded(UsbDevice {
        devnode: "/dev/ttyUSB1".into(),
        kind: UsbKind::TtyUsb,
        chip: "Silicon Labs CP210x".into(),
        vendor_id: "10c4".into(),
        product_id: "ea60".into(),
        serial: String::new(),
    }));
    assert_eq!(app.latest_usb.as_deref(), Some("/dev/ttyUSB1"));
    assert!(app.is_latest_usb("/dev/ttyUSB1"));
    assert!(!app.is_latest_usb("/dev/ttyUSB9"));
}

#[test]
fn network_selection_filters_neighbors() {
    let mut app = App::new();
    let eno1 = IpNet {
        network: Ipv4Network {
            addr: [192, 168, 116, 0],
            prefix: 24,
        },
        iface: "eno1".into(),
        label: "eno1 192.168.116.0/24".into(),
    };
    let docker = IpNet {
        network: Ipv4Network {
            addr: [172, 17, 0, 0],
            prefix: 16,
        },
        iface: "docker0".into(),
        label: "docker0 172.17.0.0/16".into(),
    };
    app.apply(AppEvent::NetworksSnapshot(vec![eno1, docker]));
    app.apply(AppEvent::NeighborSnapshot(vec![
        Neighbor {
            ip: "192.168.116.254".parse().unwrap(),
            mac: "cc:2d:e0:11:99:e1".into(),
            hostname: None,
            iface: "eno1".into(),
            state: "REACHABLE".into(),
        },
        Neighbor {
            ip: "172.17.0.2".parse().unwrap(),
            mac: "02:42:ac:11:00:02".into(),
            hostname: None,
            iface: "docker0".into(),
            state: "REACHABLE".into(),
        },
    ]));

    // Default selection: first network (eno1). Only 192.168.116.x is visible.
    let visible = app.visible_neighbor_rows();
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].0, IpAddr::V4([192, 168, 116, 254].into()));

    // Cycle to docker0 (right arrow) and the second neighbour appears.
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::empty()));
    let visible = app.visible_neighbor_rows();
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].0, IpAddr::V4([172, 17, 0, 2].into()));

    // Header reflects the current selection.
    let buf = render(&app, 140, 30);
    assert!(anywhere_contains(&buf, "docker0 172.17.0.0/16"));
}

#[test]
fn latest_neighbor_is_marked_after_snapshot_diff() {
    let mut app = App::new();
    app.apply(AppEvent::NetworksSnapshot(vec![IpNet {
        network: Ipv4Network {
            addr: [192, 168, 116, 0],
            prefix: 24,
        },
        iface: "eno1".into(),
        label: "eno1 192.168.116.0/24".into(),
    }]));
    app.apply(AppEvent::NeighborSnapshot(vec![Neighbor {
        ip: "192.168.116.254".parse().unwrap(),
        mac: "cc:2d:e0:11:99:e1".into(),
        hostname: None,
        iface: "eno1".into(),
        state: "REACHABLE".into(),
    }]));

    // Second snapshot adds a new neighbour — it becomes the latest.
    app.apply(AppEvent::NeighborSnapshot(vec![
        Neighbor {
            ip: "192.168.116.254".parse().unwrap(),
            mac: "cc:2d:e0:11:99:e1".into(),
            hostname: None,
            iface: "eno1".into(),
            state: "REACHABLE".into(),
        },
        Neighbor {
            ip: "192.168.116.10".parse().unwrap(),
            mac: "aa:bb:cc:dd:ee:ff".into(),
            hostname: None,
            iface: "eno1".into(),
            state: "REACHABLE".into(),
        },
    ]));
    assert_eq!(
        app.latest_neighbor,
        Some(IpAddr::V4([192, 168, 116, 10].into()))
    );
}

#[test]
fn network_panel_renders_hostname_instead_of_mac() {
    let mut app = App::new();
    app.apply(AppEvent::NetworksSnapshot(vec![IpNet {
        network: Ipv4Network {
            addr: [192, 168, 116, 0],
            prefix: 24,
        },
        iface: "eno1".into(),
        label: "eno1 192.168.116.0/24".into(),
    }]));
    app.apply(AppEvent::NeighborSnapshot(vec![
        Neighbor {
            ip: "192.168.116.1".parse().unwrap(),
            mac: "aa:bb:cc:dd:ee:ff".into(),
            hostname: Some("router.lan".into()),
            iface: "eno1".into(),
            state: "REACHABLE".into(),
        },
        Neighbor {
            ip: "192.168.116.50".parse().unwrap(),
            mac: "11:22:33:44:55:66".into(),
            hostname: None,
            iface: "eno1".into(),
            state: "STALE".into(),
        },
    ]));
    app.focus = Focus::Net;
    let buf = render(&app, 140, 30);

    assert!(anywhere_contains(&buf, "Hostname"));
    assert!(anywhere_contains(&buf, "router.lan"));
    assert!(anywhere_contains(&buf, "—"));
    assert!(!anywhere_contains(&buf, "aa:bb:cc:dd:ee:ff"));
    assert!(!anywhere_contains(&buf, "11:22:33:44:55:66"));
}

#[test]
fn focus_switches_panels_with_tab() {
    let mut app = App::new();
    assert_eq!(app.focus, Focus::Usb);
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()));
    assert_eq!(app.focus, Focus::Net);
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()));
    assert_eq!(app.focus, Focus::Usb);
}

#[test]
fn quit_key_exits() {
    let mut app = App::new();
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::empty()));
    assert!(app.quit);
    let mut app = App::new();
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
    assert!(app.quit);
}
