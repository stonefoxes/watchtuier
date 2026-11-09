//! Network neighbour discovery and local subnet enumeration.
//!
//! Local subnets are read via the cross-platform `if-addrs` crate. The
//! neighbour table comes from `ip neigh` on Linux (rich JSON output with
//! state) and `arp -a` everywhere else (BSD/macOS/Windows). The poller thread
//! emits snapshots every 2 seconds.
//!
//! Hostnames are resolved per neighbour IP: unicast reverse DNS first, then a
//! multicast-DNS (Avahi) fallback for zeroconf devices that never appear in a
//! unicast resolver's reverse zone.
//!
//! Liveness: the kernel keeps `STALE` entries (with MAC) long after a device
//! is physically gone. Every [`PROBE_INTERVAL`] each STALE neighbour is
//! nudged with a single ICMP echo so the kernel re-verifies it: present
//! devices come back `REACHABLE`, disconnected ones fail verification, lose
//! their link-layer address and drop out of the next snapshot.

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use if_addrs::IfAddr;
#[cfg(target_os = "linux")]
use serde::Deserialize;

use crate::types::{AppEvent, IpNet, Ipv4Network, Neighbor};

const POLL_INTERVAL: Duration = Duration::from_secs(2);
const PROBE_INTERVAL: Duration = Duration::from_secs(10);

type HostnameCache = Arc<Mutex<HashMap<IpAddr, String>>>;
type PendingSet = Arc<Mutex<HashSet<IpAddr>>>;
/// Last liveness-probe time per neighbour IP.
type ProbeLog = HashMap<IpAddr, Instant>;

/// Spawn the network poller thread. Emits a `NetworksSnapshot` and a
/// `NeighborSnapshot` immediately, then again every [`POLL_INTERVAL`].
pub fn spawn_watcher(tx: Sender<AppEvent>, shutdown: Arc<AtomicBool>) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("watchtuier-net".into())
        .spawn(move || run_watcher(tx, shutdown))
        .expect("spawn network watcher")
}

fn run_watcher(tx: Sender<AppEvent>, shutdown: Arc<AtomicBool>) {
    let cache: HostnameCache = Arc::new(Mutex::new(HashMap::new()));
    let pending: PendingSet = Arc::new(Mutex::new(HashSet::new()));
    let mut probes: ProbeLog = HashMap::new();

    let snapshot = |probes: &mut ProbeLog| {
        let neighbors = list_neighbors_with_dns(&cache, &pending);
        spawn_probes(&due_for_probe(&neighbors, probes, Instant::now()));
        neighbors
    };

    let _ = tx.send(AppEvent::NetworksSnapshot(list_networks()));
    let _ = tx.send(AppEvent::NeighborSnapshot(snapshot(&mut probes)));

    while !shutdown.load(Ordering::Acquire) {
        thread::sleep(POLL_INTERVAL);
        let _ = tx.send(AppEvent::NetworksSnapshot(list_networks()));
        let _ = tx.send(AppEvent::NeighborSnapshot(snapshot(&mut probes)));
    }
}

