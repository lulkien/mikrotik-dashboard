//! Mikrotik Dashboard: a native Slint dashboard for MikroTik RouterOS on a fixed
//! 1920x1080 surface.
//!
//! ```text
//! mikrotik-dashboard --demo                       # simulated router, local port
//! mikrotik-dashboard --target 10.0.0.1:8728 --user mikdash --password-env MIKDASH_PASSWORD
//! mikrotik-dashboard --probe --demo --seconds 6   # headless: poll and print
//! ```
//!
//! The window is a fixed 1920x1080 surface. Nothing in the UI thread talks to the
//! router: the collector thread sends whole snapshots down a channel and a timer
//! applies the newest one, so a stalled API connection shows as a stale-but-alive
//! window rather than a frozen one.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use slint::{ModelRc, VecModel};

use mikrotik_dashboard::model::{self, Iface, Snapshot, fmt_bytes, fmt_rate, fmt_rate_short};
use mikrotik_dashboard::poll::{self, Config};
use mikrotik_dashboard::ros::sim::{self, SimConfig};

mod shot;

slint::include_modules!();

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opts = match Options::parse(&args) {
        Ok(Some(opts)) => opts,
        Ok(None) => {
            print_usage();
            return Ok(());
        }
        Err(e) => {
            eprintln!("mikrotik-dashboard: {e}");
            std::process::exit(2);
        }
    };

    // --demo binds a simulated RouterOS on an ephemeral port and points the
    // collector at it: the same client, framing and poll loop as a real router.
    let mut cfg = Config {
        target: opts.target.clone(),
        user: opts.user.clone(),
        password: opts.password.clone(),
        wan: opts.wan.clone(),
        ping_target: opts.ping.clone(),
        link_mbps: opts.link_mbps,
    };
    if opts.demo {
        let (router, addr) = sim::spawn(SimConfig {
            bind: "127.0.0.1:0".to_string(),
            user: opts.user.clone(),
            password: opts.password.clone(),
            ..SimConfig::default()
        })?;
        cfg.target = addr.to_string();
        eprintln!(
            "mikrotik-dashboard: demo — simulated RouterOS on {addr}, {} sockets accepted",
            router.accepted.load(std::sync::atomic::Ordering::Relaxed)
        );
        // The accept thread lives as long as the process: leak the handle rather
        // than tear the listener down when the local goes out of scope.
        std::mem::forget(router);
    }

    let (tx, rx) = std::sync::mpsc::channel::<Snapshot>();
    poll::spawn(cfg, tx);

    if opts.probe {
        return probe(rx, opts.seconds);
    }

    // --screenshot renders offscreen and exits: no display server, and the layout
    // is drawn at the size it is written for. A `{}` in the path writes a numbered
    // series instead (one frame every 2 s), which is how a layout that shifts with
    // its own data gets caught.
    if let Some(path) = opts.screenshot.clone() {
        let window = shot::install()?;
        let ui = DashboardWindow::new()?;
        ui.show()?;
        let series = path.contains("{}");
        let deadline = Instant::now() + Duration::from_secs(opts.seconds);
        let mut last = Snapshot::default();
        let mut written = 0u32;
        let mut next_at = Instant::now();
        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let wait = if series {
                Duration::from_millis(200).min(deadline - now)
            } else {
                Duration::from_millis(500).min(deadline - now)
            };
            match rx.recv_timeout(wait) {
                Ok(snap) => last = snap,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if series && Instant::now() >= next_at {
                apply(&ui, &last);
                let frame = path.replacen("{}", &format!("{written:03}"), 1);
                shot::capture(&window, &frame)?;
                written += 1;
                next_at = Instant::now() + Duration::from_secs(2);
            }
        }
        if series {
            println!(
                "wrote {written} frames to {path} ({}x{}, every 2 s)",
                shot::WIDTH,
                shot::HEIGHT
            );
        } else {
            apply(&ui, &last);
            shot::capture(&window, &path)?;
            println!(
                "wrote {path} ({}x{}, software renderer) — convert with: magick {path} {}.png",
                shot::WIDTH,
                shot::HEIGHT,
                path.trim_end_matches(".ppm")
            );
        }
        return Ok(());
    }

    let ui = DashboardWindow::new()?;
    apply(&ui, &Snapshot::default());

    // 250 ms: the collector emits at most one snapshot a second, and the UI must
    // still redraw its clock between them.
    let timer = slint::Timer::default();
    let weak = ui.as_weak();
    let mut last: Option<Snapshot> = None;
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(250),
        move || {
            let Some(ui) = weak.upgrade() else { return };
            while let Ok(snap) = rx.try_recv() {
                last = Some(snap);
            }
            if let Some(view) = last.clone() {
                apply(&ui, &view);
            }
        },
    );

    ui.run()?;
    Ok(())
}

