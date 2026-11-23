# ridgewarden

[![CI](https://github.com/Londopy/ridgewarden/actions/workflows/ci.yml/badge.svg)](https://github.com/Londopy/ridgewarden/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/Londopy/ridgewarden)](https://github.com/Londopy/ridgewarden/releases/latest)
[![changelog](https://img.shields.io/endpoint?url=https://raw.githubusercontent.com/Londopy/ridgewarden/gh-pages/changelog-badge.json)](CHANGELOG.md)
[![License](https://img.shields.io/github/license/Londopy/ridgewarden)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-orange)](https://www.rust-lang.org)
[![Linux](https://img.shields.io/badge/platform-Linux%20(Pi%20%7C%20x86__64)-555)](#building-from-source)

**The shared agent behind a suite of AREDN mesh apps: polls the node once, keeps a signed log every site agrees on, and keeps working when the mesh splits.**

ridgewarden is a small agent that runs on a Raspberry Pi (or any Linux box) on an AREDN node's
LAN. It reads the node's API (never writes to it), keeps a signed append-only event log
that merges cleanly after the mesh splits and heals, syncs that log with the agents at
other sites, and serves a small dashboard. The apps are separate programs in their own
repos; each one runs next to a core and builds its pages from the core's event feed.

| App | Repo | What it does |
|---|---|---|
| Mountaintop SDR network (01) | [ridgewarden-sdr](https://github.com/Londopy/ridgewarden-sdr) | A signed directory of every receiver on the mesh, amateur-band-only profiles over RF, and finding who hears a signal loudest |
| Services that survive site loss (02) | [ridgewarden-failover](https://github.com/Londopy/ridgewarden-failover) | A MeshChat-compatible chat backend on the signed log: keeps working when a site goes down, merges when it returns |
| Planner (03) | [ridgewarden-planner](https://github.com/Londopy/ridgewarden-planner) | Failure what-ifs on the live mesh, terrain and ITM predictions calibrated against real links, candidate sites ranked by redundancy |
| Offline library (04) | [ridgewarden-library](https://github.com/Londopy/ridgewarden-library) | Offline Wikipedia, maps and weather alerts on the mesh, mirrored between sites and checked against the publishers' hashes |
| Topology game (05) | [ridgewarden-game](https://github.com/Londopy/ridgewarden-game) | Capture the Site: teams hold chains of real links, scored only while the links are up |
| Service health (06) | [ridgewarden-health](https://github.com/Londopy/ridgewarden-health) | Probes every advertised service from every site; tells link problems from service problems |
| Link Doctor (07) | [ridgewarden-linkdoctor](https://github.com/Londopy/ridgewarden-linkdoctor) | Link history, drift, one-way and noise alarms, single points of failure, a scored next-workday list |
| Status page (08) | [ridgewarden-status](https://github.com/Londopy/ridgewarden-status) | Green, yellow or red for served agencies, in plain words, with incidents, maintenance and a wall display |
| Weather sensor (09, experimental) | [ridgewarden-weather](https://github.com/Londopy/ridgewarden-weather) | Wet-antenna and ducting signals in link data, joined with rain and soundings, exported as an open dataset |
| Passive radar (10, experimental) | [ridgewarden-radar](https://github.com/Londopy/ridgewarden-radar) | Passive radar detections from blah2 over the mesh, tracked across sites and checked against ADS-B |
| Lightning (11) | [ridgewarden-lightning](https://github.com/Londopy/ridgewarden-lightning) | Locates lightning from receivers at several sites with the internet down, with dry-lightning fire alerts |
| Data mules (12) | [ridgewarden-mule](https://github.com/Londopy/ridgewarden-mule) | Plans what couriers carry between mesh islands and tracks signed delivery receipts |

```
$ ridgewarden diff
tst-shack: 2026-11-19 07:46 UTC compared with 2026-11-18 07:45 UTC
  New nodes on the mesh: tst-hilltop
  Gone services: Old Camera (tst-north)
  Firmware on tst-shack changed: 3.25.8.0 to 4.26.7.0
```

## Features

- **One poller per site.** Only the core talks to the AREDN node, at most once a minute
  (default every 5). The apps never touch it. 4.x nodes answer at `/a/sysinfo` and 3.x
  nodes at `/cgi-bin/sysinfo.json`; the core finds which, and reads each reply by the
  `api_version` it reports, since the two firmware lines use some field names differently.
- **What changed (spec 00 M0).** Nodes, services, links and firmware, today against yesterday.
- **Signed, partition-tolerant log (spec 00 M1).** Every observation is an Ed25519-signed,
  hash-chained event in plain JSON (Part 97: sign, never encrypt). Agents pull from each
  other over plain HTTP. After a partition heals, both sides hold exactly the same events,
  including both branches when one key ended up on two boxes. CI runs 20 seeded drills of
  random partitions, flaky links, cloned keys and pruning, and checks that every site ends
  up identical with nothing lost. Those partitions are simulated, with agents on one
  machine; no real mesh has been split under it yet.
- **Survives crashes and power cuts.** Each event is on disk before anyone sees it. Tests
  kill a writer mid-append, and simulate a power cut by cutting the tail off the
  database's write-ahead log; either way the log reopens whole and carries on numbering
  where it left off.
- **Trust you choose.** Every correctly signed event is stored and relayed, but apps only
  believe events from this station and the stations on the signed trust list.
  `ridgewarden revoke` stops anyone believing a station's later events; someone holding a
  stolen key can still write events that claim to be older, so also take the station off
  the trust list.
- **Bounded.** A peer can't make an agent download unbounded data, stall its poller, or
  push its clock far ahead: pulls, pages, authors and clock jumps are all capped.
- **Disk use that levels off, and shows it.** Polls and probes live in a separate telemetry
  log per station. A signed checkpoint lets every site prune it after 30 days
  (configurable); identity, trust, lists and app data are kept. The dashboard and
  `ridgewarden status` show the size of the main log, telemetry, blobs and other files,
  how fast the main log grows, and where telemetry levels off at the current rate.
- **Joining with a QR code.** `ridgewarden invite` prints the invite line and its QR code;
  the dashboard's invite page shows both to someone on the machine itself.
- **Files over the mesh.** Offline maps, reference content and updates move as 1 MiB
  chunks, each checked against its hash, resumable and rate-limited (overall, and per
  peer for slow links), fetched only where wanted, from the peers an app prefers first.
- **Updates without the internet.** `ridgewarden update` installs signed releases from
  GitHub, or from the mesh when a site has no internet, and only after you confirm. It
  works in a new folder in /var/tmp that only root can open, never in the data dir or
  $TMPDIR, so the agent's own user can't swap a release between the check and the
  install.
- **Optional crawl.** One hub agent can also poll the other backbone nodes (with their
  owners' OK), so links between sites without an agent show up too.
- **Optional metrics.** `GET /local/v1/metrics` gives the numbers from `ridgewarden status`
  in Prometheus' text format, for a scraper on the same machine (off by default).

## Install

On a 64-bit Raspberry Pi OS or x86_64 Linux box on an AREDN node's LAN:

```sh
curl -fsSL https://github.com/Londopy/ridgewarden/releases/latest/download/install.sh | sudo RIDGEWARDEN_CALLSIGN=N0CALL sh
```

To install the core and the apps in one go, name the apps you want (or none for the ten
main ones, or `all` to add the two experimental ones):

```sh
curl -fsSL https://github.com/Londopy/ridgewarden/releases/latest/download/install-all.sh | sudo RIDGEWARDEN_CALLSIGN=N0CALL sh -s -- health linkdoctor status
```

Before each app's installer runs, its release's checksums are checked with the core's
release key, and the installer against those checksums. On a box with no core yet, the
core comes from its own installer over HTTPS first, so those checks then show the apps
came from the same release key as that core.

The script picks the right build, checks it against `SHA256SUMS.txt`, installs
`/usr/local/bin/ridgewarden` and a systemd service, and starts it. Every
[release](https://github.com/Londopy/ridgewarden/releases/latest) also has:

| Download | What it is |
|---|---|
| `ridgewarden_<version>_arm64.deb` / `_amd64.deb` | Debian / Raspberry Pi OS package with the systemd service |
| `ridgewarden-v<version>-<arch>-linux.tar.gz` | Static binary, service file, README and license |
| `ghcr.io/londopy/ridgewarden:<version>` | Docker image (set `data_dir = "/data"`, mount `/data` and `/config`, and use `--network host` so apps on the same box can reach the local API) |
| `install-all.sh` | The core plus the apps you name, each checked with the core |
| `<file>.torrent` and magnet QR codes | Each `.deb` and `.tar.gz` as a torrent, with its GitHub download as the web seed (the scripts, checksums and signature are a few KB and have none) |
| `SHA256SUMS.txt` and `SHA256SUMS.txt.sig` | Checksums, signed with the release key |

In the Docker image the agent runs as root, so its app token is root's: run the apps in
the same container setup or use the packages, which give each app its own user in the
`ridgewarden` group. The image has nothing but the binary, so `ridgewarden update` can't
run in it (no curl or tar): pull the new image instead.

Check a download with `sha256sum -c SHA256SUMS.txt --ignore-missing`. If you already have
ridgewarden installed, `ridgewarden verify-release SHA256SUMS.txt` checks the release
signature with the key your install already trusts; after the first install, updates are
always checked that way. The binaries are not code-signed in the Windows sense.

## Usage

Bring a new site in with one line from any site already in:

```sh
# on a site already in: prints the line and, in a terminal, its QR code
ridgewarden invite --url http://n0call-pi.local.mesh:8790
# on the new site
ridgewarden join rw1.eyJh...
sudo systemctl restart ridgewarden
```

The line is `rw1.`, the signed invite as canonical JSON in base64url, and its signature:
no quotes or braces, so it pastes into a shell or a chat message as it is. `ridgewarden
join` shows who it is from and what accepting it means before it trusts anyone.

The QR code is drawn black on white whatever the terminal's colors. Invites carry every
trusted station's key, so the codes are big: about 100 columns wide for two stations
and 165 for a dozen. Widen the terminal or use the dashboard's invite page
(`/invite`). Past about 20 stations the line no longer fits in a QR code at all and has to
be pasted. That page only answers on the machine itself, since an invite is
signed with the station's key: open it on the Pi, or tunnel to it with
`ssh -L 8790:127.0.0.1:8790 pi@<pi>` and browse `http://127.0.0.1:8790/invite`. Set
`public_url` in the config so neither needs `--url`.

Joining trusts the inviting station and everyone it trusts. The trust list is a signed
event any station on it can update:

```sh
ridgewarden trust list                 # who this site trusts, and why
ridgewarden trust add <key> --call N0NEW
ridgewarden trust remove <key>
```

Other commands:

```sh
ridgewarden status                     # agent running? node answering? peers, disk use per class
ridgewarden top                        # the same and more, live in the terminal
ridgewarden diff --hours 24            # what changed on the mesh, as seen from this node
ridgewarden whoami                     # this station's callsign and public key
ridgewarden blob add map.pmtiles       # share a file over the mesh (--priority 0-9, 0 fetches first)
ridgewarden blob list
ridgewarden blob want <id>             # fetch one another site shared
ridgewarden blob want <id> --from http://10.1.2.3:8790   # ask that site first
ridgewarden blob export <id> out.pmtiles
ridgewarden blob unwant <id>           # stop fetching or keeping it and free its chunks
ridgewarden update                     # check for a signed release and install it if you say yes
```

`ridgewarden top` is for a terminal or an SSH session: an overview, then a page each for
peers, the log (each station's chains, forks, gaps and refused events), the node and the
nodes it crawls, blobs with fetch progress, and recent events. 1 to 6 or Tab switch pages,
Up and Down move through a list, r reads again and q quits. It reads again every 2 s
(`--every <seconds>`) and keeps showing the last good read, marked, if one fails. Run it
as the agent's user, `sudo -u ridgewarden ridgewarden top`, so it can read the agent's
database. `NO_COLOR=1` turns colors off.

The dashboard is on `http://<pi>:8790/`. To let other sites find this one without an
invite, advertise a service named `ridgewarden` on your node pointing at that URL. The
agent can't do this for you: an advert is a setting on the node, and the agent never
writes to the node. Other agents check an advert's signed identity before they sync with
it.

With `[metrics] enabled = true`, `GET /local/v1/metrics` answers in Prometheus' text
format: events and bytes per log, disk use, chains, forks, trusted stations, whether the
node answered and when, its links and services, each peer's last pull, and blobs. Like the
rest of the local API it only answers on loopback and wants `local.token` as a bearer
token, so run the scraper on the Pi and treat it like an app: that token can also sign
events. Each scrape counts the log the way the dashboard does, so once a minute is plenty.

## Configuration

`/etc/ridgewarden/ridgewarden.toml` (`ridgewarden init` writes a commented one):

```toml
callsign = "N0CALL"              # every event is signed under this
data_dir = "/var/lib/ridgewarden"
listen = "0.0.0.0:8790"

[node]
url = "http://localnode.local.mesh"
poll_interval_s = 300            # 60 minimum; agree it with the node's owner

[sync]
peers = ["http://10.1.2.3:8790"] # optional; adverts are found automatically

[trust]
keys = []                        # extra stations to trust; invites usually cover this
blocked = []                     # stations whose events this site won't store at all

[retention]
telemetry_days = 30              # your polls and probes are pruned everywhere after this

[blobs]
rate_kbps = 2000                 # bulk files never move faster than this
window = "1-6"                   # only fetch between these UTC hours ("" = any time)
auto_want = ["ridgewarden-release/"]   # fetch releases automatically, for offline updates

[blobs.peer_rate_kbps]           # optional: a lower cap for chunks from a peer on a slow link
"http://10.1.2.3:8790" = 500     # the address as `ridgewarden status` lists the peer

[crawl]                          # also poll other nodes for their links (off by default)
enabled = false
nodes = ["n0call-cuesta", "n0call-blackmtn"]

[metrics]
enabled = false                  # GET /local/v1/metrics for a Prometheus scraper on the Pi

[[app]]                          # linked from the dashboard
name = "Status page"
url = "http://10.1.2.3:8793/"
```

## For app authors

Apps use the `ridgewarden-client` crate in this repo:

- `GET /v1/feed?after=<n>`: every event the core holds, in arrival order, each flagged
  `trusted` and stamped with when the core received it. `follow()` turns it into your own
  SQLite views (`Projection`).
- `GET /v1/whoami`: the core's callsign, key and node (empty until the core has polled
  its node).
- `POST /local/v1/append`: have the core sign and log an event. Loopback only, with the
  token from `<data_dir>/local.token`, like every `/local/` route. The token is readable
  by the `ridgewarden` group; each app's package makes its own user in that group, so an
  app can sign through the core but never read the station key, which stays owner-only.
- `POST /local/v1/blob/add`: publish a file the app has put in `<data_dir>/imports`, with a
  name and a priority. Only that folder, so an app can't publish the station's key.
- `POST /local/v1/blob/want` and `/local/v1/blob/export`: fetch a blob from other sites,
  and write a finished one to `<data_dir>/exports`. `want` takes an optional `peers` list
  of core base URLs (`CoreClient::blob_want_from`): those are asked for each chunk first,
  in order, and the core's other peers only for chunks they don't have (404) or after one
  fails. A peer fails when it can't be reached within 20 s, doesn't send a whole chunk
  within 5 minutes, answers with an error, or sends data that doesn't match the chunk's
  hash; it then sits out the rest of that fetch round (rounds run about a minute, a
  minute apart). With `"only": true` (`blob_want_only_from`) no other peer is asked, so a
  chunk the listed peers don't have yet waits for a later round: for fan-out where each
  link should carry a chunk once. A new list replaces the old one, `[]` clears it,
  leaving `peers` out keeps it, and `unwant` forgets it. At most 16 peers; a bad address
  is a 400.
- `POST /local/v1/blob/unwant` `{root}`: stop fetching or keeping a blob. Its chunks are
  deleted unless another blob the core still wants or holds lists them; the answer says
  how many bytes were freed (`CoreClient::blob_unwant`). Auto-wanting leaves it alone
  afterwards; wanting it again undoes that.
- `GET /v1/blobs?root=<id>`: one blob's name, size, priority, author, whether it is
  complete and wanted, and whether a trusted station published it; 404 until its manifest
  arrives (`CoreClient::blob_status`). Without `root`, up to 1000 blobs, complete and
  wanted ones first.
- `GET /v1/peers`: the peers this core pulls from and how the last pull went.
- `node.poll` payloads say whether the node sent LQM link-quality trackers (`trackers`,
  3.22 and later) and how many trackers were left out as stale (`stale_skipped`), so an
  app can tell a partial link list from a quiet one, and carry the node's `api_version`.
  `aredn::parse` reads a reply by that version (`NodeObs::api`): OLSR's 0-1 link
  qualities and services by address from 1.x, LQM-only links and services by name from
  2.x, and both where the version is missing or newer than it knows.
- The client's `tui` feature is what `ridgewarden top` is built from, for an app's own
  terminal view: `tui::run` drives a `View`, `Refresh` loads data on a thread of its own so
  a slow source never freezes the screen, `Fetch` reads JSON from a local API with a
  timeout, and `Chrome`, `pill_table`, `Notice` and `Theme` draw the title, tabs, state
  pills and loading, empty and error states the way `top` does (`NO_COLOR` respected).

## Try it without a mesh

`ridgewarden-sim` serves a fake mesh whose sysinfo is shaped like the firmware's: 4.x nodes
answer at `/a/sysinfo` and redirect `/cgi-bin/sysinfo.json` there, and 3.x nodes answer at
`/cgi-bin/sysinfo.json` the way 3.25.8.0 does (OLSR link qualities, services by address)
and with an HTML page at `/a/sysinfo`. Both keep a dead neighbor's link-quality tracker
listed for a day, as the firmware does. It hosts fake services and has control endpoints
to break things:

```sh
cargo run -p ridgewarden-sim -- examples/slo-demo.toml
curl -X POST "http://127.0.0.1:9100/_ctl/link?a=TST-NORTH&b=TST-PASO&up=false"
curl -X POST "http://127.0.0.1:9100/_ctl/advertise?node=TST-SHACK&name=Offline%20Map&up=false"
curl -X POST "http://127.0.0.1:9100/_ctl/service?node=TST-SHACK&name=Mesh%20Chat&up=false"
curl -X POST "http://127.0.0.1:9100/_ctl/node?node=TST-CUESTA&up=false"
```

## Building from source

```sh
git clone https://github.com/Londopy/ridgewarden
cd ridgewarden
cargo build --release
cargo test --workspace
```

Static Linux binaries (what CI builds):

```sh
cargo install cross --locked
cross build --release --target aarch64-unknown-linux-musl -p ridgewarden   # Pi
cross build --release --target x86_64-unknown-linux-musl -p ridgewarden
```

## How it works

- **Events.** Each event's `id` is the BLAKE3 hash of its canonical JSON body, and `sig` is
  an Ed25519 signature over that hash (strict verification; keys and callsigns must be in
  canonical form). Each author's events are numbered and chain through `prev`; a Lamport
  clock gives every agent the same order even when wall clocks disagree. From this
  version on, `mono_time` counts on from where the database left off, across restarts,
  so a station's new events never go back in it (0.1.0 started it again from 0 on each
  start).
- **Merge.** The log is a set of events keyed by id, so merging is a union. Heads carry, per
  author, the gap-free chain length, fork points, and an XOR digest of every id held. When
  digests differ, agents compare digests per range of sequence numbers and fetch only the
  ranges that differ.
- **Policy on read.** Trust and revocation are applied when the feed is read, never by
  refusing to store, so what an agent holds never depends on arrival order.
- **Storage.** SQLite in WAL mode with full fsync, so a power cut can't make an agent reuse
  sequence numbers its peers already hold. Beside the log, the core keeps each poll of
  its own node in tables of its own: parsed (links, firmware, the service and host lists)
  for 8 days, for "what changed" and `top`, and the raw reply for 2 days, to check what the
  node actually sent. Those tables stay on the machine; other sites get the poll as its
  `node.poll` event.

## Layout

```
src/
  main.rs        CLI: init, run, status, top, invite, join, trust, blob, update, diff, whoami
  tui.rs         ridgewarden top
  agent.rs       wiring: poll, crawl and sync loops next to the web server
  app.rs         shared state, node poller, crawl, peer discovery
  node.rs        this site's node polls, parsed into tables
  log.rs         append, verify, merge, chains, checkpoints, digests, forks, feed
  trust.rs       invites, joining, the signed trust list
  invite.rs      the one-line invite format
  qr.rs          QR codes for invites (ISO/IEC 18004 byte mode, levels M and L)
  usage.rs       disk use per class and where it is heading
  blobs.rs       chunked, verified file transfer
  update.rs      signed releases from GitHub or the mesh
  store.rs       SQLite schema
  sync.rs        pull-only gossip client
  diff.rs        what changed (spec 00 M0)
  metrics.rs     the optional Prometheus metrics
  web.rs         dashboard, sync API, local API for apps
crates/ridgewarden-client/   shared crate for apps: events, sysinfo parser, feed follower,
                            mesh projection, graph analysis, page layout, terminal views
crates/ridgewarden-client/tests/fixtures/   an AREDN 3.25.8.0 sysinfo reply, from its source
crates/ridgewarden-sim/      fake AREDN mesh for tests and demos
packaging/                  systemd unit, install.sh, .deb scripts, Dockerfile
scripts/release_notes.py    builds the release page
scripts/qr_vectors.py       reference fingerprints for the QR tests (needs segno)
tests/partition.rs          forced partitions, forks, every route, local API
tests/drills.rs             20 seeded random partition drills with pruning
tests/crash.rs              a writer killed mid-append, a write-ahead log cut short
tests/poller.rs             polling fake 3.x and 4.x nodes over HTTP
tests/update.rs             updates from a local server and the mesh, and each way one is refused
examples/slo-demo.toml      seven-site demo scenario
```

## Limits

- Not yet run on a real AREDN mesh, Pi or node. Polling is tested against the simulator and
  against a 3.25.8.0 reply built field for field from that release's `sysinfo.json` source
  (made-up names and numbers); the partitions in the drills, and the crashes and power cuts
  in `tests/crash.rs`, are simulated on one machine.
- The spec's ten-minute join and 1 GB-per-site measures need a real mesh group to time and
  measure; `ridgewarden status` shows the numbers to check.
- Invites and the trust list haven't been tried by operators who aren't the author yet,
  which the spec asks for.
- A station can't move to a new key and keep its chain: today it revokes the old key
  (`ridgewarden revoke`), makes a new one and joins again with an invite, and a station on
  the trust list adds the new key. Station and device keys (spec change 3) are the answer
  the spec plans, and wait for users to ask, as it says.
- Sub-logs beyond telemetry (change 2): the same.
- A 2-of-3 threshold of offline root keys over the release keys: releases are signed by one
  release key for now.
- Range-based reconciliation (change 6): left on purpose; for a regional mesh the heads
  exchange is a few KB, and the spec says why it would save little until main-log and
  telemetry chains are reconciled separately.
- Meshtastic, MeshCore and Reticulum bridges: separate programs that need the radios.
- A Build-a-Pi menu entry: lives in Build-a-Pi's repo.
- Apps plug in as separate programs on the local API and `ridgewarden-client`, never as
  code loaded into the agent, so a crash or a bad dependency in an app can't take down the
  process that signs for the station. There is no in-process plugin API (spec 00 M3).
- A service or host list bigger than one event holds (256 KB as JSON, a couple of
  thousand services) stays in the core that polled it: its dashboard, `top` and "what
  changed" use it, but apps read lists from the log, so no app gets it, here or elsewhere.
- Per-peer blob rates are set by hand. The core could guess them from the PHY rates in its
  link polls, but those overstate what a shared RF link carries, so it doesn't.
- No free-disk-space figure for apps: the standard library has no portable call for it
  and the core takes no new crates; apps that need it still run `df`.

## Contributing

Issues and pull requests are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md), and add a line
under `Unreleased` in [CHANGELOG.md](CHANGELOG.md) for anything a user would notice.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