/// Read the raw neighbour table and merge in any cached reverse-DNS hostnames.
/// For IPs that have never been resolved, spawn a detached resolver thread so
/// the next poll cycle can pick up the result. The threads write back into
/// `cache` via shared Arc and remove themselves from `pending` when done.
fn list_neighbors_with_dns(cache: &HostnameCache, pending: &PendingSet) -> Vec<Neighbor> {
    let mut neighbors = list_neighbors();

    // Pick IPs that need resolution (not cached, not already pending).
    let to_resolve: Vec<IpAddr> = {
        let cache_lock = cache.lock().unwrap();
        let pending_lock = pending.lock().unwrap();
        neighbors
            .iter()
            .map(|n| n.ip)
            .filter(|ip| !cache_lock.contains_key(ip) && !pending_lock.contains(ip))
            .collect()
    };

    {
        let mut pending_lock = pending.lock().unwrap();
        for ip in &to_resolve {
            pending_lock.insert(*ip);
        }
    }

    for ip in to_resolve {
        let cache_clone = Arc::clone(cache);
        let pending_clone = Arc::clone(pending);
        thread::Builder::new()
            .name(format!("watchtuier-dns-{ip}"))
            .spawn(move || {
                let host = resolve_hostname(&ip);
                {
                    let mut cache_lock = cache_clone.lock().unwrap();
                    cache_lock.insert(ip, host);
                }
                let mut pending_lock = pending_clone.lock().unwrap();
                pending_lock.remove(&ip);
            })
            .ok();
    }

    // Prune cache entries that have dropped out of the neighbour table so the
    // cache can't grow unbounded across DHCP churn.
    {
        let current: HashSet<IpAddr> = neighbors.iter().map(|n| n.ip).collect();
        let mut cache_lock = cache.lock().unwrap();
        cache_lock.retain(|ip, _| current.contains(ip));
    }

    // Apply cached hostnames (may be empty for unresolved / pending IPs).
    let cache_lock = cache.lock().unwrap();
    for n in &mut neighbors {
        if let Some(host) = cache_lock.get(&n.ip) {
            n.hostname = Some(host.clone());
        }
    }

    neighbors
}

/// Which neighbours are due for a liveness probe, recording the probe time
/// for each returned IP.
///
/// Only IPv4 entries currently in `STALE` state are probed: `REACHABLE` is
/// fresh by definition, `PERMANENT` entries are administratively configured,
/// and `DELAY`/`PROBE` mean the kernel is already verifying them. Pure
/// decision over an injected clock so the cadence is unit-testable.
fn due_for_probe(neighbors: &[Neighbor], probes: &mut ProbeLog, now: Instant) -> Vec<IpAddr> {
    let mut due = Vec::new();
    for n in neighbors {
        if !n.ip.is_ipv4() {
            continue;
        }
        if !n.state.split(',').any(|s| s.eq_ignore_ascii_case("STALE")) {
            continue;
        }
        match probes.get(&n.ip) {
            Some(last) if now.duration_since(*last) < PROBE_INTERVAL => continue,
            _ => {}
        }
        probes.insert(n.ip, now);
        due.push(n.ip);
    }
    due
}

/// Nudge each IP with one short ICMP echo in a detached thread. The echo
/// reply itself is irrelevant — the traffic makes the *kernel* verify the
/// neighbour entry (any response, including the ARP reply, marks it
/// `REACHABLE`; silence fails verification and clears the link-layer
/// address). The next poll's `ip neigh` reflects the outcome.
#[cfg(target_os = "linux")]
fn spawn_probes(ips: &[IpAddr]) {
    for &ip in ips {
        thread::Builder::new()
            .name(format!("watchtuier-probe-{ip}"))
            .spawn(move || {
                // `-c 1` a single echo, `-w 1` a one-second hard deadline.
                let _ = Command::new("ping")
                    .args(["-c", "1", "-w", "1"])
                    .arg(ip.to_string())
                    .status();
            })
            .ok();
    }
}

/// Liveness probing relies on `ip neigh` state semantics; the `arp -a`
/// fallback path has no comparable state, so nothing is probed.
#[cfg(not(target_os = "linux"))]
fn spawn_probes(_ips: &[IpAddr]) {}

/// Resolve the hostname for one neighbour IP.
///
/// Unicast reverse DNS wins when it has an answer; devices that only
/// announce themselves over mDNS (e.g. an Avahi host on a subnet with no
/// DNS reverse zone) fall through to [`mdns_reverse_lookup`].
fn resolve_hostname(ip: &IpAddr) -> String {
    resolve_hostname_using(ip, unicast_lookup, mdns_reverse_lookup)
}

/// Strategy seam for [`resolve_hostname`] so both stages can be injected in
/// tests. A unicast answer is used only when it is a real hostname — an
/// error, an empty string, or the numeric address echoed back counts as "no
/// answer" and the mDNS fallback gets a chance.
fn resolve_hostname_using<U, M>(ip: &IpAddr, unicast: U, mdns: M) -> String
where
    U: FnOnce(&IpAddr) -> std::io::Result<String>,
    M: FnOnce(&IpAddr) -> Option<String>,
{
    if let Ok(name) = unicast(ip) {
        if is_real_hostname(&name, ip) {
            return name;
        }
    }
    mdns(ip).unwrap_or_default()
}

