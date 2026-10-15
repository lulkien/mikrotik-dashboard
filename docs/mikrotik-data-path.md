# How this dashboard reads a router

Every number on screen comes from the RouterOS **binary API** over one TCP socket —
port `8728`, or `8729` for `api-ssl`. Nothing scrapes the web UI, logs in over SSH,
or uses the REST API. `src/ros/` holds the whole client: `protocol.rs` is the wire
framing, `client.rs` the connection and command vocabulary, `sim.rs` a
RouterOS-shaped server used for development.

## 1. The wire

A *sentence* is a sequence of length-prefixed words terminated by a zero-length word;
the length prefix widens from 1 to 5 bytes. A command is a sentence whose first word
is the command path (`/system/resource/print`) and whose remaining words are
attributes:

| word | meaning |
|---|---|
| `=key=value` | a setting or an argument |
| `?key=value` | a query filter |
| `=.proplist=a,b,c` | return only these fields |
| `.tag=N` | label this command's replies |

Replies start with `!re` (one row), `!done` (end of reply), `!trap` (the router
refused) or `!fatal` (closing the connection). Two details are load-bearing and both
are easy to get wrong:

- **The length prefix is variable-width**, so the high bit of each byte means "another
  length byte follows". Reading it as a fixed 32-bit integer appears to work against
  small replies and then mis-frames on a large table.
- **A `!trap` is not a transport error.** `RosError::Trap` is split into *absent*
  ("no such command", "not supported") and *denied* ("not enough permissions",
  "not enough privileges"); both are latched, and the poller stops asking for that
  menu rather than retrying forever. A dead connection is deliberately **not**
  classified that way — a dropped link that looked like a refusal would strand a card
  permanently.

The API is byte-transparent: `/file/read` and `/export` answer with raw bytes, so
words are decoded lossily rather than dropping the row.

### Who may read

A read-only API account feeds every card:

```routeros
/user group add name=mikdash policy=read,api,test,!local,!telnet,!ssh,!ftp,!reboot,!write,!policy,!password,!sensitive
/user add name=mikdash group=mikdash password=<a-strong-password>
```

Password handling lives in the shell, not in argv: the app reads the environment
variable named by `--password-env`, and the convention is a mode-600 file such as
`~/.config/mikdash/api.env`.

## 2. The collector

`src/poll.rs` runs one thread per dashboard: one connection, one read per menu at that
menu's cadence, and a snapshot per tick sent to the UI thread over a channel.

| subscription | menu | cadence |
|---|---|---|
| identity | `/system/identity/print` | 300 s |
| system | `/system/resource/print` | 5 s |
| ifaces | `/interface/print` | 5 s |
| conns | `/ip/firewall/connection/print` | 5 s |
| ping | `/tool/ping` (`count=5 interval=0.2`) | 5 s |
| talkers | `/ip/kid-control/device/print` | 5 s |
| leases | `/ip/dhcp-server/lease/print` | 15 s |
| addresses | `/ip/address/print` | 60 s |
| dhcpnet | `/ip/dhcp-server/network/print` | 60 s |
| dns | `/ip/dns/print` | 60 s |

Five seconds is the floor on the fast menus: measured against a RouterOS 7 device that
is ~1.2 API calls a second, which the footer counts. The cost is resolution — the
Traffic chart samples `ifaces`, so its window is `samples x 5s` and the card prints the
real span — and `/tool/ping` takes `interval=0.2s` so five probes cost ~1 s instead of
blocking the whole sweep for five (measured: "last sweep 4081 ms" before, 884 ms
after).

There is no cache and no subscriber registry. The cache in a multi-client design exists
for many subscribers; here there is exactly one viewer, and it is this process.

## 3. Card by card

| card | menu |
|---|---|
| Traffic | `/interface/print` `rx-byte`/`tx-byte` deltas on the WAN interface |
| Bandwidth | the same rates against the link speed |
| Connections | `/ip/firewall/connection/print` |
| System | `/system/resource/print` + `/system/identity/print` |
| Interfaces | `/interface/print` + `/ip/address/print`, real interfaces only |
| Virtual Interfaces | the same two menus, VLANs / bridges / PPPoE only, ordered active-first |
| Latency | `/tool/ping` against the configured target |
| Top Talkers | `/ip/kid-control/device/print` |

Two mappings are worth knowing:

- **Top Talkers needs router-side setup.** It reads RouterOS **Kid Control**, which
  only tracks devices once at least one Kid Control profile exists; the README carries
  the one-line recipe. Measured on a RouterOS 7 hEX: with the profile in place the menu
  lists 23 devices with names taken from their DHCP hostnames, and
  `rate-down`/`rate-up` read `0bps` for every idle one — so the card ranks and displays
  **cumulative bytes** (`bytes-down`/`bytes-up`) instead, which on a quiet LAN would
  otherwise be an alphabetical list of zeroes.
- **The WAN interface is picked, not configured**: the interface whose comment names it
  as the WAN, else the running `pppoe-out*`, else the one holding the default route.
  `--wan` overrides it.

## 4. Deliberately not read

| topic | choice | why |
|---|---|---|
| Traffic rates | `/interface/print` byte deltas, not the `/interface/monitor-traffic` stream | One poll covers the Traffic chart, the Bandwidth bars **and** the Interfaces cards; rates match what `monitor-traffic` reports as `rx-bits-per-second`, and there is no long-lived stream to cancel on a connection shared with table reads |
| Tags | the protocol understands `.tag=N`; the poller issues one command at a time | With one viewer and no streams, a tag map has nothing to multiplex |
| TLS | `8728` only | `api-ssl` is a later step, not an oversight; the framing is identical |
| WireGuard peers | not read | A card for a feature the target network does not run is noise |
| Caching, room demand, update check | absent | All of it serves a multi-client deployment |

## 5. Reaching a router

The API service is usually scoped by address on the router
(`/ip service set api address=...`), and on a VLAN-segmented network the dashboard host
may simply not be on the permitted segment. Three ways in, in order of preference:

1. allow the dashboard host on the API port and run it where it is;
2. run `ros-sim --serve` on a host that *can* reach the router, and point the dashboard
   at that host;
3. `--demo`, which exercises everything except a real router.

Whichever way, the app reports what it cannot reach — a refused menu, a timeout, a bad
password — instead of inventing values.
