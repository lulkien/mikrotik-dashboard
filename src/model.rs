//! The dashboard's state: what the poller fills in, what the window renders.
//!
//! One `Snapshot` is a whole dashboard frame. The poller owns it, mutates it in
//! place and sends a clone to the UI thread; the UI thread never touches the
//! router, so a slow or dead router cannot stall the window.

use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

/// Samples kept for the traffic chart: one per second, so 60s of history.
pub const TRAFFIC_SAMPLES: usize = 60;
/// Samples kept for the latency sparkline.
pub const PING_SAMPLES: usize = 60;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Link {
    Connecting,
    Live,
    Degraded,
    Down,
}

impl Link {
    pub fn label(self) -> &'static str {
        match self {
            Link::Connecting => "connecting",
            Link::Live => "live",
            Link::Degraded => "degraded",
            Link::Down => "offline",
        }
    }
}

#[derive(Clone, Default, Debug)]
pub struct Iface {
    pub name: String,
    /// `ether`, `vlan`, `bridge`, `pppoe-out`, `loopback` … as the router names it.
    pub kind: String,
    /// RouterOS's own flags for this interface: X disabled, D dynamic, S slave,
    /// R running — the column `/interface print` prints.
    pub flags: String,
    pub addr: String,
    pub running: bool,
    pub rx_mbps: f64,
    pub tx_mbps: f64,
}

#[derive(Clone, Default, Debug)]
pub struct Talker {
    pub name: String,
    pub mac: String,
    /// Cumulative bytes, as Kid Control counts them (since the device was seen).
    pub down_bytes: u64,
    pub up_bytes: u64,
}

#[derive(Clone, Default, Debug)]
pub struct Ping {
    pub min: f64,
    pub avg: f64,
    pub max: f64,
    pub loss_pct: f64,
    pub received: u32,
    pub sent: u32,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub link: Link,
    pub target: String,
    pub status: String,
    pub errors: Vec<String>,

    // System
    pub identity: String,
    pub board: String,
    pub version: String,
    pub arch: String,
    pub uptime: String,
    pub cpu_count: u32,
    pub cpu_freq: u32,
    pub cpu_pct: f64,
    pub mem_used: u64,
    pub mem_total: u64,
    pub disk_used: u64,
    pub disk_total: u64,

    // Traffic
    /// The uplink the router is detected to have; the chart's interface by default.
    pub wan: String,
    pub wan_up: bool,
    /// The interface both traffic cards follow: `--interface`, else the uplink. Its
    /// rates are `rx_mbps`/`tx_mbps` and its curve is the history.
    pub traffic_iface: String,
    pub rx_mbps: f64,
    pub tx_mbps: f64,
    pub rx_peak: f64,
    pub tx_peak: f64,
    pub rx_hist: VecDeque<f64>,
    pub tx_hist: VecDeque<f64>,

    // Links
    pub link_mbps: f64,

    // Connections
    pub conn_total: u32,
    pub conn_tcp: u32,
    pub conn_udp: u32,
    pub conn_icmp: u32,
    pub conn_other: u32,
    pub top_src: Vec<(String, u32)>,
    pub top_dst: Vec<(String, u32)>,

    // Network
    pub ifaces: Vec<Iface>,
    pub lan_lines: Vec<(String, String)>,
    pub lease_bound: u32,
    pub lease_total: u32,
    pub dns_servers: String,
    pub dns_cache_used: u32,
    pub dns_cache_size: u32,

    // Latency
    pub ping_target: String,
    pub ping: Ping,
    pub ping_hist: VecDeque<f64>,

    // Talkers / VPN
    pub talkers: Vec<Talker>,

    /// The virtual interfaces — VLANs, bridges, PPPoE — which get their own card.
    pub virtual_ifaces: Vec<Iface>,

    /// Seconds between traffic samples, so the chart can state its real span.
    pub iface_interval_secs: f64,