/// False for empty answers and for resolvers that merely echo the address.
fn is_real_hostname(name: &str, ip: &IpAddr) -> bool {
    !name.is_empty() && name != ip.to_string()
}

fn unicast_lookup(ip: &IpAddr) -> std::io::Result<String> {
    dns_lookup::lookup_addr(ip)
}

/// Reverse-resolve an IP over multicast DNS by shelling out to
/// `avahi-resolve --address`. Returns `None` when the tool is missing (e.g.
/// Windows, or macOS where the system resolver already covers mDNS), when
/// the daemon cannot answer, or when the output cannot be parsed.
fn mdns_reverse_lookup(ip: &IpAddr) -> Option<String> {
    let output = Command::new("avahi-resolve")
        .arg("--address")
        .arg(ip.to_string())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_avahi_resolve_output(ip, &String::from_utf8_lossy(&output.stdout))
}

/// Parse `avahi-resolve --address` output. Each answer line echoes the
/// queried address followed by the resolved name, separated by whitespace
/// (a tab on every platform observed so far):
///
/// ```text
/// 192.168.228.8\tvrx-ec1d925f.local
/// ```
///
/// Only the line matching the queried address is honoured; anything else
/// (banner text, diagnostics) is ignored.
fn parse_avahi_resolve_output(ip: &IpAddr, stdout: &str) -> Option<String> {
    let wanted = ip.to_string();
    stdout.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let addr = fields.next()?;
        let name = fields.next()?;
        (addr == wanted).then(|| name.to_string())
    })
}

/// Enumerate local IPv4 subnets using the cross-platform `if-addrs` crate.
pub fn list_networks() -> Vec<IpNet> {
    let Ok(ifaces) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for iface in ifaces {
        let IfAddr::V4(v4) = &iface.addr else {
            continue;
        };
        let is_loopback = iface.is_loopback();
        let iface_name = iface.name.clone();
        let ip_octets = v4.ip.octets();
        let prefix = netmask_to_prefix(v4.netmask.octets());
        let net = canonical_network(ip_octets, prefix);
        // Skip link-local 169.254/16 — not useful as a discovery target.
        if net.addr[0] == 169 && net.addr[1] == 254 {
            continue;
        }
        if is_loopback && net.addr[0] != 127 {
            continue;
        }
        let label = format!("{} {}", iface_name, net.display());
        out.push(IpNet {
            network: net,
            iface: iface_name,
            label,
        });
    }
    out.sort_by(|a, b| a.label.cmp(&b.label));
    out
}

fn netmask_to_prefix(mask: [u8; 4]) -> u8 {
    let bits = u32::from_be_bytes(mask);
    bits.count_ones() as u8
}

/// Apply the network mask to produce the canonical network address.
pub fn canonical_network(addr: [u8; 4], prefix: u8) -> Ipv4Network {
    let host_bits = (32 - prefix) as u32;
    let net = u32::from_be_bytes(addr);
    let mask: u32 = if host_bits >= 32 {
        0
    } else {
        !0u32 << host_bits
    };
    let masked = (net & mask).to_be_bytes();
    Ipv4Network {
        addr: masked,
        prefix,
    }
}

/// Enumerate the neighbour table.
#[cfg(target_os = "linux")]
pub fn list_neighbors() -> Vec<Neighbor> {
    let Some(entries) = ip_neigh_json() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for ent in entries {
        let Ok(ip) = ent.dst.parse::<IpAddr>() else {
            continue;
        };
        let Some(mac) = ent.lladdr else {
            continue;
        };
        if mac.is_empty() {
            continue;
        }
        let states = ent.state.unwrap_or_default();
        if states.iter().any(|s| s.eq_ignore_ascii_case("FAILED")) {
            continue;
        }
        let state = states.join(",");
        out.push(Neighbor {
            ip,
            mac,
            hostname: None,
            iface: ent.dev,
            state,
        });
    }
    out.sort_by_key(|n| n.ip);
    out
}

