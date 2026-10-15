//! A RouterOS API connection: login, one synchronous command at a time.
//!
//! The error type separates "the router said no" (`Trap`) from "the transport
//! died" (`Io`), because a dead connection mistaken for a refusal stops a caller
//! asking for a menu that does exist, and it never recovers on its own.

use std::collections::BTreeMap;
use std::io::{self, BufReader, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

use super::protocol::{read_sentence, write_sentence};

/// One `!re` sentence: a RouterOS row, keyed by property name.
pub type Reply = BTreeMap<String, String>;

/// What went wrong. `Trap` is the router refusing; everything else is transport.
#[derive(Debug)]
pub enum RosError {
    Io(io::Error),
    /// The router answered `!trap`: a refusal, not a broken connection.
    Trap {
        message: String,
        category: String,
    },
    Protocol(String),
}

impl std::fmt::Display for RosError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RosError::Io(e) => write!(f, "{e}"),
            RosError::Trap { message, category } if !category.is_empty() => {
                write!(f, "router refused: {message} (category {category})")
            }
            RosError::Trap { message, .. } => write!(f, "router refused: {message}"),
            RosError::Protocol(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for RosError {}

impl From<io::Error> for RosError {
    fn from(e: io::Error) -> Self {
        RosError::Io(e)
    }
}

impl RosError {
    /// True when the router does not have this menu at all (a different RouterOS
    /// build), or this account may not read it.
    ///
    /// Both mean "stop asking": the poller latches on this and never retries the
    /// menu, which is the whole point of classifying them apart from `Io`.
    pub fn stop_asking(&self) -> bool {
        match self {
            RosError::Trap { message, .. } => {
                let m = message.to_lowercase();
                m.contains("no such command")
                    || m.contains("unknown command")
                    || m.contains("not supported")
                    || m.contains("not enough permissions")
                    || m.contains("not enough privileges")
                    || m.contains("permission denied")
                    || m.contains("no permissions")
            }
            _ => false,
        }
    }

    /// A short reason fit for a status line.
    pub fn short(&self) -> String {
        match self {
            RosError::Io(e) if e.kind() == io::ErrorKind::TimedOut => "timed out".to_string(),
            RosError::Io(e) => e.to_string(),
            other => other.to_string(),
        }
    }
}

pub struct RosClient {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    reads: u64,
}

impl RosClient {
    /// Connect and log in. `target` is `host:port`, port 8728 for the plain API.
    ///
    /// RouterOS 6.43 and later accept the name and password in the `/login`
    /// sentence itself, which is what this does; older builds answer with a
    /// challenge that needs an MD5 reply, and that reply is deliberately not
    /// implemented here: RouterOS 6 is out of scope, 7 answers with the sentence.
    pub fn connect(
        target: &str,
        user: &str,
        password: &str,
        timeout: Duration,
    ) -> Result<Self, RosError> {
        let addr = resolve(target, timeout)?;
        let stream = TcpStream::connect_timeout(&addr, timeout)?;
        stream.set_nodelay(true)?;
        // A read timeout is what stops a half-open connection from hanging the
        // poller: without it the collector thread blocks for ever instead of
        // reporting and reconnecting.
        stream.set_read_timeout(Some(Duration::from_secs(20)))?;
        let mut client = RosClient {
            reader: BufReader::new(stream.try_clone()?),
            writer: stream,
            reads: 0,
        };
        client.login(user, password)?;
        Ok(client)
    }

    fn login(&mut self, user: &str, password: &str) -> Result<(), RosError> {
        let words = vec![
            "/login".to_string(),
            format!("=name={user}"),
            format!("=password={password}"),
        ];
        let rows = self.exchange(&words)?;
        // A 6.42-style challenge arrives as `=ret=<hex>` on the done sentence.
        for row in &rows {
            if row.contains_key("ret") {
                return Err(RosError::Protocol(
                    "router asked for the pre-6.43 challenge/response login, which this client \
                     does not implement"
                        .to_string(),
                ));
            }
        }
        Ok(())
    }

    /// Run one command and collect every `!re` row until `!done`.
    pub fn cmd(&mut self, command: &str, args: &[String]) -> Result<Vec<Reply>, RosError> {
        let mut words = Vec::with_capacity(args.len() + 1);
        words.push(command.to_string());
        words.extend_from_slice(args);
        self.exchange(&words)
    }

    /// `/path/print` with an optional proplist, which is how a read narrows a menu
    /// to the fields a card actually shows.
    pub fn print(&mut self, path: &str, proplist: &[&str]) -> Result<Vec<Reply>, RosError> {
        let mut args = Vec::new();
        if !proplist.is_empty() {
            args.push(format!("=.proplist={}", proplist.join(",")));
        }
        self.cmd(&format!("{path}/print"), &args)
    }

    fn exchange(&mut self, words: &[String]) -> Result<Vec<Reply>, RosError> {
        write_sentence(&mut self.writer, words).map_err(RosError::Io)?;
        self.writer.flush().ok();
        self.reads += 1;
        let mut rows = Vec::new();
        loop {
            let sentence = read_sentence(&mut self.reader)?.ok_or_else(|| {
                RosError::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "router closed the connection",
                ))
            })?;
            let mut iter = sentence.iter();
            let kind = iter.next().map(String::as_str).unwrap_or("");
            match kind {
                "!re" => rows.push(parse_row(iter)),
                "!done" => return Ok(rows),
                "!trap" => {
                    let row = parse_row(iter);
                    return Err(RosError::Trap {
                        message: row.get("message").cloned().unwrap_or_default(),
                        category: row.get("category").cloned().unwrap_or_default(),
                    });
                }
                "!fatal" => {
                    let row = parse_row(iter);
                    return Err(RosError::Trap {
                        message: format!(
                            "fatal: {}",
                            row.get("message").cloned().unwrap_or_default()
                        ),
                        category: "fatal".to_string(),
                    });
                }
                // `.tag` echoes and anything else the router sends unprompted
                // (log lines from a stream we are not reading) are skipped.
                _ => continue,
            }
        }
    }
}

