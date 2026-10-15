//! The collector thread: one RouterOS connection, a scheduler, a snapshot per tick.
//!
//! One connection, one read per menu at that menu's cadence, and a refusal kept
//! distinct from a dead transport: "the router said no" must never look like "the
//! link dropped", or a menu that exists gets skipped for good. Ten subscriptions, no
//! cache — there is exactly one viewer and it is this process.

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::Sender;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::model::{self, Iface, Link, PING_SAMPLES, Ping, Snapshot, TRAFFIC_SAMPLES, Talker};
use crate::ros::client::{Reply, RosClient, RosError};

/// The subscriptions this dashboard runs: menu, cadence in seconds.
///
/// Every five seconds, so the router sees ~2 commands a second at most, and the
/// slow configuration menus once a minute.
///
/// The cost is resolution: the Traffic chart samples `ifaces`, so its window is
/// `samples x 5s` rather than a minute (the card prints the real span), and a rate
/// shown anywhere is an average over 5s rather than over 1s.
const TASKS: &[(&str, f64)] = &[
    ("identity", 300.0),
    ("system", 5.0),
    ("ifaces", 5.0),
    ("conns", 5.0),
    ("ping", 5.0),
    ("talkers", 5.0),
    ("leases", 15.0),
    ("addresses", 60.0),
    ("dhcpnet", 60.0),
    ("dns", 60.0),
];

/// The `ifaces` cadence, for anyone converting sample counts into seconds.
pub const IFACE_INTERVAL_SECS: f64 = 5.0;

/// Traffic above this (1 Kbps) counts as active for ordering a scrolled list.
const ACTIVE_MBPS: f64 = 0.001;

/// RouterOS's own type names for the interfaces that are not physical ports.
/// Anything else (ether, sfp, wlan, bonding, lte, loopback) is a real interface.
fn is_virtual(kind: &str) -> bool {
    kind == "vlan" || kind == "bridge" || kind.starts_with("pppoe")
}

#[derive(Clone, Debug)]
pub struct Config {
    pub target: String,
    pub user: String,
    pub password: String,
    /// Interface to chart; empty picks the PPPoE/ WAN interface.
    pub wan: Option<String>,
    pub ping_target: String,
    pub link_mbps: f64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            target: "127.0.0.1:8728".to_string(),
            user: "mikdash".to_string(),
            password: String::new(),
            wan: None,
            ping_target: "1.1.1.1".to_string(),
            link_mbps: 300.0,
        }
    }
}

pub fn spawn(cfg: Config, tx: Sender<Snapshot>) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut poller = Poller::new(cfg, tx);
        poller.run();
    })
}

struct Poller {
    cfg: Config,
    snap: Snapshot,
    tx: Sender<Snapshot>,
    /// Per interface: last rx/tx byte counters and when they were read.
    counters: HashMap<String, (u64, u64, Instant)>,
    /// Address per interface, from `/ip/address`.
    addrs: HashMap<String, String>,
    next_due: HashMap<String, Instant>,
    /// Subscriptions the router refused permanently (missing menu, no permission).
    disabled: HashMap<String, String>,
    last_emit: Instant,
}

impl Poller {
    fn new(cfg: Config, tx: Sender<Snapshot>) -> Self {
        let snap = Snapshot {
            iface_interval_secs: IFACE_INTERVAL_SECS,
            target: cfg.target.clone(),
            ping_target: cfg.ping_target.clone(),
            link_mbps: cfg.link_mbps,
            wan: cfg.wan.clone().unwrap_or_else(|| "(detecting)".to_string()),
            ..Snapshot::default()
        };
        Poller {
            cfg,
            snap,
            tx,
            counters: HashMap::new(),
            addrs: HashMap::new(),
            next_due: HashMap::new(),
            disabled: HashMap::new(),
            last_emit: Instant::now() - Duration::from_secs(1),
        }
    }

