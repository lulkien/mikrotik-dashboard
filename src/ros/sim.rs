//! A RouterOS API server that answers like a router, for development.
//!
//! This is a *test double*, not a data source: it speaks the real binary API over
//! a real socket (so `--demo` exercises the same client, framing and poll loop as
//! a live router) but the values it serves are simulated. It exists because the
//! dashboard has to be brought up and shown before a router that will accept a
//! connection is available, and a UI built against invented in-process structs
//! would prove nothing about the client.
//!
//! The state it simulates is an hEX-shaped RouterOS 7.x: a PPPoE WAN with a
//! daytime-ish traffic curve, an ethernet set and a bridge, routed VLANs, DHCP
//! leases, a firewall connection table, Kid Control devices for Top Talkers,
//! WireGuard peers and a `/tool/ping` that answers in single-digit milliseconds.

use std::io::BufReader;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Instant;

use super::protocol::{read_sentence, write_sentence};

/// A row, as it goes on the wire: property name, value.
type Row = Vec<(String, String)>;

#[derive(Clone, Debug)]
pub struct SimConfig {
    pub bind: String,
    pub user: String,
    pub password: String,
    pub identity: String,
    pub board: String,
    pub version: String,
    pub lease_count: usize,
    pub connection_count: usize,
}

impl Default for SimConfig {
    fn default() -> Self {
        SimConfig {
            bind: "127.0.0.1:8728".to_string(),
            user: "mikdash".to_string(),
            password: "demo".to_string(),
            identity: "home-hex".to_string(),
            board: "hEX".to_string(),
            version: "7.24.4 (stable)".to_string(),
            lease_count: 38,
            connection_count: 424,
        }
    }
}

struct State {
    started: Instant,
    last_tick: Instant,
    counters: Vec<(String, u64, u64)>,
}

pub struct SimRouter {
    cfg: SimConfig,
    state: Mutex<State>,
    /// Connections accepted so far, so a caller can prove the client really dialled.
    pub accepted: AtomicU64,
    /// Commands served so far.
    pub served: AtomicU64,
}

/// The hEX this dashboard was developed against, verbatim from `/interface print`:
/// five ethers, the fallback bridge, seven VLANs, loopback, PPPoE.
const IFACE_NAMES: [&str; 15] = [
    "ether1",
    "ether2",
    "ether3",
    "ether4",
    "ether5",
    "0-fallback",
    "10-mgmt",
    "20-home",
    "50-server",
    "90-infra",
    "100-camera",
    "110-iot",
    "150-guest",
    "lo",
    "pppoe-out1",
];

impl SimRouter {
    pub fn new(cfg: SimConfig) -> Self {
        let names = IFACE_NAMES;
        SimRouter {
            cfg,
            state: Mutex::new(State {
                started: Instant::now(),
                last_tick: Instant::now(),
                counters: names
                    .iter()
                    .map(|n| (n.to_string(), 940_000_000_000u64, 210_000_000_000u64))
                    .collect(),
            }),
            accepted: AtomicU64::new(0),
            served: AtomicU64::new(0),
        }
    }

    /// Advance the simulated clock by the time since the last read, so that a
    /// poller computing rates from byte deltas sees plausible rates.
    fn tick(&self) -> (f64, f64) {
        let mut st = self.state.lock().unwrap();
        let now = Instant::now();
        let dt = now.duration_since(st.last_tick).as_secs_f64().max(0.001);
        st.last_tick = now;
        let t = now.duration_since(st.started).as_secs_f64();
        // A sum of incommensurate sines: smooth, never repeating exactly, and
        // shaped like household traffic rather than a sawtooth.
        let wan_rx = 34.0 + 8.0 * (t / 7.3).sin() + 4.0 * (t / 2.1).sin() + 2.5 * (t / 0.9).sin();
        let wan_tx = 12.0 + 3.2 * (t / 5.1).sin() + 2.0 * (t / 1.3).sin() + 1.1 * (t / 0.6).sin();
        let lan_rx = 19.0 + 5.0 * (t / 3.7).sin() + 2.0 * (t / 1.1).sin();
        let lan_tx = 8.0 + 2.6 * (t / 4.3).sin() + 1.4 * (t / 0.8).sin();
        for (name, rx_b, tx_b) in st.counters.iter_mut() {
            let (r, x) = match name.as_str() {
                "pppoe-out1" | "ether1" => (wan_rx.max(0.2), wan_tx.max(0.1)),
                "ether5" | "0-fallback" | "10-mgmt" | "20-home" => {
                    (lan_rx.max(0.1), lan_tx.max(0.1))
                }
                "50-server" | "100-camera" | "110-iot" => {
                    ((lan_rx / 5.0).max(0.05), (lan_tx / 5.0).max(0.05))
                }
                _ => ((lan_rx / 60.0).max(0.01), (lan_tx / 60.0).max(0.01)),
            };
            *rx_b += (r * 1e6 / 8.0 * dt) as u64;
            *tx_b += (x * 1e6 / 8.0 * dt) as u64;
        }
        (wan_rx.max(0.2), wan_tx.max(0.1))
    }