fn resolve(target: &str, timeout: Duration) -> Result<SocketAddr, RosError> {
    let mut addrs = target
        .to_socket_addrs()
        .map_err(|e| RosError::Protocol(format!("routeros: cannot resolve {target}: {e}")))?;
    addrs.next().ok_or_else(|| {
        let _ = timeout;
        RosError::Protocol(format!("routeros: {target} resolved to no address"))
    })
}

fn parse_row<'a>(words: impl Iterator<Item = &'a String>) -> Reply {
    let mut row = Reply::new();
    for word in words {
        let stripped = word.strip_prefix('=').unwrap_or(word);
        if let Some((k, v)) = stripped.split_once('=') {
            row.insert(k.to_string(), v.to_string());
        }
    }
    row
}

/// RouterOS prints a duration as number+unit pairs: `24ms651us`, `1s23ms`, `235us`.
/// Milliseconds.
///
/// This is not cosmetic. Measured on the hEX (RouterOS 7.24.5), `/tool ping`
/// answers `min-rtt=24ms651us` to an internet target and would answer `235us` to
/// something on the LAN — reading the microsecond tail as milliseconds is wrong by
/// a factor of a thousand, and the Latency card would show "235 ms".
pub fn parse_time_ms(text: &str) -> f64 {
    fn flush(num: &mut String, unit: &mut String, total: &mut f64, seen: &mut bool) {
        if num.is_empty() {
            unit.clear();
            return;
        }
        let value = num.parse::<f64>().unwrap_or(0.0);
        let factor = match unit.as_str() {
            "us" => 0.001,
            "" | "ms" => 1.0,
            "s" => 1000.0,
            "m" => 60_000.0,
            "h" => 3_600_000.0,
            "d" => 86_400_000.0,
            "w" => 604_800_000.0,
            _ => 1.0,
        };
        *total += value * factor;
        *seen = true;
        num.clear();
        unit.clear();
    }

    let mut total = 0.0;
    let mut seen = false;
    let mut num = String::new();
    let mut unit = String::new();
    for ch in text.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            if !unit.is_empty() {
                flush(&mut num, &mut unit, &mut total, &mut seen);
            }
            num.push(ch);
        } else if ch.is_ascii_alphabetic() {
            unit.push(ch.to_ascii_lowercase());
        } else if ch == '\u{b5}' || ch == '\u{3bc}' {
            // the micro sign, in case a build prints it
            unit.push_str("us");
        } else {
            flush(&mut num, &mut unit, &mut total, &mut seen);
        }
    }
    flush(&mut num, &mut unit, &mut total, &mut seen);
    if seen { total } else { 0.0 }
}