fn probe(rx: Receiver<Snapshot>, seconds: u64) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut frames = 0u32;
    let mut last = Snapshot::default();
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(snap) => {
                frames += 1;
                last = snap;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    print!("{}", frame_summary(&last, frames));
    Ok(())
}

fn frame_summary(s: &Snapshot, frames: u32) -> String {
    let mut out = String::new();
    out.push_str(&format!("snapshots    {frames}\n"));
    out.push_str(&format!("link         {}\n", s.link.label()));
    out.push_str(&format!("status       {}\n", s.status));
    out.push_str(&format!(
        "router       {} · {} · RouterOS {} · {} · up {}\n",
        s.identity, s.board, s.version, s.arch, s.uptime
    ));
    let clock = if s.cpu_freq > 0 {
        format!(" @ {} MHz", s.cpu_freq)
    } else {
        String::new()
    };
    out.push_str(&format!(
        "system       cpu {:.0}% · ram {} of {} · storage {} of {} · {} cpu{}\n",
        s.cpu_pct,
        fmt_bytes(s.mem_used),
        fmt_bytes(s.mem_total),
        fmt_bytes(s.disk_used),
        fmt_bytes(s.disk_total),
        s.cpu_count,
        clock
    ));
    out.push_str(&format!(
        "traffic      {} rx {} · tx {} · peak rx {} · {:.0}s of samples\n",
        s.wan,
        fmt_rate(s.rx_mbps),
        fmt_rate(s.tx_mbps),
        fmt_rate(s.rx_peak),
        s.rx_hist.len() as f64 * s.iface_interval_secs
    ));
    out.push_str(&format!(
        "connections  {} total · tcp {} · udp {} · icmp {} · other {}\n",
        s.conn_total, s.conn_tcp, s.conn_udp, s.conn_icmp, s.conn_other
    ));
    for (addr, n) in s.top_src.iter().take(2) {
        out.push_str(&format!("  src {addr} ×{n}\n"));
    }
    for (addr, n) in s.top_dst.iter().take(3) {
        out.push_str(&format!("  dst {addr} ×{n}\n"));
    }
    out.push_str(&format!(
        "network      {} real + {} virtual interfaces\n",
        s.ifaces.len(),
        s.virtual_ifaces.len()
    ));
    // Same columns as the Interfaces card, and all of them: the count above is only
    // meaningful if the lines that follow account for every one.
    for i in s.ifaces.iter().chain(s.virtual_ifaces.iter()) {
        out.push_str(&format!(
            "  iface {:<3} {:<11} {:<9} {:<16} rx {} tx {}\n",
            i.flags,
            i.name,
            i.kind,
            if i.addr.is_empty() { "-" } else { &i.addr },
            fmt_rate(i.rx_mbps),
            fmt_rate(i.tx_mbps)
        ));
    }
    for (k, v) in s.lan_lines.iter() {
        out.push_str(&format!("  {k} {v}\n"));
    }
    out.push_str(&format!(
        "leases       {} of {} bound · dns {} · cache {}/{}\n",
        s.lease_bound, s.lease_total, s.dns_servers, s.dns_cache_used, s.dns_cache_size
    ));
    out.push_str(&format!(
        "latency      {} now {:.1} ms · min {:.1} · avg {:.1} · max {:.1} · loss {:.0}% · {} samples\n",
        s.ping_target,
        s.ping.avg,
        s.ping.min,
        s.ping.avg,
        s.ping.max,
        s.ping.loss_pct,
        s.ping_hist.len()
    ));
    for t in s.talkers.iter().take(3) {
        out.push_str(&format!(
            "  talker {} {} down {} up {}\n",
            t.name,
            t.mac,
            fmt_bytes(t.down_bytes),
            fmt_bytes(t.up_bytes)
        ));
    }
    out.push_str(&format!("api calls    {}\n", s.api_calls));
    if !s.errors.is_empty() {
        out.push_str(&format!("errors       {}\n", s.errors.join(" · ")));
    }
    out
}

