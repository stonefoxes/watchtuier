//! USB serial device discovery via the `serialport` crate.
//!
//! Enumerates every USB-backed serial port and derives chip information from
//! the USB VID/PID + the manufacturer/product strings reported by the OS. A
//! vendor-id fallback table covers adapters whose descriptors are missing or
//! stripped down. Polls every second — works identically on Linux, macOS, and
//! Windows because `serialport` abstracts the platform-specific enumeration.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use serialport::{available_ports, SerialPortType};

use crate::types::{AppEvent, UsbDevice, UsbKind};

const REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// Snapshot every currently-plugged USB serial adapter.
pub fn enumerate() -> Vec<UsbDevice> {
    let ports = match available_ports() {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };
    let mut out: Vec<UsbDevice> = ports
        .into_iter()
        .filter_map(|info| {
            let kind = classify(&info.port_name)?;
            let SerialPortType::UsbPort(usb) = info.port_type else {
                return None;
            };
            let vendor_id = format!("{:04x}", usb.vid);
            let product_id = format!("{:04x}", usb.pid);

            let chip = build_chip(
                &vendor_id,
                &product_id,
                usb.product.as_deref(),
                usb.manufacturer.as_deref(),
            );

            Some(UsbDevice {
                devnode: info.port_name,
                kind,
                chip,
                vendor_id,
                product_id,
                serial: usb.serial_number.unwrap_or_default(),
            })
        })
        .collect();
    out.sort_by(|a, b| a.devnode.cmp(&b.devnode));
    out
}

/// Classify a port name into our `UsbKind` enum, filtering out ports that
/// aren't relevant (e.g. built-in `/dev/ttyS0` on Linux).
fn classify(port_name: &str) -> Option<UsbKind> {
    #[cfg(target_os = "linux")]
    {
        if port_name.starts_with("/dev/ttyUSB") {
            Some(UsbKind::TtyUsb)
        } else if port_name.starts_with("/dev/ttyACM") {
            Some(UsbKind::TtyAcm)
        } else {
            None
        }
    }
    #[cfg(windows)]
    {
        let upper = port_name.to_uppercase();
        if upper.starts_with("COM") {
            Some(UsbKind::Com)
        } else {
            None
        }
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = port_name;
        None
    }
}

/// Combine OS-reported strings with our VID/PID lookup. We PREFER the
/// short chip name from the lookup table because OS-reported USB descriptors
/// are often misleading: CH340/CH341 chips frequently ship with the
/// descriptor "USB2.0-Serial", STM32 CDC gadgets report "Linux Foundation",
/// and Arduino clones use a generic Arduino string. The VID/PID pair is the
/// only reliable identifier, so it wins whenever we recognise it.
fn build_chip(
    vendor_id: &str,
    product_id: &str,
    product: Option<&str>,
    manufacturer: Option<&str>,
) -> String {
    if let Some(short) = chip_short_name(vendor_id, product_id) {
        return short.to_string();
    }
    if let Some(prod) = product.filter(|s| !s.is_empty()) {
        let mfg = manufacturer.filter(|m| !m.is_empty()).unwrap_or("");
        return if mfg.is_empty() {
            prod.to_string()
        } else {
            format!("{mfg} {prod}")
        };
    }
    vendor_name(vendor_id).to_string()
}