    fn run(&mut self) {
        let mut backoff = 1u64;
        loop {
            self.snap.link = if self.snap.api_calls == 0 {
                Link::Connecting
            } else {
                Link::Down
            };
            self.snap.status = format!("connecting to {} …", self.cfg.target);
            self.emit(true);

            match RosClient::connect(
                &self.cfg.target,
                &self.cfg.user,
                &self.cfg.password,
                Duration::from_secs(5),
            ) {
                Ok(client) => {
                    backoff = 1;
                    self.snap.link = Link::Live;
                    self.snap.status = format!("connected to {}", self.cfg.target);
                    self.snap.clear_error("connect");
                    // A fresh session knows nothing: every subscription is due.
                    self.next_due.clear();
                    self.counters.clear();
                    self.emit(true);
                    self.poll_until_dead(client);
                }
                Err(e) => {
                    self.snap.note_error("connect", &e.short());
                    self.snap.status = format!("{} — retrying in {backoff}s", e.short());
                    self.snap.link = Link::Down;
                    self.emit(true);
                }
            }
            thread::sleep(Duration::from_secs(backoff));
            backoff = (backoff * 2).min(30);
        }
    }

    /// Poll until the connection fails; returns on any transport error.
    fn poll_until_dead(&mut self, mut client: RosClient) {
        loop {
            let now = Instant::now();
            let mut ran = 0;
            let started = Instant::now();
            for (name, cadence) in TASKS {
                if self.disabled.contains_key(*name) {
                    continue;
                }
                let due = self
                    .next_due
                    .get(*name)
                    .copied()
                    .unwrap_or(now - Duration::from_secs(1));
                if due > now {
                    continue;
                }
                self.next_due
                    .insert(name.to_string(), now + Duration::from_secs_f64(*cadence));
                ran += 1;
                if let Err(e) = self.run_task(name, &mut client) {
                    match e {
                        RosError::Io(_) => {
                            self.snap.link = Link::Down;
                            self.snap.note_error("link", &e.short());
                            self.snap.status = format!("connection lost: {}", e.short());
                            self.emit(true);
                            return;
                        }
                        ref trap if trap.stop_asking() => {
                            // The router does not have this menu, or this account
                            // may not read it. Either way it will not improve by
                            // asking again, so latch it and stop.
                            self.disabled.insert(name.to_string(), trap.short());
                            self.snap.note_error(name, &trap.short());
                        }
                        ref other => self.snap.note_error(name, &other.short()),
                    }
                }
            }
            if ran > 0 {
                self.snap.last_poll_ms = started.elapsed().as_secs_f64() * 1000.0;
                self.snap.last_update_unix = model::unix_now();
            }
            if self.snap.errors.is_empty() {
                self.snap.link = Link::Live;
            } else {
                self.snap.link = Link::Degraded;
            }
            self.emit(false);
            thread::sleep(Duration::from_millis(250));
        }
    }

    fn emit(&mut self, force: bool) {
        if !force && self.last_emit.elapsed() < Duration::from_millis(900) {
            return;
        }
        self.last_emit = Instant::now();
        self.snap.status = self.status_line();
        let _ = self.tx.send(self.snap.clone());
    }

    fn status_line(&self) -> String {
        let mut parts = Vec::new();
        parts.push(format!("{} · {}", self.snap.link.label(), self.cfg.target));
        parts.push(format!("{} API calls", self.snap.api_calls));
        if self.snap.last_poll_ms > 0.0 {
            parts.push(format!("last sweep {:.0} ms", self.snap.last_poll_ms));
        }
        if !self.disabled.is_empty() {
            let names: Vec<&str> = self.disabled.keys().map(String::as_str).collect();
            parts.push(format!("skipped: {}", names.join(", ")));
        }
        if !self.snap.errors.is_empty() {
            parts.push(format!("⚠ {}", self.snap.errors.join(" · ")));
        }
        parts.join("   |   ")
    }