    fn uptime(&self) -> String {
        let secs =
            self.state.lock().unwrap().started.elapsed().as_secs() + 2 * 86400 + 3 * 3600 + 41 * 60;
        let (d, h, m) = (secs / 86400, (secs % 86400) / 3600, (secs % 3600) / 60);
        if d > 0 {
            format!("{d}d{h}h{m}m")
        } else {
            format!("{h}h{m}m")
        }
    }

    fn elapsed(&self) -> f64 {
        self.state.lock().unwrap().started.elapsed().as_secs_f64()
    }

    /// One command: `(rows, trap message)`.
    fn handle(&self, words: &[String]) -> (Vec<Row>, Option<String>) {
        self.served.fetch_add(1, Ordering::Relaxed);
        let command = words.first().map(String::as_str).unwrap_or("");
        let fields: Option<Vec<String>> = words.iter().find_map(|w| {
            w.strip_prefix("=.proplist=")
                .or_else(|| w.strip_prefix(".proplist="))
                .map(|p| p.split(',').map(str::to_string).collect())
        });
        let keep = |key: &str| -> bool {
            match &fields {
                None => true,
                Some(list) => key == ".id" || list.iter().any(|f| f == key),
            }
        };

        let mut rows: Vec<Row> = Vec::new();
        macro_rules! push {
            ($($k:expr => $v:expr),+ $(,)?) => {{
                let row: Row = vec![$(($k.to_string(), $v.to_string())),+]
                    .into_iter()
                    .filter(|(k, _)| keep(k))
                    .collect();
                if !row.is_empty() {
                    rows.push(row);
                }
            }};
        }

        match command {
            "/login" => {}
            "/system/identity/print" => push!(".id" => "*1", "name" => self.cfg.identity.clone()),
            "/system/resource/print" => {
                let t = self.elapsed();
                let cpu = (11.0 + 6.0 * (t / 4.1).sin() + 3.0 * (t / 1.7).sin()).clamp(1.0, 97.0);
                let free_mem = (512.0 + 40.0 * (t / 11.0).sin()).max(80.0) as u64 * 1024 * 1024;
                push!(
                    ".id" => "*1",
                    "uptime" => self.uptime(),
                    "version" => self.cfg.version.clone(),
                    "board-name" => self.cfg.board.clone(),
                    "architecture-name" => "arm64",
                    "cpu" => "ARM64",
                    "cpu-count" => "4",
                    "cpu-frequency" => "880",
                    "cpu-load" => format!("{}", cpu.round() as u32),
                    "free-memory" => free_mem.to_string(),
                    "total-memory" => (1024u64 * 1024 * 1024).to_string(),
                    "free-hdd-space" => (47u64 * 1024 * 1024).to_string(),
                    "total-hdd-space" => (128u64 * 1024 * 1024).to_string(),
                    "platform" => "MikroTik",
                );
            }
            "/interface/print" => {
                self.tick();
                let st = self.state.lock().unwrap();
                // IFACE_NAMES order, because that is the order the router answers in.
                for name in IFACE_NAMES.iter() {
                    let (rx, tx) = st
                        .counters
                        .iter()
                        .find(|(n, _, _)| n == name)
                        .map(|(_, rx, tx)| (*rx, *tx))
                        .unwrap_or((0, 0));
                    let (ty, comment) = match *name {
                        "ether1" => ("ether", "WAN - PPPoE uplink to ISP"),
                        "ether2" | "ether3" | "ether4" => {
                            ("ether", "Fallback LAN port (bridge 0-fallback)")
                        }
                        "ether5" => ("ether", "Trunk to ES208G"),
                        "0-fallback" => ("bridge", "Fallback LAN"),
                        "lo" => ("loopback", "core"),
                        "pppoe-out1" => ("pppoe-out", "carrier uplink"),
                        _ => ("vlan", "VLAN"),
                    };
                    // ether2-4 are bridge slaves with no link, exactly as reported.
                    let slave = matches!(*name, "ether2" | "ether3" | "ether4");
                    push!(
                        ".id" => format!("*{:X}", IFACE_NAMES.iter().position(|n| n == name).unwrap()),
                        "name" => (*name).to_string(),
                        "type" => ty,
                        "disabled" => "false",
                        "dynamic" => "false",
                        "slave" => slave.to_string(),
                        "running" => (!slave).to_string(),
                        "rx-byte" => rx.to_string(),
                        "tx-byte" => tx.to_string(),
                        "comment" => comment,
                    );
                }
            }
            "/ip/address/print" => {
                for (i, (iface, addr)) in [
                    ("10-mgmt", "10.21.10.1/24"),
                    ("20-home", "10.21.20.1/24"),
                    ("50-server", "10.21.50.1/24"),
                    ("100-camera", "10.21.100.1/24"),
                    ("110-iot", "10.21.110.1/24"),
                    ("0-fallback", "192.168.88.1/24"),
                    ("lo", "127.0.0.1/8"),
                    ("pppoe-out1", "198.51.100.31/32"),
                ]
                .iter()
                .enumerate()
                {
                    push!(
                        ".id" => format!("*{:X}", i + 1),
                        "address" => *addr,
                        "network" => addr.split('/').next().unwrap_or(""),
                        "interface" => *iface,
                        "dynamic" => "false",
                        "disabled" => "false",
                        "comment" => *iface,
                    );
                }
            }
            "/ip/dhcp-server/network/print" => {
                for (i, (net, gw)) in [
                    ("10.21.20.0/24", "10.21.20.1"),
                    ("10.21.50.0/24", "10.21.50.1"),
                    ("10.21.150.0/24", "10.21.150.1"),
                    ("192.168.88.0/24", "192.168.88.1"),
                ]
                .iter()
                .enumerate()
                {
                    push!(
                        ".id" => format!("*{:X}", i + 1),
                        "address" => *net,
                        "gateway" => *gw,
                        "dns-server" => "10.21.20.1",
                    );
                }
            }
            "/ip/dhcp-server/lease/print" => {
                for i in 0..self.cfg.lease_count {
                    let seg = if i % 3 == 0 { 20 } else { 50 };
                    let host = [
                        "pixel",
                        "tablet",
                        "espressif",
                        "tplink",
                        "printer",
                        "nvr",
                        "huebridge",
                        "tv",
                    ][i % 8];
                    push!(
                        ".id" => format!("*{:X}", 0x100 + i),
                        "address" => format!("10.21.{seg}.{}", 100 + (i % 120)),
                        "mac-address" => format!("F4:1E:57:{:02X}:{:02X}:{:02X}", i, i * 7 % 251, i * 13 % 251),
                        "host-name" => format!("{host}{i}"),
                        "status" => if i % 9 == 0 { "waiting" } else { "bound" },
                        "dynamic" => "true",
                        "comment" => ["", "Camera", "IoT", "Guest", "Server", ""][i % 6],
                    );
                }
            }
            "/ip/dns/print" => {
                let cache_used = 38 + (self.elapsed() as u64 % 30);
                push!(
                    ".id" => "*1",
                    "servers" => "10.21.20.1,10.21.50.11",
                    "dynamic-servers" => "",
                    "allow-remote-requests" => "true",
                    "cache-size" => "2048",
                    "cache-used" => cache_used.to_string(),
                    "cache-max-ttl" => "1w",
                    "max-concurrent-queries" => "100",
                );
            }
            "/ip/firewall/connection/print" => {
                let jitter = (18.0 * (self.elapsed() / 9.0).sin()) as i64;
                let total = (self.cfg.connection_count as i64 + jitter).clamp(50, 2000) as usize;
                for i in 0..total {
                    let proto = match i % 100 {
                        0..=67 => "tcp",
                        68..=96 => "udp",
                        97..=98 => "icmp",
                        _ => "other",
                    };
                    let src = format!(
                        "10.21.{}.{}",
                        if i % 3 == 0 { 20 } else { 50 },
                        100 + (i % 120)
                    );
                    let dst = match i % 7 {
                        0 => format!("92.123.{}.{}:443", i % 250, 10 + i % 90),
                        1 => format!("18.228.{}.{}:443", i % 200, i % 130),
                        2 => "8.8.8.8:53".to_string(),
                        3 => format!("13.112.{}.{}:443", i % 90, i % 200),
                        4 => format!("185.60.{}.{}:443", i % 200, i % 120),
                        5 => format!("198.51.100.{}:123", 10 + i % 40),
                        _ => format!("52.{}.{}.{}:443", i % 40, i % 200, i % 250),
                    };
                    push!(
                        ".id" => format!("*{:X}", 0x4000 + i),
                        "protocol" => proto,
                        "src-address" => src,
                        "dst-address" => dst,
                        "tcp-state" => "established",
                    );
                }
            }
            "/ip/kid-control/device/print" => {
                let t = self.elapsed();
                for (i, name) in [
                    "kids-tablet",
                    "living-room-tv",
                    "office-pc",
                    "phone-anna",
                    "guest-laptop",
                    "doorbell-cam",
                    "nvr",
                    "hue-bridge",
                    "printer",
                    "washing-machine",
                    "solar-inverter",
                    "work-laptop",
                ]
                .iter()
                .enumerate()
                {
                    let down = (95.0 + 260.0 * (t / (3.0 + i as f64)).sin().abs() + 40.0 * i as f64)
                        as u64;
                    let up = (18.0 + 120.0 * (t / (2.0 + i as f64)).sin().abs()) as u64;
                    push!(
                        ".id" => format!("*{:X}", 0x900 + i),
                        "name" => *name,
                        "mac-address" => format!("02:{:02X}:{:02X}:B0:{:02X}:{:02X}", i * 17, i * 31, i * 7, i * 11),
                        "blocked" => "false",
                        "rate-down" => (down * 1000).to_string(),
                        "rate-up" => (up * 1000).to_string(),
                        // cumulative, as the router reports it
                        "bytes-down" => ((down as f64 + 400.0 * (i as f64 + 1.0)) as u64 * 1_000_000).to_string(),
                        "bytes-up" => ((up as f64 + 90.0 * (i as f64 + 1.0)) as u64 * 1_000_000).to_string(),
                    );
                }
            }
            "/interface/wireguard/peers/print" => {
                let t = self.elapsed();
                for (i, (name, iface)) in [
                    ("luna", "wg-luna"),
                    ("neptunus", "wg-neptunus"),
                    ("sol", "wg-sol"),
                    ("phone", "wg-phone"),
                ]
                .iter()
                .enumerate()
                {
                    let hs = 7.0 + (i as f64 * 13.0 + t / 3.0) % 90.0;
                    push!(
                        ".id" => format!("*{:X}", 0xA00 + i),
                        "interface" => *iface,
                        "comment" => *name,
                        "current-endpoint-address" => format!("198.51.100.{}", 140 + i),
                        "current-endpoint-port" => format!("{}", 41000 + i * 7),
                        "last-handshake" => format!("{}s", hs.round() as u64),
                        "rx" => ((12 + i * 3) * 1024 * 1024).to_string(),
                        "tx" => ((6 + i) * 1024 * 1024).to_string(),
                    );
                }
            }
            "/tool/ping" => {
                let t = self.elapsed();
                let base = 4.2 + 1.8 * (t / 5.0).sin() + 0.9 * (t / 1.3).sin();
                for seq in 1..=5u32 {
                    let rtt = (base + 1.4 * ((t + seq as f64) / 2.0).sin().abs()).max(0.4);
                    push!(
                        ".id" => format!("*{:X}", 0xB00 + seq),
                        "seq" => seq.to_string(),
                        "host" => "1.1.1.1",
                        "size" => "56",
                        "time" => format!("{:.1}ms", rtt),
                    );
                }
                let rtt = base.max(0.4);
                push!(
                    ".id" => "*BFF",
                    "host" => "1.1.1.1",
                    "sent" => "5",
                    "received" => "5",
                    "packet-loss" => "0",
                    "min-rtt" => format!("{:.1}ms", rtt * 0.8),
                    "avg-rtt" => format!("{:.1}ms", rtt),
                    "max-rtt" => format!("{:.1}ms", rtt * 1.7),
                );
            }
            "/log/print" => {
                for (i, level) in ["info", "warning", "error"].iter().enumerate() {
                    push!(
                        ".id" => format!("*{:X}", 0xC00 + i),
                        "time" => format!("{}:{:02}:{:02}", 12 + i, i * 7 % 60, i * 13 % 60),
                        "topics" => "system,info",
                        "message" => format!("simulated {level} line"),
                    );
                }
            }
            other => {
                return (
                    Vec::new(),
                    Some(format!("no such command prefix (line 1 column 1): {other}")),
                );
            }
        }
        (rows, None)
    }
}