/// Push one snapshot into every `in` property the window declares.
fn apply(ui: &DashboardWindow, s: &Snapshot) {
    ui.set_clock(model::clock_hms().into());
    ui.set_identity(s.identity.clone().into());
    ui.set_router_addr(s.target.clone().into());
    ui.set_board(s.board.clone().into());
    ui.set_uptime(s.uptime.clone().into());
    ui.set_ros_version(s.version.clone().into());
    ui.set_link_state(s.link.label().into());
    ui.set_link_tint(match s.link {
        model::Link::Live => slint::Color::from_rgb_u8(52, 211, 153),
        model::Link::Connecting | model::Link::Degraded => slint::Color::from_rgb_u8(251, 191, 36),
        model::Link::Down => slint::Color::from_rgb_u8(248, 113, 113),
    });

    // Traffic: the chart's ceiling is the peak in the window plus headroom, so a
    // quiet minute does not flatten a busy one into the axis.
    ui.set_wan_name(s.wan.clone().into());
    ui.set_rx_now(fmt_rate(s.rx_mbps).into());
    ui.set_tx_now(fmt_rate(s.tx_mbps).into());
    ui.set_rx_peak(fmt_rate(s.rx_peak).into());
    ui.set_tx_peak(fmt_rate(s.tx_peak).into());
    // The chart's real span: samples x the polling interval, not the sample count.
    let span = s.rx_hist.len() as f64 * s.iface_interval_secs;
    ui.set_traffic_window(
        if span >= 120.0 {
            format!("{:.0}m window", span / 60.0)
        } else {
            format!("{:.0}s window", span)
        }
        .into(),
    );
    let ceiling = (s.rx_peak.max(s.tx_peak) * 1.15).max(0.5);
    ui.set_rx_area(model::area_path(&s.rx_hist, ceiling, 1000.0, 240.0).into());
    ui.set_rx_line(model::line_path(&s.rx_hist, ceiling, 1000.0, 240.0).into());
    ui.set_tx_area(model::area_path(&s.tx_hist, ceiling, 1000.0, 240.0).into());
    ui.set_tx_line(model::line_path(&s.tx_hist, ceiling, 1000.0, 240.0).into());

    ui.set_dl_pct(model::pct((s.rx_mbps * 1000.0) as u64, (s.link_mbps * 1000.0) as u64) as f32);
    ui.set_ul_pct(model::pct((s.tx_mbps * 1000.0) as u64, (s.link_mbps * 1000.0) as u64) as f32);
    ui.set_dl_text(fmt_rate(s.rx_mbps).into());
    ui.set_ul_text(fmt_rate(s.tx_mbps).into());

    ui.set_conn_total(s.conn_total as i32);
    ui.set_conn_tcp(s.conn_tcp.max(1) as f32);
    ui.set_conn_udp(s.conn_udp.max(1) as f32);
    ui.set_conn_icmp(s.conn_icmp.max(1) as f32);
    ui.set_conn_other(s.conn_other.max(1) as f32);
    ui.set_conn_tcp_text(s.conn_tcp.to_string().into());
    ui.set_conn_udp_text(s.conn_udp.to_string().into());
    ui.set_conn_icmp_text(s.conn_icmp.to_string().into());
    ui.set_conn_other_text(s.conn_other.to_string().into());
    ui.set_top_src(model_of(
        s.top_src
            .iter()
            .map(|(a, n)| row(a, &n.to_string(), "", ""))
            .collect(),
    ));
    ui.set_top_dst(model_of(
        s.top_dst
            .iter()
            .map(|(a, n)| row(a, &n.to_string(), "", ""))
            .collect(),
    ));

    ui.set_cpu_arc(model::gauge_arc(s.cpu_pct / 100.0).into());
    ui.set_ram_arc(model::gauge_arc(model::pct(s.mem_used, s.mem_total) / 100.0).into());
    ui.set_disk_arc(model::gauge_arc(model::pct(s.disk_used, s.disk_total) / 100.0).into());
    ui.set_cpu_text(format!("{:.0}%", s.cpu_pct).into());
    ui.set_ram_text(format!("{:.0}%", model::pct(s.mem_used, s.mem_total)).into());
    ui.set_disk_text(format!("{:.0}%", model::pct(s.disk_used, s.disk_total)).into());
    // The hEX reports no cpu-frequency at all (it printed "0 MHz" against the real
    // router), so say only what the router actually gave us.
    ui.set_sys_line1(
        if s.cpu_freq > 0 {
            format!(
                "{} · {} {} · {} cpu @ {} MHz",
                s.identity, s.board, s.arch, s.cpu_count, s.cpu_freq
            )
        } else {
            format!(
                "{} · {} {} · {} cpu",
                s.identity, s.board, s.arch, s.cpu_count
            )
        }
        .into(),
    );
    ui.set_sys_line2(
        format!(
            "RAM {} of {} · RouterOS {}",
            fmt_bytes(s.mem_used),
            fmt_bytes(s.mem_total),
            s.version
        )
        .into(),
    );
    ui.set_sys_line3(
        format!(
            "Storage {} of {} · DNS cache {}/{}",
            fmt_bytes(s.disk_used),
            fmt_bytes(s.disk_total),
            s.dns_cache_used,
            s.dns_cache_size
        )
        .into(),
    );

    // One row per interface, in the router's own order, with its flags and type:
    // the cards are meant to read like `/interface print`. The R/X flags carry what
    // the address column used to say ("running" / "no link").
    let ifaces: Vec<TableRow> = s.ifaces.iter().map(iface_row).collect();
    ui.set_ifaces(model_of(ifaces));

    let virtuals: Vec<TableRow> = s.virtual_ifaces.iter().map(iface_row).collect();
    ui.set_virtual_ifaces(model_of(virtuals));

    ui.set_ping_target(s.ping_target.clone().into());
    ui.set_ping_now(format!("{:.1} ms", s.ping.avg).into());
    ui.set_ping_min(format!("{:.1} ms", s.ping.min).into());
    ui.set_ping_avg(format!("{:.1} ms", s.ping.avg).into());
    ui.set_ping_max(format!("{:.1} ms", s.ping.max).into());
    ui.set_ping_loss(format!("{:.0}%", s.ping.loss_pct).into());
    let ping_ceiling = (s.ping.max * 1.3).max(5.0);
    ui.set_ping_path(model::line_path(&s.ping_hist, ping_ceiling, 600.0, 60.0).into());

    ui.set_talkers(model_of(
        s.talkers
            .iter()
            .map(|t| {
                row(
                    &t.name,
                    &t.mac,
                    &fmt_bytes(t.down_bytes),
                    &fmt_bytes(t.up_bytes),
                )
            })
            .collect(),
    ));

    ui.set_status(s.status.clone().into());
}