/// RouterOS prints uptime as `1w2d3h4m5s`, omitting leading units. Seconds.
pub fn parse_duration(text: &str) -> u64 {
    let mut total = 0u64;
    let mut num = 0u64;
    for ch in text.chars() {
        match ch {
            '0'..='9' => num = num.saturating_mul(10) + (ch as u64 - '0' as u64),
            'w' => {
                total += num * 7 * 86400;
                num = 0;
            }
            'd' => {
                total += num * 86400;
                num = 0;
            }
            'h' => {
                total += num * 3600;
                num = 0;
            }
            'm' => {
                total += num * 60;
                num = 0;
            }
            's' => {
                total += num;
                num = 0;
            }
            _ => {}
        }
    }
    total
}

/// `2d4h` from seconds, the way RouterOS prints a duration.
pub fn format_duration(secs: u64) -> String {
    let (d, h, m) = (secs / 86400, (secs % 86400) / 3600, (secs % 3600) / 60);
    if d > 0 {
        format!("{d}d {h}h {m}m")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sub_millisecond_times_are_milliseconds_not_seconds() {
        // the exact string the hEX answered on 2026-10-15, plus a LAN-fast one
        assert!((parse_time_ms("24ms651us") - 24.651).abs() < 0.001);
        assert!((parse_time_ms("235us") - 0.235).abs() < 0.001);
        assert!((parse_time_ms("1s23ms") - 1023.0).abs() < 0.001);
        assert!((parse_time_ms("12.4ms") - 12.4).abs() < 0.001);
        assert!((parse_time_ms("1m2s") - 62000.0).abs() < 0.001);
        assert_eq!(parse_time_ms(""), 0.0);
        assert_eq!(parse_time_ms("never"), 0.0);
    }

    #[test]
    fn durations_parse_from_the_router_format() {
        assert_eq!(
            parse_duration("2d4h5m6s"),
            2 * 86400 + 4 * 3600 + 5 * 60 + 6
        );
        assert_eq!(parse_duration("1w"), 7 * 86400);
        assert_eq!(parse_duration("5m"), 300);
        assert_eq!(parse_duration(""), 0);
    }

    #[test]
    fn durations_format_for_the_system_card() {
        assert_eq!(format_duration(2 * 86400 + 3 * 3600 + 40 * 60), "2d 3h 40m");
        assert_eq!(format_duration(3 * 3600 + 40 * 60), "3h 40m");
        assert_eq!(format_duration(40 * 60), "40m");
    }

    #[test]
    fn rows_keep_the_id_and_drop_the_equals_sign() {
        let words: Vec<String> = vec![
            "=.id=*1F".into(),
            "=name=ether1".into(),
            "=running=true".into(),
        ];
        let row = parse_row(words.iter());
        assert_eq!(row.get("name").map(String::as_str), Some("ether1"));
        assert_eq!(row.get(".id").map(String::as_str), Some("*1F"));
        assert_eq!(row.get("running").map(String::as_str), Some("true"));
    }

    #[test]
    fn a_missing_menu_is_a_stop_asking_trap() {
        let e = RosError::Trap {
            message: "no such command prefix".into(),
            category: "0".into(),
        };
        assert!(e.stop_asking());
        let e = RosError::Trap {
            message: "not enough permissions (9)".into(),
            category: "0".into(),
        };
        assert!(e.stop_asking());
        let e = RosError::Io(io::Error::new(io::ErrorKind::ConnectionReset, "reset"));
        assert!(!e.stop_asking());
    }
}
