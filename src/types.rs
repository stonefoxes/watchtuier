//! Shared domain types used across modules.

use std::net::IpAddr;

/// A USB serial device exposed as /dev/ttyUSB* or /dev/ttyACM*.
#[derive(Clone, Debug)]
pub struct UsbDevice {
    /// Absolute device node path, e.g. `/dev/ttyUSB0`.
    pub devnode: String,
    /// Subsystem family derived from the device path: `ttyUSB` or `ttyACM`.
    pub kind: UsbKind,
    /// Human-readable chip description, e.g. `FTDI FT232 Serial (UART) IC`.
    pub chip: String,
    /// Raw USB vendor id as 4 lowercase hex digits, e.g. `0403`.
    pub vendor_id: String,
    /// Raw USB product id as 4 lowercase hex digits, e.g. `6001`.
    pub product_id: String,
    /// USB iSerial string if available (empty when missing).
    pub serial: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsbKind {
    /// Linux `/dev/ttyUSB*` — typically FT232/CH340/CP2102-style adapters.
    TtyUsb,
    /// Linux `/dev/ttyACM*` — USB CDC ACM, e.g. microcontrollers.
    TtyAcm,
    /// Windows `COMx` — any USB-to-serial adapter surfaced as a COM port.
    Com,
}

impl UsbKind {
    pub fn as_str(self) -> &'static str {
        match self {
            UsbKind::TtyUsb => "ttyUSB",
            UsbKind::TtyAcm => "ttyACM",
            UsbKind::Com => "COM",
        }
    }
}

/// An IPv4 subnet derived from a local interface address.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct IpNet {
    pub network: Ipv4Network,
    pub iface: String,
    pub label: String,
}

/// Minimal IPv4 network (address + prefix length) without pulling in the
/// `ipnet` crate. Stored in canonical network form.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Ipv4Network {
    pub addr: [u8; 4],
    pub prefix: u8,
}

impl Ipv4Network {
    pub fn contains(&self, ip: [u8; 4]) -> bool {
        if self.prefix == 0 {
            return true;
        }
        let host_bits = (32 - self.prefix) as u32;
        let net = u32::from_be_bytes(self.addr);
        let target = u32::from_be_bytes(ip);
        if host_bits >= 32 {
            return true;
        }
        let mask = !0u32 << host_bits;
        (net & mask) == (target & mask)
    }

    pub fn display(&self) -> String {
        format!(
            "{}.{}.{}.{}",
            self.addr[0], self.addr[1], self.addr[2], self.addr[3]
        ) + "/"
            + &self.prefix.to_string()
    }
}

/// One entry from `ip neigh`, filtered to a single IPv4 subnet.
#[derive(Clone, Debug)]
pub struct Neighbor {
    pub ip: IpAddr,
    pub mac: String,
    /// Hostname from unicast reverse DNS, falling back to mDNS (Avahi) for
    /// zeroconf-only devices. Populated asynchronously by the watcher;
    /// `None` means the lookup is pending or has not produced a result yet.
    pub hostname: Option<String>,
    pub iface: String,
    pub state: String,
}

/// Cross-thread events emitted by background watchers.
pub enum AppEvent {
    /// Full USB snapshot after enumeration/refresh.
    UsbSnapshot(Vec<UsbDevice>),
    /// A new USB device appeared since last snapshot.
    UsbAdded(UsbDevice),
    /// A USB device node disappeared.
    UsbRemoved(String),
    /// Full neighbour table snapshot for all interfaces.
    NeighborSnapshot(Vec<Neighbor>),
    /// Available interface IPv4 networks were recomputed.
    NetworksSnapshot(Vec<IpNet>),
}
