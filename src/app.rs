//! App state and event handling.

use std::net::IpAddr;

use crossterm::event::{KeyCode, KeyEvent};

use crate::types::{AppEvent, IpNet, Neighbor, UsbDevice};

/// Which panel currently has keyboard focus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Usb,
    Net,
}

/// Top-level UI state.
#[derive(Debug)]
pub struct App {
    /// All currently-known USB serial devices, sorted by devnode.
    pub usb_devices: Vec<UsbDevice>,
    /// Most recently plugged USB device devnode (for highlighting).
    pub latest_usb: Option<String>,

    /// All locally visible IPv4 subnets.
    pub networks: Vec<IpNet>,
    /// Index into `networks` for the currently selected subnet.
    pub selected_network: usize,
    /// All neighbours across all interfaces.
    pub neighbors: Vec<Neighbor>,
    /// Most recently observed neighbour IP (for highlighting).
    pub latest_neighbor: Option<IpAddr>,

    pub focus: Focus,
    /// USB list cursor (row index).
    pub usb_cursor: usize,
    /// Neighbour list cursor.
    pub net_cursor: usize,

    pub quit: bool,

    /// Most recent status message (errors, counts, etc.).
    pub status: String,
}

impl App {
    pub fn new() -> Self {
        Self {
            usb_devices: Vec::new(),
            latest_usb: None,
            networks: Vec::new(),
            selected_network: 0,
            neighbors: Vec::new(),
            latest_neighbor: None,
            focus: Focus::Usb,
            usb_cursor: 0,
            net_cursor: 0,
            quit: false,
            status:
                "Press q to quit · Tab to switch panels · ↑/↓ to scroll · ←/→ to switch networks"
                    .to_string(),
        }
    }

    /// The selected subnet, if any.
    pub fn selected_network(&self) -> Option<&IpNet> {
        self.networks.get(self.selected_network)
    }

    /// Neighbours that belong to the currently selected subnet.
    pub fn visible_neighbors(&self) -> Vec<&Neighbor> {
        let Some(net) = self.selected_network() else {
            return Vec::new();
        };
        let target = net.network;
        self.neighbors
            .iter()
            .filter(|n| {
                if let IpAddr::V4(v4) = n.ip {
                    target.contains(v4.octets())
                } else {
                    false
                }
            })
            .collect()
    }

    /// Apply an event from a background watcher.
    pub fn apply(&mut self, event: AppEvent) {
        match event {
            AppEvent::UsbSnapshot(snap) => {
                self.usb_devices = snap;
                self.clamp_usb_cursor();
                self.status = format!(
                    "{} USB serial device(s) · {} network(s) visible",
                    self.usb_devices.len(),
                    self.networks.len()
                );
            }
            AppEvent::UsbAdded(dev) => {
                self.latest_usb = Some(dev.devnode.clone());
                self.status = format!("USB plugged: {} ({})", dev.devnode, dev.chip);
            }
            AppEvent::UsbRemoved(devnode) => {
                self.status = format!("USB unplugged: {devnode}");
            }
            AppEvent::NetworksSnapshot(nets) => {
                // Preserve selection by matching network+iface if possible.
                let prev = self
                    .networks
                    .get(self.selected_network)
                    .map(|n| (n.network, n.iface.clone()));
                self.networks = nets;
                let new_idx = match prev {
                    Some(key) => self
                        .networks
                        .iter()
                        .position(|n| (n.network, &n.iface) == (key.0, &key.1))
                        .unwrap_or(0),
                    None => self.selected_network,
                };
                self.selected_network = new_idx.min(self.networks.len().saturating_sub(1));
                self.status = format!(
                    "{} USB serial device(s) · {} network(s) visible",
                    self.usb_devices.len(),
                    self.networks.len()
                );
            }
            AppEvent::NeighborSnapshot(n) => {
                // Detect new arrivals against the previous table.
                let prev: std::collections::HashSet<IpAddr> =
                    self.neighbors.iter().map(|x| x.ip).collect();
                let latest = n.iter().find(|x| !prev.contains(&x.ip));
                if let Some(new) = latest {
                    self.latest_neighbor = Some(new.ip);
                    self.status = format!("Neighbour appeared: {}", new.ip);
                }
                self.neighbors = n;
                self.clamp_net_cursor();
            }
        }
    }

    /// Handle a keyboard event.
    pub fn handle_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Usb => Focus::Net,
                    Focus::Net => Focus::Usb,
                };
            }
            KeyCode::Char('j') | KeyCode::Down => match self.focus {
                Focus::Usb => {
                    if self.usb_cursor + 1 < self.usb_devices.len() {
                        self.usb_cursor += 1;
                    }
                }
                Focus::Net => {
                    let n = self.visible_neighbors().len();
                    if self.net_cursor + 1 < n {
                        self.net_cursor += 1;
                    }
                }
            },
            KeyCode::Char('k') | KeyCode::Up => match self.focus {
                Focus::Usb => {
                    if self.usb_cursor > 0 {
                        self.usb_cursor -= 1;
                    }
                }
                Focus::Net => {
                    if self.net_cursor > 0 {
                        self.net_cursor -= 1;
                    }
                }
            },
            KeyCode::Left => self.cycle_network(-1),
            KeyCode::Right => self.cycle_network(1),
            KeyCode::Char('h') => self.cycle_network(-1),
            KeyCode::Char('l') => self.cycle_network(1),
            _ => {}
        }
    }

    fn cycle_network(&mut self, dir: i32) {
        if self.networks.is_empty() {
            return;
        }
        let len = self.networks.len() as i32;
        let mut i = self.selected_network as i32 + dir;
        // wrap-around
        i = ((i % len) + len) % len;
        self.selected_network = i as usize;
        self.net_cursor = 0;
        let net = &self.networks[i as usize];
        self.status = format!("Selected: {}", net.label);
    }

    fn clamp_usb_cursor(&mut self) {
        if self.usb_cursor >= self.usb_devices.len() {
            self.usb_cursor = self.usb_devices.len().saturating_sub(1);
        }
    }
    fn clamp_net_cursor(&mut self) {
        let n = self.visible_neighbors().len();
        if self.net_cursor >= n {
            self.net_cursor = n.saturating_sub(1);
        }
    }

    /// True when the given devnode is the latest-plugged device.
    pub fn is_latest_usb(&self, devnode: &str) -> bool {
        self.latest_usb.as_deref() == Some(devnode)
    }

    /// True when the given IP is the latest-seen neighbour.
    pub fn is_latest_neighbor(&self, ip: IpAddr) -> bool {
        self.latest_neighbor == Some(ip)
    }

    /// Compute the visible neighbour list once for both cursor math and UI.
    pub fn visible_neighbor_rows(&self) -> Vec<(IpAddr, Option<String>, String, String)> {
        self.visible_neighbors()
            .into_iter()
            .map(|n| (n.ip, n.hostname.clone(), n.iface.clone(), n.state.clone()))
            .collect()
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}
