# 00 Core agent

Repo: `ridgewarden` | Reviewed 2026-11-19 | Status: version 1 built, not yet released

Built: poller (3.x and 4.x), signed log with forks and revocation, pull-only sync with fork and digest repair, telemetry sub-log with signed checkpoints (change 1), invites with a QR code in the terminal and on the dashboard's invite page (change 4), the signed trust list (change 5), blob store with per-blob status, unwant, preferred source peers per blob, with an only-these-peers mode, and per-peer rate caps (the core side of app 04's topology-aware fan-out and per-link rate limits), `trackers` and `stale_skipped` in `node.poll` so apps can tell a partial poll, signed updates, packaging, `ridgewarden status` and the dashboard's disk use per class (main log, telemetry, blobs, other files) with the main log's yearly growth and where telemetry levels off, and 20 seeded forced-partition drills with pruning in CI (`tests/drills.rs`).

Not done: changes 2 and 3 and the 2-of-3 release root keys (the spec waits for users to ask; what each takes is under "Later"); change 6, range-based reconciliation (left on purpose; why is under "Later"); the Meshtastic, MeshCore and Reticulum bridges (other projects' code and radios to test with); the Build-a-Pi menu entry (lives in Build-a-Pi's repo); the plugin API (apps are separate programs on the local API instead); per-peer rates derived from link quality (the PHY rates in link polls overstate what a shared RF link carries, so rates are set by hand); a free-disk-space call for apps (no portable way in std without a new crate); the ten-minute join and the 13-site 1 GB measures need a real club mesh, though `ridgewarden status` and the dashboard now show the numbers to check them.

## What it is

One small Rust program on a Raspberry Pi (or any Linux box) on an AREDN node's LAN. It reads the node's read-only API, records what it sees as signed events, shares those events with the cores at other sites, and gives the apps on the same box a feed to build their pages from. It never writes to the node. Every other app in the suite depends on it.

## Who it's for

Every club that installs any ridgewarden app. Nobody installs the core for its own sake; the app installers pull it in.

## What exists today

| Project | What it does well | Why it isn't the answer here |
|---|---|---|
| Secure Scuttlebutt (SSB) | Per-author signed append-only logs, gossip replication. The closest design to ours. | Logs only ever grew until compaction was bolted on; onboarding was its biggest complaint; partial replication came late. Not used on AREDN. |
| p2panda (Rust) | Per-author logs with a prune point; targets radio and sneakernet transports | General framework, no AREDN knowledge |
| Hypercore / Holepunch | Sparse replication, explicit fork ids | Built for the internet; pruning had replication bugs |
| Willow / Earthstar | Partial sync by path, prefix deletion, a "drop" format for sneakernet | Young; capability model is more than a club needs |
| Iroh docs | Range-based set reconciliation, separate blob transfer | Internet-oriented |
| Reticulum / LXMF | Mature store-and-forward over many transports | Encryption is mandatory for relayed traffic, which Part 97 doesn't allow on ham RF |
| Nostr (HAMSTR) | Signed events, already tried over ham radio | Relay-centric, no topology knowledge |

Nothing found runs on AREDN or knows AREDN's topology. The new part is not the signed log itself; it is a Part 97-legal, plaintext, signed log that every site holds, that survives the mesh splitting, and that understands the mesh it runs on.

## What makes ours the best

- **Legal and readable on ham RF.** Signed, never encrypted, multi-hop. Reticulum can't do that legally on US ham frequencies; we make it the selling point.
- **One poll per site.** Twelve apps, one polite reader of the node.
- **Converges after any split**, including two boxes sharing one key. Proven in CI with forced partitions.
- **Joining takes one step.** A QR code or one pasted invite line, not hex keys (see changes below).
- **Predictable disk use.** Every kind of event has a retention class, and the dashboard shows how big the log will get.

## Changes from the v0.1 spec

1. **Retention from day one.** Each event kind gets a class: telemetry (polls, probes) expires after 30 days by default; summaries, alerts, receipts and identity events are kept. A signed "checkpoint" event lets an author mark everything before sequence N as prunable, and a local purge list stops pruned data being fetched again. SSB's main lesson is that a log with no pruning hits a wall.
2. **Sub-logs by class.** Each author's events are split into classes (telemetry, app data, blob index) so a slow link or a car can carry only what it's told to. Sync negotiates per class.
3. **Station and device keys.** A station's root key (held offline by the licensee) signs device keys for each Pi. Replacing a dead Pi or recovering from a stolen SD card no longer forks a chain or needs every other club member to edit their trust list.
4. **Invites.** `ridgewarden invite` prints a single line and QR code holding the club's trusted-key list, peer addresses and the inviting station's signature. The joining site's screen says plainly what accepting it means.
5. **Signed trust list.** The club's list of trusted stations is itself a signed, versioned event that any listed station can update, instead of a list each site edits by hand.
6. **Set reconciliation for big meshes.** Keep heads, paging and digest repair for v1. When a mesh has hundreds of authors, switch the digest step to range-based reconciliation (as in Willow and Iroh).