fn chip_short_name(vendor_id: &str, product_id: &str) -> Option<&'static str> {
    Some(match (vendor_id, product_id) {
        // FTDI FT232 / FT2232 / FT4232 / FT232H / FT231X family
        ("0403", "6001") => "FT232",
        ("0403", "6006") => "FT232R",
        ("0403", "6010") => "FT2232",
        ("0403", "6011") => "FT4232",
        ("0403", "6014") => "FT232H",
        ("0403", "6015") => "FT231X",
        ("0403", "7c70") => "FT2232C",
        ("0403", "8372") => "FT4222",
        // WCH (QinHeng) — CH340 / CH341 / CH9102 family
        ("1a86", "5512") => "CH341",
        ("1a86", "5523") => "CH341",
        ("1a86", "7522") => "CH340",
        ("1a86", "7523") => "CH340",
        ("1a86", "55d4") => "CH9102",
        ("1a86", "55dd") => "CH549",
        // Silicon Labs CP210x family
        ("10c4", "ea60") => "CP2102",
        ("10c4", "ea61") => "CP2104",
        ("10c4", "ea63") => "CP2108",
        ("10c4", "ea70") => "CP2105",
        ("10c4", "ea71") => "CP2109",
        ("10c4", "ea9f") => "BGM13P",
        // Prolific PL2303 family
        ("067b", "2303") => "PL2303",
        ("067b", "23a3") => "PL2303GC",
        ("067b", "23b3") => "PL2303GB",
        ("067b", "23c3") => "PL2303GS",
        ("067b", "23d3") => "PL2303GT",
        ("067b", "23e3") => "PL2303GL",
        ("067b", "23f3") => "PL2303GS-A",
        // STMicroelectronics STM32 USB CDC virtual COM port
        ("0483", "5740") => "STM32",
        ("0483", "df11") => "STM32 DFU",
        // Espressif USB-JTAG/Serial (ESP32-S2 / S3 / C3 etc.)
        ("303a", "4001") => "ESP32-S2",
        ("303a", "4002") => "ESP32-S3",
        ("303a", "4003") => "ESP32-C3",
        ("303a", "1001") => "ESP32",
        // SEGGER J-Link debug probes
        ("1366", "0101") => "J-Link",
        ("1366", "0105") => "J-Link",
        ("1366", "1015") => "J-Link",
        ("1366", "1050") => "J-Link OB",
        ("1366", "1051") => "J-Link OB",
        // Atmel / Microchip EDBG and at90usb
        ("03eb", "2111") => "at90usb",
        ("03eb", "6124") => "EDBG",
        ("03eb", "2104") => "JTAGICE3",
        // Arduino LLC (whole vendor) — varies by board, all "Arduino"
        ("2341", _) => "Arduino",
        // Adafruit (Feather / Metro / Trinket family)
        ("239a", _) => "Adafruit",
        // Raspberry Pi Pico / Pi Debug Probe
        ("2e8a", "000a") => "Pico",
        ("2e8a", "000c") => "Pico Probe",
        // Microchip MCP2200
        ("04d8", "00df") => "MCP2200",
        _ => return None,
    })
}

/// Generic vendor name used when the device is genuinely unknown.
fn vendor_name(vendor_id: &str) -> &'static str {
    match vendor_id {
        "0403" => "FTDI",
        "1a86" => "WCH",
        "10c4" => "Silicon Labs",
        "067b" => "Prolific",
        "0483" => "STMicroelectronics",
        "1d50" => "OpenMoko",
        "303a" => "Espressif",
        "04d8" => "Microchip",
        "1366" => "SEGGER",
        "03eb" => "Atmel",
        "2e8a" => "Raspberry Pi",
        _ => "Unknown USB serial",
    }
}

/// Spawn the USB watcher thread. Sends an initial snapshot, then re-enumerates
/// every [`REFRESH_INTERVAL`] (1s). 1s latency is acceptable UX and keeps the
/// implementation portable across Linux/macOS/Windows where no unified
/// device-change API exists.
pub fn spawn_watcher(tx: Sender<AppEvent>, shutdown: Arc<AtomicBool>) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("watchtuier-usb".into())
        .spawn(move || run_watcher(tx, shutdown))
        .expect("spawn usb watcher")
}

fn run_watcher(tx: Sender<AppEvent>, shutdown: Arc<AtomicBool>) {
    let mut last: HashMap<String, UsbDevice> = HashMap::new();
    refresh(&tx, &mut last);

    while !shutdown.load(Ordering::Acquire) {
        thread::sleep(REFRESH_INTERVAL);
        refresh(&tx, &mut last);
    }
}

fn refresh(tx: &Sender<AppEvent>, last: &mut HashMap<String, UsbDevice>) {
    let snapshot = enumerate();
    let now_keys: std::collections::HashSet<&String> =
        snapshot.iter().map(|d| &d.devnode).collect();

    for d in &snapshot {
        let changed = match last.get(&d.devnode) {
            None => true,
            Some(prev) => prev.vendor_id != d.vendor_id || prev.product_id != d.product_id,
        };
        if changed {
            let _ = tx.send(AppEvent::UsbAdded(d.clone()));
        }
    }
    for devnode in last.keys() {
        if !now_keys.contains(devnode) {
            let _ = tx.send(AppEvent::UsbRemoved(devnode.clone()));
        }
    }

    last.clear();
    for d in &snapshot {
        last.insert(d.devnode.clone(), d.clone());
    }
    let _ = tx.send(AppEvent::UsbSnapshot(snapshot));
}