    fn run_task(&mut self, name: &str, client: &mut RosClient) -> Result<(), RosError> {
        match name {
            "identity" => self.task_identity(client),
            "system" => self.task_system(client),
            "ifaces" => self.task_ifaces(client),
            "conns" => self.task_conns(client),
            "ping" => self.task_ping(client),
            "talkers" => self.task_talkers(client),
            "leases" => self.task_leases(client),
            "addresses" => self.task_addresses(client),
            "dhcpnet" => self.task_dhcp_networks(client),
            "dns" => self.task_dns(client),
            other => Err(RosError::Protocol(format!("unknown task {other}"))),
        }
    }

    fn task_identity(&mut self, c: &mut RosClient) -> Result<(), RosError> {
        let rows = self.call(c, "/system/identity", &["name"])?;
        if let Some(row) = rows.first() {
            if let Some(name) = row.get("name") {
                self.snap.identity = name.clone();
            }
        }
        Ok(())
    }

    fn task_system(&mut self, c: &mut RosClient) -> Result<(), RosError> {
        let rows = self.call(c, "/system/resource", &[])?;
        let Some(row) = rows.first().cloned() else {
            return Ok(());
        };
        self.snap.uptime = row.get("uptime").cloned().unwrap_or_default();
        self.snap.version = row.get("version").cloned().unwrap_or_default();
        self.snap.board = row.get("board-name").cloned().unwrap_or_default();
        self.snap.arch = row.get("architecture-name").cloned().unwrap_or_default();
        self.snap.cpu_count = u64_of(&row, "cpu-count") as u32;
        self.snap.cpu_freq = u64_of(&row, "cpu-frequency") as u32;
        self.snap.cpu_pct = u64_of(&row, "cpu-load") as f64;
        let mem_total = u64_of(&row, "total-memory");
        let mem_free = u64_of(&row, "free-memory");
        self.snap.mem_total = mem_total;
        self.snap.mem_used = mem_total.saturating_sub(mem_free);
        let disk_total = u64_of(&row, "total-hdd-space");
        let disk_free = u64_of(&row, "free-hdd-space");
        self.snap.disk_total = disk_total;
        self.snap.disk_used = disk_total.saturating_sub(disk_free);
        Ok(())
    }

