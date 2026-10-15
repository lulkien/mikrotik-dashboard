# Design

## Process shape

```
collector thread                         UI thread
─────────────────                        ─────────
RosClient (TCP 8728)                     DashboardWindow (Slint)
  │  /system/resource/print               ▲
  │  /interface/print                     │ 250 ms Timer: drain the channel,
  │  /ip/firewall/connection/print        │ keep the newest Snapshot, push it
  │  /tool/ping …                         │ into the window's in properties
  ▼                                      │
Snapshot ──────── mpsc::Sender<Snapshot> ┘
```

The UI thread never touches the socket. A router that stops answering makes the
window **stale but alive** (the footer says so), never frozen: the poll thread
blocks on its own reads with a 20 s socket timeout plus a 5 s connect timeout, and
reconnects with 1s → 30s backoff.

`lib.rs` holds everything testable without a display (`ros`, `model`, `poll`);
`main.rs` holds the window and the property mapping; `shot.rs` is the offscreen
renderer for headless checks.

## Modules

| module | responsibility |
|---|---|
| `ros::protocol` | length-prefixed word framing, sentence read/write |
| `ros::client` | connect/login, one command at a time, `!re`/`!done`/`!trap` handling, trap classification (`stop_asking`), RouterOS duration parsing |
| `ros::sim` | a RouterOS-shaped API server for development (`--demo`, `ros-sim`) |
| `model` | one `Snapshot` = one dashboard frame, plus formatting (rates, bytes, uptime, clock) and the SVG paths for the chart, sparkline and gauges |
| `poll` | the collector thread: subscriptions, cadences, rate maths, error bookkeeping |
| `shot` | a Slint platform that renders into a buffer and writes a PPM |

## Polling

Eleven subscriptions, all always on (no rooms, no demand: there is one viewer):

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

Five seconds is the floor on the fast menus: measured against the hEX that is ~1.2
API calls a second (the footer counts them). The cost is resolution — the Traffic
chart samples `ifaces`, so its window is `samples x 5s` and the card prints the real
span — and `/tool/ping` takes `interval=0.2s` so five probes cost ~1s instead of
blocking the sweep for five (measured: "last sweep 4081 ms" before, 884 ms after).

A subscription the router refuses with "no such command" / "not enough permissions"
is disabled and listed in the footer status line; anything else is recorded as a
transient error and retried on the next cadence. Transport failure ends the
connection and restarts the reconnect loop.

Rates: `rx-byte`/`tx-byte` from `/interface/print` are kept per interface with the
instant they were read, and the delta over elapsed time is the rate. The WAN
interface is the running PPPoE one, else the one whose comment contains `WAN`, else
the first running `ether*`; `--interface` overrides. History is a 60-sample deque per
direction, so the chart is a 60-second window once warm.

## The UI: tokens, components, cards, layout

The Slint sources are laid out like a small front-end project, so a change to the
palette or a primitive does not mean editing every card:

```text
ui/theme.slint          design tokens: colour, radius, spacing steps, type scale
ui/components/          primitives any card is built from
  card.slint            the card shell: panel, heading, slot
  pill.slint            a status capsule
  stat.slint            a labelled figure
  card-heading.slint    the one heading style every card wears
  table.slint           the row shape the table cards take
  iface-card.slint      the interface table (used twice: real and virtual)
ui/cards/               one file per card, composed from the primitives
ui/layout/              the window chrome: header bar, status line
ui/dashboard.slint      the shell: chrome, the three columns, the wiring
```

Rules that keep it navigable:

- **No colour, radius, spacing or font size outside `theme.slint`.** A hex value in a
  card is a missing token.
- **A card owns its markup, the shell owns its geometry.** Card components declare the
  values they draw (`SystemCard` takes three arcs, three percentages and three info
  lines) and the columns pin their height (`height: 226px`), because the column budgets
  the vertical space, not the card.
- **Primitives never know a card by name.** `IfaceCard` gets rows; the shell decides
  whether they are the real interfaces or the virtual ones.
- **Components inherit their element directly** (`Card inherits Rectangle`,
  `StatusLine inherits Text`) rather than wrapping one: an extra inner element costs a
  border in layout and silently shifts everything below it by 2px.

## The 1920x1080 grid

Outer padding 16, gaps 12. Header 52; traffic row 272 (Traffic takes the rest of the
row; Bandwidth is pinned to 360px);
three equal columns (`column-width: (self.width - 32px - 24px) / 3`, pinned with
`min-width`/`max-width` on each column — left to the layout, content preferred
widths decide and the tables in Top Talkers/WireGuard won 892px while System,
Network and Latency were squeezed into 300px); status line 14. `1080 - 32 - 52 - 272 - 14 - 36 = 674` for the
column row.

- LEFT — Connections 538 (total, protocol bar with stretch ∝ count, legend, top 5
  sources, top 8 destinations) then **Latency 120** (five stats + sparkline).
  538 + 12 + 120 = 670, the column height. Both are pinned: a card inside the
  column's wrapper layout does not stretch.