fn write_sentences(writer: &mut TcpStream, tag: Option<&str>, rows: &[Row]) -> std::io::Result<()> {
    for row in rows {
        let mut words = vec!["!re".to_string()];
        if let Some(t) = tag {
            words.push(format!("=.tag={t}"));
        }
        words.extend(row.iter().map(|(k, v)| format!("={k}={v}")));
        write_sentence(writer, &words)?;
    }
    Ok(())
}

fn write_end(
    writer: &mut TcpStream,
    kind: &str,
    tag: Option<&str>,
    message: Option<&str>,
) -> std::io::Result<()> {
    let mut words = vec![kind.to_string()];
    if let Some(t) = tag {
        words.push(format!("=.tag={t}"));
    }
    if let Some(m) = message {
        words.push(format!("=message={m}"));
        words.push("=category=0".to_string());
    }
    write_sentence(writer, &words)
}

fn serve_connection(stream: TcpStream, router: Arc<SimRouter>) {
    let _ = stream.set_nodelay(true);
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(_) => return,
    };
    let mut reader = BufReader::new(stream);
    let mut authed = false;
    loop {
        let sentence = match read_sentence(&mut reader) {
            Ok(Some(s)) => s,
            _ => return,
        };
        let command = sentence.first().cloned().unwrap_or_default();
        let tag = sentence
            .iter()
            .find_map(|w| w.strip_prefix("=.tag=").or_else(|| w.strip_prefix(".tag=")))
            .map(str::to_string);

        if command == "/login" {
            let name = sentence
                .iter()
                .find_map(|w| w.strip_prefix("=name="))
                .unwrap_or("");
            let pass = sentence
                .iter()
                .find_map(|w| w.strip_prefix("=password="))
                .unwrap_or("");
            if name == router.cfg.user && pass == router.cfg.password {
                authed = true;
                let _ = write_end(&mut writer, "!done", tag.as_deref(), None);
            } else {
                let _ = write_end(
                    &mut writer,
                    "!trap",
                    tag.as_deref(),
                    Some("invalid user name or password (6)"),
                );
            }
            continue;
        }
        if !authed {
            let _ = write_end(
                &mut writer,
                "!trap",
                tag.as_deref(),
                Some("not logged in (4)"),
            );
            continue;
        }

        let (rows, err) = router.handle(&sentence);
        match err {
            Some(message) => {
                let _ = write_end(&mut writer, "!trap", tag.as_deref(), Some(&message));
            }
            None => {
                if write_sentences(&mut writer, tag.as_deref(), &rows).is_err() {
                    return;
                }
                let _ = write_end(&mut writer, "!done", tag.as_deref(), None);
            }
        }
    }
}

/// Serve on an already-bound listener, in a background thread.
pub fn serve(listener: TcpListener, cfg: SimConfig) -> (Arc<SimRouter>, SocketAddr) {
    let addr = listener.local_addr().expect("listener has a local address");
    let router = Arc::new(SimRouter::new(cfg));
    let accept_router = Arc::clone(&router);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            accept_router.accepted.fetch_add(1, Ordering::Relaxed);
            let router = Arc::clone(&accept_router);
            thread::spawn(move || serve_connection(stream, router));
        }
    });
    (router, addr)
}

/// Bind `cfg.bind` and serve in the background.
pub fn spawn(cfg: SimConfig) -> std::io::Result<(Arc<SimRouter>, SocketAddr)> {
    let listener = TcpListener::bind(&cfg.bind)?;
    let (router, addr) = serve(listener, cfg);
    Ok((router, addr))
}