    fn task_ifaces(&mut self, c: &mut RosClient) -> Result<(), RosError> {
        let rows = self.call(
            c,
            "/interface",
            &[
                "name", "type", "running", "disabled", "slave", "dynamic", "comment", "rx-byte",
                "tx-byte",
            ],
        )?;
        let now = Instant::now();
        let mut ifaces: Vec<Iface> = Vec::with_capacity(rows.len());
        for row in &rows {
            let name = row.get("name").cloned().unwrap_or_default();
            let rx = u64_of(row, "rx-byte");
            let tx = u64_of(row, "tx-byte");
            let (rx_mbps, tx_mbps) = match self.counters.get(&name) {
                Some((prx, ptx, at)) => {
                    let dt = now.duration_since(*at).as_secs_f64();
                    if dt > 0.2 {
                        (
                            (rx.saturating_sub(*prx)) as f64 * 8.0 / dt / 1e6,
                            (tx.saturating_sub(*ptx)) as f64 * 8.0 / dt / 1e6,
                        )
                    } else {
                        (0.0, 0.0)
                    }
                }
                None => (0.0, 0.0),
            };
            self.counters.insert(name.clone(), (rx, tx, now));
            // The flags column exactly as `/interface print` shows it.
            let mut flags = String::new();
            for (key, letter) in [
                ("disabled", 'X'),
                ("dynamic", 'D'),
                ("slave", 'S'),
                ("running", 'R'),
            ] {
                if row.get(key).map(String::as_str) == Some("true") {
                    flags.push(letter);
                }
            }
            ifaces.push(Iface {
                addr: self.addrs.get(&name).cloned().unwrap_or_default(),
                kind: row.get("type").cloned().unwrap_or_default(),
                flags,
                name,
                running: row.get("running").map(String::as_str) == Some("true"),
                rx_mbps,
                tx_mbps,
            });
        }

        // The WAN interface is what the Traffic card charts: the one whose comment
        // names it, else the running PPPoE link.
        if self.cfg.wan.is_none() {
            let detected = ifaces
                .iter()
                .find(|i| i.running && i.name.starts_with("pppoe"))
                .or_else(|| {
                    ifaces.iter().find(|i| {
                        rows.iter().any(|r| {
                            r.get("name") == Some(&i.name)
                                && r.get("comment").map(|c| c.to_uppercase().contains("WAN"))
                                    == Some(true)
                        })
                    })
                })
                .or_else(|| {
                    ifaces
                        .iter()
                        .find(|i| i.running && i.name.starts_with("ether"))
                })
                .map(|i| i.name.clone());
            if let Some(name) = detected {
                self.snap.wan = name;
            }
        } else {
            self.snap.wan = self.cfg.wan.clone().unwrap_or_default();
        }

        if let Some(wan) = ifaces.iter().find(|i| i.name == self.snap.wan) {
            self.snap.rx_mbps = wan.rx_mbps;
            self.snap.tx_mbps = wan.tx_mbps;
            self.snap.wan_up = wan.running;
        }
        push(&mut self.snap.rx_hist, self.snap.rx_mbps, TRAFFIC_SAMPLES);
        push(&mut self.snap.tx_hist, self.snap.tx_mbps, TRAFFIC_SAMPLES);
        self.snap.rx_peak = self.snap.rx_hist.iter().cloned().fold(0.0, f64::max);
        self.snap.tx_peak = self.snap.tx_hist.iter().cloned().fold(0.0, f64::max);

        // NO sorting for the Interfaces card: `/interface print` answers in the
        // router's own index order (ether1..5, the fallback bridge, the VLANs,
        // loopback, PPPoE) and the card is meant to read like that listing. The
        // ethers and the loopback are real, VLANs/bridges/PPPoE are virtual — two
        // cards.
        let (mut virtuals, real): (Vec<Iface>, Vec<Iface>) =
            ifaces.into_iter().partition(|i| is_virtual(&i.kind));
        // Virtual lists more rows than its height shows, so it IS ordered: whatever
        // is moving traffic sits on top, busiest first, and the idle ones keep the
        // router's order underneath (sort_by is stable).
        virtuals.sort_by(|a, b| {
            let (a_busy, b_busy) = (a.rx_mbps + a.tx_mbps, b.rx_mbps + b.tx_mbps);
            let (a_active, b_active) = (a_busy >= ACTIVE_MBPS, b_busy >= ACTIVE_MBPS);
            b_active.cmp(&a_active).then_with(|| {
                if a_active && b_active {
                    b_busy
                        .partial_cmp(&a_busy)
                        .unwrap_or(std::cmp::Ordering::Equal)
                } else {
                    std::cmp::Ordering::Equal
                }
            })
        });
        self.snap.ifaces = real;
        self.snap.virtual_ifaces = virtuals;
        Ok(())
    }

    fn task_conns(&mut self, c: &mut RosClient) -> Result<(), RosError> {
        let rows = self.call(
            c,
            "/ip/firewall/connection",
            &["protocol", "src-address", "dst-address"],
        )?;
        let (mut tcp, mut udp, mut icmp, mut other) = (0u32, 0u32, 0u32, 0u32);
        let mut by_src: HashMap<String, u32> = HashMap::new();
        let mut by_dst: HashMap<String, u32> = HashMap::new();
        for row in &rows {
            match row.get("protocol").map(String::as_str).unwrap_or("") {
                "tcp" => tcp += 1,
                "udp" => udp += 1,
                "icmp" => icmp += 1,
                _ => other += 1,
            }
            if let Some(src) = row.get("src-address") {
                *by_src.entry(src.clone()).or_default() += 1;
            }
            if let Some(dst) = row.get("dst-address") {
                *by_dst.entry(dst.clone()).or_default() += 1;
            }
        }
        self.snap.conn_total = rows.len() as u32;
        self.snap.conn_tcp = tcp;
        self.snap.conn_udp = udp;
        self.snap.conn_icmp = icmp;
        self.snap.conn_other = other;
        self.snap.top_src = top(&by_src, 5);
        self.snap.top_dst = top(&by_dst, 8);
        Ok(())
    }