- MIDDLE — System 226 (three 84x84 gauges, fixed squares so the arcs stay circular,
  then three info lines), Interfaces 210, Virtual 210.
  226 + 12 + 210 + 12 + 210 = 670, and each is pinned for the same reason (the
  System card previously ran its last line onto its own border).

  The two interface cards mirror `/interface print`: one line per interface in the
  router's own order — **no sorting** — with its flags column, type, address and
  rates. They split by type: `vlan`, `bridge` and `pppoe*` are the Virtual card,
  everything else (ethers, loopback) is an Interfaces card, because the hEX reports
  7 VLANs and a bridge alongside its five ethers and would otherwise bury them.
  Rows are 18px with a 6px gutter, and each card's list is a `ScrollView` that is
  enabled **only when its content overflows**: six real rows fit in the 144px the
  card leaves, so that card shows the router's order untouched; Virtual holds nine
  and scrolls, so it is ordered active-first (busiest first, idle ones in router
  order below) — which is what keeps the uplink and the busy VLANs inside the fold.
- RIGHT — Top Talkers, filling the column. There was a WireGuard card here; the
  network has no WireGuard peers, so both the card and its `/interface/wireguard/
  peers` subscription are gone and the slot is open for a card this router can
  actually feed.

The Bandwidth card is two vertical bars: the reading above, a fixed 72px track
below it filling from the bottom with its share of the link, and the interface
label under that. The track carries quarter lines so the empty height reads as a
scale, and the fill has a 3px floor so a 1% reading is still a bar. No percentage
is printed - the bar is the percentage - and the card's width is pinned (360px)
because two bars do not earn a wide card, so the Traffic chart takes the rest.

Charts are Slint `Path` elements whose `commands` are built in Rust against a fixed
viewbox (1000x240 traffic, 600x60 latency, 100x100 gauge), so the geometry does not
depend on the pixel size the layout happens to give the widget. Gauges sweep 270° from 135°, which is why `gauge_arc()` and the Slint track
share the same start point.

**Stretch only applies to direct children of a row.** A lone card wrapped in its
own `VerticalLayout` inside the column row came out content-sized (202px) with the
rest of the column empty, while the same card as a direct child of the row fills it
(662px) — the wrapper was removed for that reason. Nested layouts do not forward
cross-axis fill; when a column needs two cards, give each an explicit height.

**Nothing that a card shows may change the layout.** Slint satisfies minimum sizes
before sharing the leftover space by stretch, so a text whose minimum grows with its
value — a rate, a counter, a percentage — re-distributes the row and slides the
neighbouring card a few pixels on every poll. Every data-dependent cell therefore
carries `min-width`/`preferred-width` plus `overflow: elide`, every card carries
`min-width: 0`, and a frame series (`--screenshot 'f-{}.ppm'`) is measured to prove
the boundaries do not move.

Table columns use `min-width`/`preferred-width`, not `width`: inside a Slint layout
`width` is ignored and every cell competes for space, which silently elides cells
(the first render of this dashboard did exactly that, on the Network card's address
column).

## One interface, two cards

`--interface IFACE` names the interface both traffic cards follow: the Traffic chart's
curve and rates, and the Bandwidth gauges, all read from that one interface and it is
named in the Traffic card's heading. Without the flag the poller charts the detected
uplink (a running `pppoe*`, else the interface whose comment says WAN, else the first
running ether).

A name the router does not have — an interface deleted since the last launch, or a typo —
is reported as `interface: no interface named <name>` and the uplink is charted instead.
That is `charted_iface()` in `poll.rs`, unit-tested for all three cases, because a typo
that silently charts zeroes would look exactly like a dead link.

## The simulator, and why it is a socket

`ros::sim` is a real TCP server speaking the real protocol: it parses sentences,
authenticates `/login` (refusing a wrong password with the router's own wording),
honours `=.proplist`, and answers the dashboard's eleven menus with values that move
— a PPPoE WAN on a sum of sines, a connection table that jitters around 424 rows, 38
DHCP leases, five Kid Control devices, four WireGuard peers, single-digit-ms pings.
Unknown menus trap with "no such command prefix", which is what makes the
latch-on-denied path testable.

That choice is deliberate: a mock inside the process would prove the UI against
invented structs. `ros-sim --check` dials the simulator with the same `RosClient`
the dashboard uses and prints what came back, so the framing, login, proplist
filtering, rate deltas and trap handling are all exercised without hardware.

## Headless checks

`--probe` prints the same numbers the window draws, the traffic rates and the gauges
included, so "which interface is this card on" is answerable without a display.

| command | what it proves |
|---|---|
| `cargo test` | framing at every length width, duration parsing, row parsing, trap classification, rate maths, path generation, rolling history |
| `ros-sim --check` | the client against a live socket: login, bad password, tables, proplist, byte-delta rates, ping, unknown-menu trap |
| `mikrotik-dashboard --probe --demo` | the poll scheduler end to end, printed as one frame |
| `mikrotik-dashboard --screenshot out.ppm --demo` | the layout at exactly 1920x1080, no display server |
| `mikrotik-dashboard --screenshot 'f-{}.ppm' --demo --seconds 30` | a frame every 2 s — the series a measurement script reads to prove the card boundaries do not move with the data |

The screenshot mode exists because a locked desktop session refuses screen capture
(KDE's ScreenShot2 returns no reply), and because a captured compositor window is
not evidence of what the layout asked for — a software-rendered frame at the target
size is.