#[derive(Debug, Deserialize)]
#[cfg(target_os = "linux")]
struct NeighEntry {
    dst: String,
    dev: String,
    lladdr: Option<String>,
    state: Option<Vec<String>>,
}

#[cfg(target_os = "linux")]
fn ip_neigh_json() -> Option<Vec<NeighEntry>> {
    let output = Command::new("ip").args(["-j", "neigh"]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

/// Non-Linux fallback: parse `arp -a` text output. Works on Windows (where the
/// MAC uses dashes), macOS, and the BSDs (where the MAC uses colons).
#[cfg(not(target_os = "linux"))]
pub fn list_neighbors() -> Vec<Neighbor> {
    let output = Command::new("arp").arg("-a").output();
    let Ok(out) = output else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut result = Vec::new();
    let mut current_iface_ip: Option<String> = None;

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }

        // Windows `arp -a` groups entries under "Interface: <ip> --- <hex>".
        if let Some(rest) = line.strip_prefix("Interface:") {
            let ip_part = rest.split_whitespace().next().unwrap_or("");
            current_iface_ip = Some(ip_part.to_string());
            continue;
        }
        // Skip column headers on Windows ("Internet Address   Physical ...").
        if line.starts_with("Internet Address") {
            continue;
        }

        // macOS/BSD `arp -a` lines look like:
        //   ? (192.168.1.1) at ab:cd:ef:01:23:45 on en0 ifscope [ethernet]
        // Extract the parenthesised IP and the `at <mac>` segment.
        if let Some(ip) = extract_paren_ip(line) {
            if let Some(mac) = extract_at_mac(line) {
                let iface = extract_on_iface(line)
                    .unwrap_or_else(|| current_iface_ip.clone().unwrap_or_default());
                let state = extract_bsd_state(line).unwrap_or_default();
                push_neighbour(&mut result, ip, &mac, iface, state);
                continue;
            }
        }

        // Windows `arp -a` entries are whitespace-aligned columns:
        //   192.168.1.1           ab-cd-ef-01-23-45     dynamic
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 {
            if let Ok(ip) = parts[0].parse::<IpAddr>() {
                let mac_raw = parts[1];
                let mac = normalise_mac(mac_raw);
                if mac.contains(':') {
                    let iface = current_iface_ip.clone().unwrap_or_default();
                    let state = parts.get(2).map(|s| (*s).to_string()).unwrap_or_default();
                    push_neighbour(&mut result, ip, &mac, iface, state);
                }
            }
        }
    }
    result.sort_by_key(|n| n.ip);
    result
}

#[cfg(not(target_os = "linux"))]
fn push_neighbour(out: &mut Vec<Neighbor>, ip: IpAddr, mac: &str, iface: String, state: String) {
    // Skip broadcast/multicast placeholders commonly emitted by `arp -a`.
    if mac.eq_ignore_ascii_case("ff:ff:ff:ff:ff:ff") {
        return;
    }
    if mac.starts_with("01:00:5e:") {
        return;
    }
    out.push(Neighbor {
        ip,
        mac: mac.to_string(),
        hostname: None,
        iface,
        state,
    });
}

#[cfg(not(target_os = "linux"))]
fn normalise_mac(raw: &str) -> String {
    raw.replace('-', ":").to_lowercase()
}

#[cfg(not(target_os = "linux"))]
fn extract_paren_ip(line: &str) -> Option<IpAddr> {
    let open = line.find('(')?;
    let close = line[open + 1..].find(')')? + open + 1;
    line[open + 1..close].parse().ok()
}

#[cfg(not(target_os = "linux"))]
fn extract_at_mac(line: &str) -> Option<String> {
    let at = line.find(" at ")?;
    let rest = &line[at + 4..];
    let mac = rest.split_whitespace().next()?;
    let normalised = normalise_mac(mac);
    if normalised.contains(':') {
        Some(normalised)
    } else {
        None
    }
}