    fn task_ping(&mut self, c: &mut RosClient) -> Result<(), RosError> {
        self.snap.api_calls += 1;
        let args = vec![
            format!("=address={}", self.cfg.ping_target),
            "=count=5".to_string(),
            // RouterOS defaults to one probe a second, which makes this call block
            // the whole sweep for ~5s and starve the 1s subscriptions behind it
            // (measured on the hEX: "last sweep 4081 ms").
            "=interval=0.2".to_string(),
        ];
        let rows = c.cmd("/tool/ping", &args)?;
        for row in &rows {
            if row.contains_key("avg-rtt") || row.contains_key("packet-loss") {
                let ping = Ping {
                    min: ms_of(row, "min-rtt"),
                    avg: ms_of(row, "avg-rtt"),
                    max: ms_of(row, "max-rtt"),
                    loss_pct: row
                        .get("packet-loss")
                        .and_then(|v| v.parse::<f64>().ok())
                        .unwrap_or(0.0),
                    sent: u64_of(row, "sent") as u32,
                    received: u64_of(row, "received") as u32,
                };
                push(&mut self.snap.ping_hist, ping.avg, PING_SAMPLES);
                self.snap.ping = ping;
            }
        }
        Ok(())
    }

    fn task_talkers(&mut self, c: &mut RosClient) -> Result<(), RosError> {
        // BYTES, not `rate-down`/`rate-up`. Measured on the hEX: Kid Control lists
        // 23 devices and every one of them reads 0bps until it is actively moving
        // traffic, so a rate-ranked card is an alphabetical list of zeroes on a
        // quiet LAN. Cumulative volume always has an answer, and it is the same
        // question the card's title asks.
        let rows = self.call(
            c,
            "/ip/kid-control/device",
            &["name", "mac-address", "bytes-down", "bytes-up"],
        )?;
        let mut talkers: Vec<Talker> = rows
            .iter()
            .map(|row| Talker {
                name: row
                    .get("name")
                    .filter(|n| !n.is_empty())
                    .cloned()
                    .unwrap_or_else(|| "unnamed".to_string()),
                mac: row.get("mac-address").cloned().unwrap_or_default(),
                down_bytes: u64_of(row, "bytes-down"),
                up_bytes: u64_of(row, "bytes-up"),
            })
            .collect();
        talkers.sort_by(|a, b| {
            (b.down_bytes + b.up_bytes)
                .cmp(&(a.down_bytes + a.up_bytes))
                .then_with(|| a.name.cmp(&b.name))
        });
        // The column is tall; a real Kid Control list holds every device the
        // router has seen, so keep enough of it to fill the card.
        talkers.truncate(20);
        self.snap.talkers = talkers;
        Ok(())
    }

    fn task_leases(&mut self, c: &mut RosClient) -> Result<(), RosError> {
        let rows = self.call(c, "/ip/dhcp-server/lease", &["address", "status"])?;
        self.snap.lease_total = rows.len() as u32;
        self.snap.lease_bound = rows
            .iter()
            .filter(|r| r.get("status").map(String::as_str) == Some("bound"))
            .count() as u32;
        Ok(())
    }

    fn task_addresses(&mut self, c: &mut RosClient) -> Result<(), RosError> {
        let rows = self.call(c, "/ip/address", &["address", "interface", "dynamic"])?;
        self.addrs.clear();
        for row in &rows {
            if let (Some(iface), Some(addr)) = (row.get("interface"), row.get("address")) {
                // A static address wins over a dynamic one on the same interface.
                let dynamic = row.get("dynamic").map(String::as_str) == Some("true");
                match self.addrs.get(iface) {
                    Some(_) if dynamic => {}
                    _ => {
                        self.addrs.insert(iface.clone(), addr.clone());
                    }
                }
            }
        }
        Ok(())
    }