fn row(a: &str, b: &str, c: &str, d: &str) -> TableRow {
    TableRow {
        c1: a.into(),
        c2: b.into(),
        c3: c.into(),
        c4: d.into(),
        c5: "".into(),
        c6: "".into(),
        c7: slint::Color::from_rgb_u8(0xe2, 0xe8, 0xf0),
    }
}

/// Six columns, for the Interfaces card.
#[allow(clippy::too_many_arguments)]
fn row6(a: &str, b: &str, c: &str, d: &str, e: &str, f: &str) -> TableRow {
    TableRow {
        c1: a.into(),
        c2: b.into(),
        c3: c.into(),
        c4: d.into(),
        c5: e.into(),
        c6: f.into(),
        c7: slint::Color::from_rgb_u8(0xe2, 0xe8, 0xf0),
    }
}

/// RouterOS colours its flags column the same way: disabled red, running green,
/// dynamic blue, a mere slave grey.
fn flag_color(flags: &str) -> slint::Color {
    if flags.contains('X') {
        slint::Color::from_rgb_u8(0xf8, 0x71, 0x71)
    } else if flags.contains('D') {
        slint::Color::from_rgb_u8(0x38, 0xbd, 0xf8)
    } else if flags.contains('R') {
        slint::Color::from_rgb_u8(0x34, 0xd3, 0x99)
    } else {
        slint::Color::from_rgb_u8(0x64, 0x74, 0x8b)
    }
}