    // Bookkeeping for the status line
    pub api_calls: u64,
    pub last_poll_ms: f64,
    pub last_update_unix: u64,
}

impl Default for Snapshot {
    fn default() -> Self {
        Snapshot {
            link: Link::Connecting,
            target: String::new(),
            status: "starting".to_string(),
            errors: Vec::new(),
            identity: "—".to_string(),
            board: String::new(),
            version: String::new(),
            arch: String::new(),
            uptime: "—".to_string(),
            cpu_count: 0,
            cpu_freq: 0,
            cpu_pct: 0.0,
            mem_used: 0,
            mem_total: 0,
            disk_used: 0,
            disk_total: 0,
            wan: "—".to_string(),
            wan_up: false,
            rx_mbps: 0.0,
            tx_mbps: 0.0,
            traffic_iface: String::new(),
            rx_peak: 0.0,
            tx_peak: 0.0,
            rx_hist: VecDeque::new(),
            tx_hist: VecDeque::new(),
            link_mbps: 300.0,
            conn_total: 0,
            conn_tcp: 0,
            conn_udp: 0,
            conn_icmp: 0,
            conn_other: 0,
            top_src: Vec::new(),
            top_dst: Vec::new(),
            ifaces: Vec::new(),
            lan_lines: Vec::new(),
            lease_bound: 0,
            lease_total: 0,
            dns_servers: String::new(),
            dns_cache_used: 0,
            dns_cache_size: 0,
            ping_target: "1.1.1.1".to_string(),
            iface_interval_secs: 5.0,
            virtual_ifaces: Vec::new(),
            ping: Ping::default(),
            ping_hist: VecDeque::new(),
            talkers: Vec::new(),
            api_calls: 0,
            last_poll_ms: 0.0,
            last_update_unix: 0,
        }
    }
}

impl Snapshot {
    pub fn note_error(&mut self, what: &str, err: &str) {
        let line = format!("{what}: {err}");
        if !self.errors.iter().any(|e| e == &line) {
            self.errors.push(line);
        }
        self.errors.truncate(4);
    }

    pub fn clear_error(&mut self, what: &str) {
        self.errors.retain(|e| !e.starts_with(&format!("{what}:")));
    }
}

/// `41.00 Mbps`, `842.0 Kbps`, `0 bps` — the Traffic card's headline format.
pub fn fmt_rate(mbps: f64) -> String {
    if mbps >= 100.0 {
        format!("{mbps:.0} Mbps")
    } else if mbps >= 1.0 {
        format!("{mbps:.2} Mbps")
    } else if mbps >= 0.001 {
        format!("{:.0} Kbps", mbps * 1000.0)
    } else {
        "0 bps".to_string()
    }
}

/// The compact form the tables use: `311.0K`, `7.25M`.
pub fn fmt_rate_short(mbps: f64) -> String {
    if mbps >= 1.0 {
        format!("{mbps:.2}M")
    } else if mbps >= 0.001 {
        format!("{:.1}K", mbps * 1000.0)
    } else {
        "0".to_string()
    }
}

pub fn fmt_bytes(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= 1e9 {
        format!("{:.2} GB", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.1} MB", b / 1e6)
    } else if b >= 1e3 {
        format!("{:.1} KB", b / 1e3)
    } else {
        format!("{bytes} B")
    }
}