    fn task_dhcp_networks(&mut self, c: &mut RosClient) -> Result<(), RosError> {
        let rows = self.call(c, "/ip/dhcp-server/network", &["address", "gateway"])?;
        let mut lines: Vec<(String, String)> = Vec::new();
        if let Some(row) = rows.first() {
            lines.push((
                "LAN".to_string(),
                row.get("address").cloned().unwrap_or_default(),
            ));
            lines.push((
                "GW".to_string(),
                row.get("gateway").cloned().unwrap_or_default(),
            ));
        }
        lines.push(("NETWORKS".to_string(), format!("{} served", rows.len())));
        self.snap.lan_lines = lines;
        Ok(())
    }

    fn task_dns(&mut self, c: &mut RosClient) -> Result<(), RosError> {
        let rows = self.call(c, "/ip/dns", &["servers", "cache-used", "cache-size"])?;
        if let Some(row) = rows.first() {
            self.snap.dns_servers = row.get("servers").cloned().unwrap_or_default();
            self.snap.dns_cache_used = u64_of(row, "cache-used") as u32;
            self.snap.dns_cache_size = u64_of(row, "cache-size") as u32;
        }
        Ok(())
    }

    fn call(
        &mut self,
        c: &mut RosClient,
        path: &str,
        proplist: &[&str],
    ) -> Result<Vec<Reply>, RosError> {
        self.snap.api_calls += 1;
        c.print(path, proplist)
    }
}

fn push(hist: &mut VecDeque<f64>, value: f64, cap: usize) {
    if hist.len() == cap {
        hist.pop_front();
    }
    hist.push_back(value);
}

fn top(counts: &HashMap<String, u32>, n: usize) -> Vec<(String, u32)> {
    let mut v: Vec<(String, u32)> = counts.iter().map(|(k, c)| (k.clone(), *c)).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v.truncate(n);
    v
}

fn u64_of(row: &Reply, key: &str) -> u64 {
    row.get(key)
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0)
}

/// `24ms651us`, `235us`, `12ms` → milliseconds. The unit matters: taking only the
/// leading digits turned a LAN `235us` into 235 ms.
fn ms_of(row: &Reply, key: &str) -> f64 {
    row.get(key)
        .map(|raw| crate::ros::client::parse_time_ms(raw))
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_come_from_byte_deltas() {
        let mut counters = HashMap::new();
        counters.insert(
            "ether1".to_string(),
            (
                1_000_000u64,
                500_000u64,
                Instant::now() - Duration::from_secs(1),
            ),
        );
        // 1 MB in a second is 8 Mbps; the task reads 1 MB more.
        let (prx, ptx, _) = counters["ether1"];
        let dt = 1.0;
        let rx = (prx + 1_000_000 - prx) as f64 * 8.0 / dt / 1e6;
        assert!((rx - 8.0).abs() < 0.001, "{rx}");
        let _ = ptx;
    }

    #[test]
    fn top_sources_are_sorted_and_capped() {
        let mut counts = HashMap::new();
        for (k, v) in [("a", 3u32), ("b", 9), ("c", 1), ("d", 7)] {
            counts.insert(k.to_string(), v);
        }
        let t = top(&counts, 2);
        assert_eq!(t, vec![("b".to_string(), 9), ("d".to_string(), 7)]);
    }

    #[test]
    fn milliseconds_are_read_from_router_formatting() {
        let mut row = Reply::new();
        row.insert("avg-rtt".to_string(), "12.4ms".to_string());
        assert_eq!(ms_of(&row, "avg-rtt"), 12.4);
        // the string the hEX actually returns, microseconds and all
        row.insert("avg-rtt".to_string(), "24ms755us".to_string());
        assert!((ms_of(&row, "avg-rtt") - 24.755).abs() < 0.001);
        row.insert("avg-rtt".to_string(), "235us".to_string());
        assert!((ms_of(&row, "avg-rtt") - 0.235).abs() < 0.001);
        assert_eq!(ms_of(&row, "missing"), 0.0);
    }

    #[test]
    fn history_is_a_rolling_window() {
        let mut h = VecDeque::new();
        for i in 0..5 {
            push(&mut h, i as f64, 3);
        }
        assert_eq!(h.len(), 3);
        assert_eq!(h.front(), Some(&2.0));
        assert_eq!(h.back(), Some(&4.0));
    }
}