/// One interface, as `/interface print` would put it on a line.
fn iface_row(i: &Iface) -> TableRow {
    let mut r = row6(
        &i.flags,
        &i.name,
        &i.kind,
        if i.addr.is_empty() { "—" } else { &i.addr },
        &fmt_rate_short(i.rx_mbps),
        &fmt_rate_short(i.tx_mbps),
    );
    r.c7 = flag_color(&i.flags);
    r
}

fn model_of(rows: Vec<TableRow>) -> ModelRc<TableRow> {
    ModelRc::from(std::rc::Rc::new(VecModel::from(rows)))
}

struct Options {
    demo: bool,
    probe: bool,
    seconds: u64,
    target: String,
    user: String,
    password: String,
    wan: Option<String>,
    ping: String,
    link_mbps: f64,
    screenshot: Option<String>,
}

impl Options {
    fn parse(args: &[String]) -> Result<Option<Options>, String> {
        let mut o = Options {
            demo: false,
            probe: false,
            seconds: 8,
            target: "127.0.0.1:8728".to_string(),
            user: "mikdash".to_string(),
            password: String::new(),
            wan: None,
            ping: "1.1.1.1".to_string(),
            link_mbps: 300.0,
            screenshot: None,
        };
        let mut i = 0;
        while i < args.len() {
            let value = |name: &str, i: &mut usize| -> Result<String, String> {
                *i += 1;
                args.get(*i)
                    .cloned()
                    .ok_or_else(|| format!("{name} needs a value"))
            };
            match args[i].as_str() {
                "-h" | "--help" => return Ok(None),
                "--demo" => o.demo = true,
                "--probe" => o.probe = true,
                "--seconds" => {
                    o.seconds = value("--seconds", &mut i)?
                        .parse()
                        .map_err(|e| format!("--seconds: {e}"))?
                }
                "--target" => o.target = value("--target", &mut i)?,
                "--user" => o.user = value("--user", &mut i)?,
                // The password is never an argv: `ps` shows every command line on
                // the machine, so it is read from the environment by name.
                "--password-env" => {
                    let var = value("--password-env", &mut i)?;
                    o.password = std::env::var(&var)
                        .map_err(|_| format!("environment variable {var} is not set"))?;
                }
                "--wan" => o.wan = Some(value("--wan", &mut i)?),
                "--ping" => o.ping = value("--ping", &mut i)?,
                "--screenshot" => o.screenshot = Some(value("--screenshot", &mut i)?),
                "--link-mbps" => {
                    o.link_mbps = value("--link-mbps", &mut i)?
                        .parse()
                        .map_err(|e| format!("--link-mbps: {e}"))?
                }
                other => return Err(format!("unknown argument {other}")),
            }
            i += 1;
        }
        if !o.demo && o.password.is_empty() {
            o.password = std::env::var("MIKDASH_PASSWORD").unwrap_or_default();
        }
        Ok(Some(o))
    }
}

fn print_usage() {
    println!(
        "mikrotik-dashboard — native RouterOS dashboard (1920x1080)\n\n\
         USAGE:\n  \
         mikrotik-dashboard --demo\n  \
         mikrotik-dashboard --target 192.0.2.1:8728 --user mikdash --password-env MIKDASH_PASSWORD\n  \
         mikrotik-dashboard --probe [--demo] [--seconds N]\n\n\
         OPTIONS:\n  \
         --demo                 run a simulated RouterOS on a local port and chart it\n  \
         --target HOST:PORT     RouterOS API endpoint (default 127.0.0.1:8728)\n  \
         --user NAME            API user (default mikdash)\n  \
         --password-env VAR     read the API password from VAR (falls back to MIKDASH_PASSWORD)\n  \
         --wan IFACE            interface the Traffic card charts (default: detected)\n  \
         --ping HOST            ping target for the Latency card (default 1.1.1.1)\n  \
         --link-mbps N          uplink speed the Bandwidth gauges scale to (default 300)\n  \
         --probe                headless: poll and print one frame, then exit\n  \
         --screenshot FILE.ppm  headless: render one 1920x1080 frame to a PPM and exit\n  \
         --screenshot 'f-{{}}.ppm'  headless: a frame every 2 s, numbered ({{}} = frame index)\n  \
         --seconds N            seconds of polling before probe/screenshot (default 8)"
    );
}