#[cfg(not(target_os = "linux"))]
fn extract_on_iface(line: &str) -> Option<String> {
    let on = line.find(" on ")?;
    let rest = &line[on + 4..];
    let iface = rest.split_whitespace().next()?;
    Some(iface.to_string())
}

#[cfg(not(target_os = "linux"))]
fn extract_bsd_state(line: &str) -> Option<String> {
    // macOS/BSD arp -a doesn't expose state; we tag permanent entries as
    // "static" if the `[permanent]` marker is present.
    if line.contains("[permanent]") {
        Some("static".into())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn not_found() -> std::io::Result<String> {
        Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no PTR"))
    }

    // ---- liveness probing ----

    fn neigh(ip: &str, state: &str) -> Neighbor {
        Neighbor {
            ip: ip.parse().unwrap(),
            mac: "aa:bb:cc:dd:ee:ff".into(),
            hostname: None,
            iface: "eth0".into(),
            state: state.into(),
        }
    }

    #[test]
    fn only_stale_ipv4_neighbours_are_probed() {
        let neighbors = vec![
            neigh("10.0.0.1", "REACHABLE"),
            neigh("10.0.0.2", "STALE"),
            neigh("10.0.0.3", "PROBE"),     // kernel already verifying
            neigh("10.0.0.4", "PERMANENT"), // administratively configured
            neigh("fe80::1", "STALE"),      // v6: no interface scope to ping
        ];
        let mut probes = ProbeLog::new();
        let due = due_for_probe(&neighbors, &mut probes, Instant::now());
        assert_eq!(due, vec![ip("10.0.0.2")]);
    }

    #[test]
    fn stale_neighbours_reprobed_only_after_probe_interval() {
        let neighbors = vec![neigh("10.0.0.2", "STALE")];
        let mut probes = ProbeLog::new();
        let t0 = Instant::now();

        assert_eq!(
            due_for_probe(&neighbors, &mut probes, t0),
            vec![ip("10.0.0.2")],
            "first sight of a STALE entry is probed immediately"
        );
        assert!(
            due_for_probe(&neighbors, &mut probes, t0 + Duration::from_secs(5)).is_empty(),
            "no probe before the interval elapsed"
        );
        assert_eq!(
            due_for_probe(&neighbors, &mut probes, t0 + PROBE_INTERVAL),
            vec![ip("10.0.0.2")],
            "probed again once the interval elapsed"
        );
    }

    #[test]
    fn fresh_neighbours_skip_probing_entirely() {
        let neighbors = vec![neigh("10.0.0.1", "REACHABLE")];
        let mut probes = ProbeLog::new();
        assert!(due_for_probe(&neighbors, &mut probes, Instant::now()).is_empty());
    }

    // ---- resolve_hostname_using: preference order ----

    #[test]
    fn unicast_name_wins_and_mdns_is_not_consulted() {
        let mdns_called = std::cell::Cell::new(false);
        let name = resolve_hostname_using(
            &ip("192.168.228.8"),
            |_| Ok("printer.lan".into()),
            |_| {
                mdns_called.set(true);
                Some("printer.local".into())
            },
        );
        assert_eq!(name, "printer.lan");
        assert!(
            !mdns_called.get(),
            "mdns fallback must not run when unicast answered"
        );
    }

    #[test]
    fn mdns_fallback_used_when_unicast_fails() {
        let name = resolve_hostname_using(
            &ip("192.168.228.8"),
            |_| not_found(),
            |_| Some("vrx-ec1d925f.local".into()),
        );
        assert_eq!(name, "vrx-ec1d925f.local");
    }

    #[test]
    fn numeric_unicast_echo_falls_through_to_mdns() {
        let name = resolve_hostname_using(
            &ip("192.168.228.8"),
            |ip| Ok(ip.to_string()),
            |_| Some("vrx-ec1d925f.local".into()),
        );
        assert_eq!(name, "vrx-ec1d925f.local");
    }

    #[test]
    fn empty_unicast_answer_falls_through_to_mdns() {
        let name = resolve_hostname_using(
            &ip("192.168.228.8"),
            |_| Ok(String::new()),
            |_| Some("vrx-ec1d925f.local".into()),
        );
        assert_eq!(name, "vrx-ec1d925f.local");
    }

    #[test]
    fn total_failure_yields_empty_string() {
        let name = resolve_hostname_using(&ip("192.168.228.8"), |_| not_found(), |_| None);
        assert_eq!(name, "");
    }

    // ---- parse_avahi_resolve_output ----

    #[test]
    fn parses_tab_separated_avahi_answer() {
        let out =
            parse_avahi_resolve_output(&ip("192.168.228.8"), "192.168.228.8\tvrx-ec1d925f.local\n");
        assert_eq!(out.as_deref(), Some("vrx-ec1d925f.local"));
    }

    #[test]
    fn parses_space_separated_avahi_answer() {
        let out =
            parse_avahi_resolve_output(&ip("192.168.228.8"), "192.168.228.8 vrx-ec1d925f.local\n");
        assert_eq!(out.as_deref(), Some("vrx-ec1d925f.local"));
    }

    #[test]
    fn picks_the_line_matching_the_queried_ip() {
        let out = parse_avahi_resolve_output(
            &ip("192.168.228.8"),
            "192.168.228.7\tother.local\n192.168.228.8\tvrx-ec1d925f.local\n",
        );
        assert_eq!(out.as_deref(), Some("vrx-ec1d925f.local"));
    }

    #[test]
    fn garbage_output_yields_none() {
        assert_eq!(parse_avahi_resolve_output(&ip("192.168.228.8"), ""), None);
        assert_eq!(
            parse_avahi_resolve_output(&ip("192.168.228.8"), "Failed to resolve\n"),
            None,
        );
        assert_eq!(
            parse_avahi_resolve_output(&ip("192.168.228.8"), "192.168.228.8\n"),
            None,
        );
    }

    // ---- mdns_reverse_lookup via PATH-shimmed fake avahi-resolve ----

    /// Serialises tests that mutate process-global state (PATH).
    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Create an executable `avahi-resolve` shim in a temp dir and put it
    /// first on PATH for the duration of `body`. Restores PATH afterwards.
    fn with_avahi_shim(script_body: &str, body: impl FnOnce()) {
        let _guard = ENV_MUTEX.lock().unwrap();
        let dir =
            std::env::temp_dir().join(format!("watchtuier-avahi-shim-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let shim = dir.join("avahi-resolve");
        std::fs::write(&shim, format!("#!/bin/sh\n{script_body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let old_path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{old_path}", dir.display()));
        body();
        std::env::set_var("PATH", &old_path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mdns_reverse_lookup_shells_out_to_avahi_resolve() {
        with_avahi_shim("printf '192.168.228.8\\tvrx-ec1d925f.local\\n'", || {
            assert_eq!(
                mdns_reverse_lookup(&ip("192.168.228.8")).as_deref(),
                Some("vrx-ec1d925f.local"),
            );
        });
    }

    #[test]
    fn mdns_reverse_lookup_none_when_avahi_fails() {
        with_avahi_shim("exit 1", || {
            assert_eq!(mdns_reverse_lookup(&ip("192.168.228.8")), None);
        });
    }

    #[test]
    fn mdns_reverse_lookup_none_when_avahi_absent() {
        // Empty PATH: the tool cannot be found at all.
        let _guard = ENV_MUTEX.lock().unwrap();
        let old = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", "");
        let r = mdns_reverse_lookup(&ip("192.168.228.8"));
        std::env::set_var("PATH", &old);
        assert_eq!(r, None);
    }

    // ---- live network (opt-in) ----

    #[test]
    #[ignore = "hits the live network; run with: cargo test -- --ignored"]
    fn live_avahi_only_device_gets_hostname() {
        // Real zeroconf device on the 192.168.228/24 subnet: unicast reverse
        // DNS cannot resolve it, only mDNS can.
        let name = resolve_hostname(&ip("192.168.228.8"));
        assert_eq!(name, "vrx-ec1d925f.local");
    }
}