## Version 1 scope

Built: poller (3.x and 4.x), signed log with forks and revocation, pull-only sync with fork and digest repair, trust list, local feed and append API, crawl, "what changed" diff, simulator.

Built for v1 in the second pass:
- retention: a telemetry sub-log per station, signed checkpoints, a block list (change 1, in the simpler form of one telemetry class)
- invites and the signed trust list (changes 4 and 5)
- blob store (M2): 1 MiB BLAKE3 chunks, manifests as events, index blobs for big files, resumable, rate-limited, time window
- packaging (M3): `.deb`, Docker image, one-line install script. Docker matters because DigiPi-style images run read-only. A Build-a-Pi menu entry is still to do.
- signed updates: an Ed25519 signature over each release's `SHA256SUMS.txt`, checked against release public keys built into the binary, with versions that can only go up. Updates travel as blobs, so they reach sites with no internet. TUF and Sigstore assume a trusted clock and the internet, which a mesh doesn't have. A threshold of offline root keys (2 of 3) signing the release keys is the next step.

## Later

- **Sub-logs by class (change 2)**, if v1 users ask. v1 has one class split out (telemetry). More classes need: a `log` value per class (`app`, `blobidx`) that `append` picks by kind; a `[sync] classes = [...]` setting that `pull_from` checks against each head's `key:log` suffix, the way `telemetry = false` already skips `:telemetry`; checkpoints per class with their own retention; and apps saying which class an event goes in on `/local/v1/append`. The chain format already allows it (`chain_id(key, Some(log))`), so old peers keep syncing.
- **Station and device keys (change 3)**, if v1 users ask. Needs: a station root key made and kept offline (`ridgewarden station-key`), a signed `identity.device` event from the root naming each Pi's key, trust and the club list keyed by the root rather than the device, and revocation of one device without touching the others. Feed trust then checks "device listed by a trusted root and not revoked" instead of "key trusted". The hard part is the offline signing step for non-technical operators, which needs testing with real users first.
- **2-of-3 offline root keys for releases.** Needs three root keys held by different people, a signed `release-keys` document listing the current release keys that any two roots sign, `update` checking that document (shipped in the release and in the binary) before trusting a release key, and a version number on the document so it can only move forward. Worth doing before more than one person can publish releases.
- **Range-based reconciliation (change 6), left on purpose.** Each peer's heads cost about 230 bytes per chain, two chains per station: 6 KB for 13 sites, 90 KB for 200 stations, 0.9 MB at the 2000-author cap, every sync round per peer. Reconciling the heads by ranges (fingerprint ranges of chains, recurse only into ranges that differ, fall back to `/v1/heads` for old peers) only saves bytes for chains that didn't change since the last round. Every station writes telemetry every poll (5 minutes by default) and sync runs every 2 minutes, so about half the chains change between rounds and nearly every range differs; the saving would be small for the added protocol. It pays off once main-log chains are reconciled separately from telemetry chains (which needs change 2's class split) or meshes reach thousands of mostly idle stations. The hard limit today is the 4.5 MB response cap, which `/v1/heads` reaches near 10,000 stations. The drills in `tests/drills.rs` are the test it would have to pass.
- **Bridges:** import Meshtastic/MeshCore message-board history as signed events; an optional Reticulum bridge for unlicensed segments only, disabled on Part 97 RF. These belong in their own programs on the local API, and need the radios to build and test against.
- **Build-a-Pi menu entry:** a change to Build-a-Pi's own repo, using `install.sh`.

## Part 97

- Payloads are plain JSON, signed, never encrypted (97.113(a)(4)).
- Every event names the station that wrote it.
- The token on the local API stays on loopback; nothing secret crosses RF.
- Read-only toward the node; polling no faster than once a minute.

## Risks

- Pruning bugs: Hypercore shipped one that broke replication. Pruning needs the same forced-partition tests as sync. (The drills in `tests/drills.rs` prune under random partitions and check nothing else is lost.)
- Key handling for non-technical hams. Invites and station keys exist to answer this; test them with real club members.
- Disk wear on SD cards. Recommend an SSD for hub sites; keep telemetry retention short by default.

## Success measures

- A new site joins a club mesh in under ten minutes with one invite. (Needs a real club to time.)
- 20 forced partition drills, zero lost events, every site identical afterwards. (Checked on every test run: `tests/drills.rs`, seeded, with forks, flaky links and telemetry pruning.)
- Log size on a 13-site mesh stays under 1 GB per site at default retention. (`ridgewarden status` and the dashboard show size per class and where telemetry levels off; needs a real mesh to confirm.)

## Open questions

- Does the AREDN forum know of any similar agent? Ask before announcing.
- Cross-certify station keys with an LoTW certificate? Attractive, unverified that it's practical.
