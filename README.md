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
  (default every 5). The apps never touch it.
- **What changed (spec 00 M0).** Nodes, services, links and firmware, today against yesterday.
- **Signed, partition-tolerant log (spec 00 M1).** Every observation is an Ed25519-signed,
  hash-chained event in plain JSON (Part 97: sign, never encrypt). Agents pull from each
  other over plain HTTP. After a partition heals, both sides hold exactly the same events,
  including both branches when one key ended up on two boxes. CI runs 20 seeded drills of
  random partitions, flaky links, cloned keys and pruning, and checks that every site ends
  up identical with nothing lost.
- **Trust you choose.** Every correctly signed event is stored and relayed, but apps only
  believe events from this station and the stations on the club's signed trust list.
  `ridgewarden revoke` stops anyone believing a station's later events; someone holding a
  stolen key can still write events that claim to be older, so also take the station off
  the club list.
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
  GitHub, or from the mesh when a site has no internet, and only after you confirm.
- **Optional crawl.** One hub agent can also poll the other backbone nodes (with their
  owners' OK), so links between sites without an agent show up too.

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
| `<file>.torrent` and magnet QR codes | Each download as a torrent, with its GitHub download as the web seed |
| `SHA256SUMS.txt` and `SHA256SUMS.txt.sig` | Checksums, signed with the release key |

In the Docker image the agent runs as root, so its app token is root's: run the apps in
the same container setup or use the packages, which give each app its own user in the
`ridgewarden` group.

Check a download with `sha256sum -c SHA256SUMS.txt --ignore-missing`. If you already have
ridgewarden installed, `ridgewarden verify-release SHA256SUMS.txt` checks the release
signature with the key your install already trusts; after the first install, updates are
always checked that way. The binaries are not code-signed in the Windows sense.

## Usage

Join an existing club with one line from any of its sites:

```sh
# on a site already in the club: prints the line and, in a terminal, its QR code
ridgewarden invite --url http://n0call-pi.local.mesh:8790
# on the new site
ridgewarden join rw1.eyJh...
sudo systemctl restart ridgewarden
```

The QR code is drawn black on white whatever the terminal's colors. Invites carry every
trusted station's key, so the codes are big: about 100 columns wide for a club of two
stations and 165 for a dozen. Widen the terminal or use the dashboard's invite page
(`/invite`). Past about 20 stations the line no longer fits in a QR code at all and has to
be pasted. That page only answers on the machine itself, since an invite is
signed with the station's key: open it on the Pi, or tunnel to it with
`ssh -L 8790:127.0.0.1:8790 pi@<pi>` and browse `http://127.0.0.1:8790/invite`. Set
`public_url` in the config so neither needs `--url`.

Joining trusts the inviting station and everyone it trusts. The club's list of stations is
a signed event any of them can update:

```sh
ridgewarden trust list                 # who this site trusts, and why
ridgewarden trust add <key> --call N0NEW
ridgewarden trust remove <key>
```

Other commands:

```sh
ridgewarden status                     # agent running? node answering? peers, disk use per class
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

The dashboard is on `http://<pi>:8790/`. To let other sites find this one without an
invite, advertise a service named `ridgewarden` on your node pointing at that URL.

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
- `node.poll` payloads say whether the node sent 4.x link-quality trackers (`trackers`)
  and how many trackers were left out as stale (`stale_skipped`), so an app can tell a
  partial link list from a quiet one.

## Try it without a mesh

`ridgewarden-sim` serves a fake mesh whose sysinfo is shaped like the firmware's (4.x and 3.x
layouts, including 4.x keeping dead neighbors listed for a day). It hosts fake services
and has control endpoints to break things:

```sh
cargo run -p ridgewarden-sim -- examples/slo-demo.toml
curl -X POST "http://127.0.0.1:9100/_ctl/link?a=TST-NORTH&b=TST-PASO&up=false"
curl -X POST "http://127.0.0.1:9100/_ctl/advertise?node=TST-SHACK&name=Offline%20Map&up=false"
curl -X POST "http://127.0.0.1:9100/_ctl/service?node=TST-SHACK&name=Club%20Chat&up=false"
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
  clock gives every agent the same order even when wall clocks disagree.
- **Merge.** The log is a set of events keyed by id, so merging is a union. Heads carry, per
  author, the gap-free chain length, fork points, and an XOR digest of every id held. When
  digests differ, agents compare digests per range of sequence numbers and fetch only the
  ranges that differ.
- **Policy on read.** Trust and revocation are applied when the feed is read, never by
  refusing to store, so what an agent holds never depends on arrival order.
- **Storage.** SQLite in WAL mode with full fsync, so a power cut can't make an agent reuse
  sequence numbers its peers already hold. Raw snapshots of the node's own sysinfo stay
  local for 8 days.

## Layout

```
src/
  main.rs        CLI: init, run, status, invite, join, trust, blob, update, diff, whoami
  agent.rs       wiring: poll, crawl and sync loops next to the web server
  app.rs         shared state, node poller, crawl, peer discovery
  log.rs         append, verify, merge, chains, checkpoints, digests, forks, feed
  trust.rs       invites, joining, the club's signed trust list
  invite.rs      the one-line invite format
  qr.rs          QR codes for invites (ISO/IEC 18004 byte mode, levels M and L)
  usage.rs       disk use per class and where it is heading
  blobs.rs       chunked, verified file transfer
  update.rs      signed releases from GitHub or the mesh
  store.rs       SQLite schema
  sync.rs        pull-only gossip client
  diff.rs        what changed (spec 00 M0)
  web.rs         dashboard, sync API, local API for apps
crates/ridgewarden-client/   shared crate for apps: events, sysinfo parser, feed follower,
                            mesh projection, graph analysis, page layout
crates/ridgewarden-sim/      fake AREDN mesh for tests and demos
packaging/                  systemd unit, install.sh, .deb scripts, Dockerfile
scripts/release_notes.py    builds the release page
scripts/qr_vectors.py       reference fingerprints for the QR tests (needs segno)
tests/partition.rs          forced partitions, forks, every route, local API
tests/drills.rs             20 seeded random partition drills with pruning
examples/slo-demo.toml      seven-site demo scenario
```

## Not done yet

- Station and device keys (spec change 3), so a dead Pi can be replaced without its chain
  forking: waiting for users to ask, as the spec says.
- Sub-logs beyond telemetry (change 2): the same.
- A 2-of-3 threshold of offline root keys over the release keys: releases are signed by one
  release key for now.
- Range-based reconciliation (change 6): left on purpose; at club sizes the heads exchange
  is a few KB, and the spec says why it would save little until main-log and telemetry
  chains are reconciled separately.
- Meshtastic, MeshCore and Reticulum bridges: separate programs that need the radios.
- A Build-a-Pi menu entry: lives in Build-a-Pi's repo.
- The out-of-process plugin API (spec 00 M3): apps are separate programs on the local API
  instead.
- Per-peer blob rates are set by hand. The core could guess them from the PHY rates in its
  link polls, but those overstate what a shared RF link carries, so it doesn't.
- No free-disk-space figure for apps: the standard library has no portable call for it
  and the core takes no new crates; apps that need it still run `df`.
- The spec's ten-minute join and 1 GB-per-site measures need a real club mesh to confirm;
  `ridgewarden status` shows the numbers.

## Contributing

Issues and pull requests are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md), and add a line
under `Unreleased` in [CHANGELOG.md](CHANGELOG.md) for anything a user would notice.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
