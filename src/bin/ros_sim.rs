//! `ros-sim` — the RouterOS API simulator, as a standalone tool.
//!
//! ```text
//! ros-sim --serve --bind 127.0.0.1:8728 --user mikdash --password demo
//! ros-sim --check      # start on an ephemeral port, connect, print what came back
//! ```
//!
//! `--check` is the honest test of the client: it dials the simulator over TCP
//! with the same `RosClient` the dashboard uses, logs in, runs the dashboard's
//! commands and prints the rows. It proves the framing, the login and the reply
//! parsing work against a server that speaks the protocol — which is exactly what
//! a router does — without needing a router.

use std::time::{Duration, Instant};

use mikrotik_dashboard::ros::client::RosClient;
use mikrotik_dashboard::ros::sim::{self, SimConfig};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut bind = "127.0.0.1:8728".to_string();
    let mut user = "mikdash".to_string();
    let mut password = "demo".to_string();
    let mut check = false;
    let mut serve = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--check" => check = true,
            "--serve" => serve = true,
            "--bind" => {
                i += 1;
                bind = args.get(i).cloned().unwrap_or(bind);
            }
            "--user" => {
                i += 1;
                user = args.get(i).cloned().unwrap_or(user);
            }
            "--password" => {
                i += 1;
                password = args.get(i).cloned().unwrap_or(password);
            }
            "-h" | "--help" => {
                println!(
                    "ros-sim — RouterOS API simulator\n\n  \
                     --serve               serve on --bind (default 127.0.0.1:8728)\n  \
                     --check               self-test: serve on an ephemeral port and query it\n  \
                     --bind HOST:PORT      listen address\n  \
                     --user NAME           accepted API user (default mikdash)\n  \
                     --password VALUE      accepted password (default demo)"
                );
                return;
            }
            other => {
                eprintln!("ros-sim: unknown argument {other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    let cfg = SimConfig {
        bind: if check {
            "127.0.0.1:0".to_string()
        } else {
            bind.clone()
        },
        user: user.clone(),
        password: password.clone(),
        ..SimConfig::default()
    };
    let (router, addr) = sim::spawn(cfg).expect("bind the simulator");
    println!("ros-sim: listening on {addr} as user {user}");

    if check {
        match self_check(&addr.to_string(), &user, &password) {
            Ok(()) => println!("ros-sim: check OK"),
            Err(e) => {
                eprintln!("ros-sim: check FAILED: {e}");
                std::process::exit(1);
            }
        }
        println!(
            "ros-sim: {} sockets accepted, {} commands served",
            router.accepted.load(std::sync::atomic::Ordering::Relaxed),
            router.served.load(std::sync::atomic::Ordering::Relaxed)
        );
        return;
    }

    if serve {
        println!("ros-sim: serving until interrupted (Ctrl-C)");
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    } else {
        println!("ros-sim: nothing to do — pass --serve or --check");
    }
}