/// `2d 3h 41m` from an uptime already formatted by the router, or seconds.
pub fn hms(seconds: u64) -> String {
    let (h, m, s) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
    if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

/// Wall clock in the user's zone (UTC+7), as the header shows it.
pub fn clock_hms() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        + 7 * 3600;
    let s = secs % 86400;
    format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// An SVG path for a filled area under `hist`, in a `w`x`h` viewbox.
pub fn area_path(hist: &VecDeque<f64>, max: f64, w: f32, h: f32) -> String {
    let line = line_path(hist, max, w, h);
    if line.is_empty() {
        return line;
    }
    format!("{line} L {w:.1} {h:.1} L 0.0 {h:.1} Z")
}

/// An SVG path through `hist`, in a `w`x`h` viewbox, zero at the bottom.
pub fn line_path(hist: &VecDeque<f64>, max: f64, w: f32, h: f32) -> String {
    if hist.len() < 2 || max <= 0.0 {
        return String::new();
    }
    let n = hist.len();
    let mut out = String::with_capacity(n * 16);
    for (i, v) in hist.iter().enumerate() {
        let x = i as f32 * w / (n as f32 - 1.0);
        let frac = (v / max).clamp(0.0, 1.0) as f32;
        let y = h - 2.0 - frac * (h - 6.0);
        out.push_str(if i == 0 { "M " } else { " L " });
        out.push_str(&format!("{x:.1} {y:.1}"));
    }
    out
}

/// A 270° gauge arc, from 135° clockwise, in a 100x100 viewbox.
///
/// The start point of the empty gauge is (23.13, 76.87) for r=38 about (50, 50);
/// the same geometry the Slint track uses, so the value lands on the track.
pub fn gauge_arc(frac: f64) -> String {
    let frac = frac.clamp(0.0, 1.0);
    if frac <= 0.001 {
        return String::new();
    }
    let start = 135.0_f64.to_radians();
    let sweep = 270.0 * frac;
    let end = (135.0 + sweep).to_radians();
    let r = 38.0;
    let (x0, y0) = (50.0 + r * start.cos(), 50.0 + r * start.sin());
    let (x1, y1) = (50.0 + r * end.cos(), 50.0 + r * end.sin());
    let large = if sweep > 180.0 { 1 } else { 0 };
    if sweep >= 359.9 {
        // A full sweep has no distinct endpoint: draw both halves.
        return format!(
            "M {x0:.2} {y0:.2} A {r} {r} 0 1 1 50 {:.2} A {r} {r} 0 1 1 {x0:.2} {y0:.2}",
            50.0 + r
        );
    }
    format!("M {x0:.2} {y0:.2} A {r} {r} 0 {large} 1 {x1:.2} {y1:.2}")
}

/// Percentage of a budget, clamped, for a gauge.
pub fn pct(used: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        (used as f64 / total as f64 * 100.0).clamp(0.0, 100.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_read_like_the_traffic_card() {
        assert_eq!(fmt_rate(41.0), "41.00 Mbps");
        assert_eq!(fmt_rate(123.4), "123 Mbps");
        assert_eq!(fmt_rate(0.842), "842 Kbps");
        assert_eq!(fmt_rate(0.0), "0 bps");
    }

    #[test]
    fn a_line_path_starts_at_the_left_edge_and_ends_at_the_right() {
        let mut h = VecDeque::new();
        h.extend([0.0, 10.0, 5.0]);
        let p = line_path(&h, 10.0, 100.0, 50.0);
        assert!(p.starts_with("M 0.0 48.0"), "{p}");
        assert!(p.ends_with("100.0 26.0"), "{p}");
        let area = area_path(&h, 10.0, 100.0, 50.0);
        assert!(area.ends_with("L 100.0 50.0 L 0.0 50.0 Z"), "{area}");
    }

    #[test]
    fn a_gauge_arc_marks_a_quarter_of_the_sweep() {
        let arc = gauge_arc(0.25);
        // 135° + 67.5° = 202.5°: the point is left of centre and above it.
        assert!(arc.contains("A 38 38 0 0 1"), "{arc}");
        assert!(arc.contains("14.89 35.46"), "{arc}");
        assert_eq!(gauge_arc(0.0), "");
    }

    #[test]
    fn percentages_do_not_divide_by_zero() {
        assert_eq!(pct(5, 0), 0.0);
        assert_eq!(pct(512, 1024), 50.0);
        assert_eq!(pct(2048, 1024), 100.0);
    }
}