fn self_check(target: &str, user: &str, password: &str) -> Result<(), String> {
    // A wrong password must be refused before anything else is trusted.
    match RosClient::connect(target, user, "definitely-wrong", Duration::from_secs(5)) {
        Err(e) => println!("  login with a bad password refused: {e}"),
        Ok(_) => return Err("the simulator accepted a wrong password".to_string()),
    }

    let mut client = RosClient::connect(target, user, password, Duration::from_secs(5))
        .map_err(|e| e.to_string())?;
    println!("  login as {user}: ok");

    let resource = client
        .print("/system/resource", &[])
        .map_err(|e| e.to_string())?;
    let row = resource.first().ok_or("no /system/resource row")?;
    println!(
        "  /system/resource: {} {} cpu {}% mem {} free",
        row.get("board-name").map(String::as_str).unwrap_or("?"),
        row.get("version").map(String::as_str).unwrap_or("?"),
        row.get("cpu-load").map(String::as_str).unwrap_or("?"),
        row.get("free-memory").map(String::as_str).unwrap_or("?")
    );

    let interfaces = client
        .print("/interface", &["name", "rx-byte", "tx-byte"])
        .map_err(|e| e.to_string())?;
    println!("  /interface: {} rows", interfaces.len());
    let first = interfaces.first().ok_or("no interface rows")?;
    let (rx0, tx0) = (
        first
            .get("rx-byte")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0),
        first
            .get("tx-byte")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0),
    );
    let name = first.get("name").cloned().unwrap_or_default();
    let started = Instant::now();
    std::thread::sleep(Duration::from_millis(1200));
    let interfaces = client
        .print("/interface", &["name", "rx-byte", "tx-byte"])
        .map_err(|e| e.to_string())?;
    let second = interfaces
        .iter()
        .find(|r| r.get("name") == Some(&name))
        .ok_or("interface vanished")?;
    let rx1 = second
        .get("rx-byte")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    let tx1 = second
        .get("tx-byte")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    let dt = started.elapsed().as_secs_f64();
    let rx_mbps = (rx1 - rx0) as f64 * 8.0 / dt / 1e6;
    let tx_mbps = (tx1 - tx0) as f64 * 8.0 / dt / 1e6;
    println!("  {name}: rx {rx_mbps:.2} Mbps tx {tx_mbps:.2} Mbps over {dt:.2}s");
    if !(1.0..500.0).contains(&rx_mbps) {
        return Err(format!("implausible rx rate {rx_mbps:.2} Mbps"));
    }

    let conns = client
        .print("/ip/firewall/connection", &["protocol"])
        .map_err(|e| e.to_string())?;
    println!("  /ip/firewall/connection: {} rows", conns.len());
    if conns.is_empty() {
        return Err("no connection rows".to_string());
    }

    let leases = client
        .print("/ip/dhcp-server/lease", &["address", "status"])
        .map_err(|e| e.to_string())?;
    let bound = leases
        .iter()
        .filter(|r| r.get("status").map(String::as_str) == Some("bound"))
        .count();
    println!(
        "  /ip/dhcp-server/lease: {} rows, {bound} bound",
        leases.len()
    );

    let talkers = client
        .print("/ip/kid-control/device", &["name", "rate-down", "rate-up"])
        .map_err(|e| e.to_string())?;
    for t in talkers.iter().take(2) {
        println!(
            "  kid-control {} down {} up {}",
            t.get("name").map(String::as_str).unwrap_or("?"),
            t.get("rate-down").map(String::as_str).unwrap_or("?"),
            t.get("rate-up").map(String::as_str).unwrap_or("?")
        );
    }

    let peers = client
        .print(
            "/interface/wireguard/peers",
            &["interface", "last-handshake"],
        )
        .map_err(|e| e.to_string())?;
    println!("  /interface/wireguard/peers: {} rows", peers.len());

    let ping = client
        .cmd(
            "/tool/ping",
            &["=address=1.1.1.1".to_string(), "=count=3".to_string()],
        )
        .map_err(|e| e.to_string())?;
    let summary = ping
        .iter()
        .find(|r| r.contains_key("avg-rtt"))
        .ok_or("no ping summary row")?;
    println!(
        "  /tool/ping: avg {} loss {}%",
        summary.get("avg-rtt").map(String::as_str).unwrap_or("?"),
        summary
            .get("packet-loss")
            .map(String::as_str)
            .unwrap_or("?")
    );

    // A menu the simulator does not implement must come back as a trap that the
    // collector treats as "stop asking", not as a broken connection.
    match client.print("/ip/nonexistent/menu", &[]) {
        Err(e) if e.stop_asking() => println!("  unknown menu: refused as expected ({e})"),
        Err(e) => return Err(format!("unknown menu gave the wrong error: {e}")),
        Ok(_) => return Err("unknown menu answered with rows".to_string()),
    }

    let big = client
        .print("/ip/address", &["address", "interface"])
        .map_err(|e| e.to_string())?;
    println!("  /ip/address: {} rows", big.len());
    Ok(())
}
