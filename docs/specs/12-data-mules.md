# 12 Data mules

Repo: `ridgewarden-mule` | Reviewed 2026-11-19 | Status: version 1 built, not yet released

Built: two islands and a car with message plus file delivery and receipts back by any path; priority classes (urgent, routine, bulk, with per-class file caps and blob priorities); the mule page with waiting, in transit, delivered and corridor times (send to receipt, and the drive by the car's clock, per class, also as JSON); couriers' registered trips and a plan that puts each message on the first departure with room, urgent then oldest first, with an expected arrival time and overdue loads flagged (change 1); a couriers table of registered trips against drives the cars actually logged (the couriers risk); tonight's manifest in the car, loading only its drive's share and letting files go after the receipt; signed pickups and drop-offs naming the control operator (change 3), counted only from pinned cars, with drop-offs checked against keys pinned per island; third-party traffic excluded by default (change 4); the internet-down pitch in the README (change 5); offline-content updates from app 04 carried as library messages; an optional BPv7 bridge through dtnd from dtn7-rs that is never the internal format (change 2); a build guide for the car kit and parking-lot syncs; `scripts/three-cores.sh`, the end-to-end run on simulated meshes.

Not done: the car kit itself and parking-lot syncs (hardware; written up as a build guide only); a test against a running dtnd (the bridge follows dtnd's API and source and is tested against a stand-in); finding couriers and reachable public-parking nodes, and the real message and receipt between two regions with no internet (field work, not software; the corridor times page is where the result shows).

## What it is

Hams' cars carry data between mesh islands that have no radio link between them, for example San Luis Obispo and Los Angeles along Highway 101. A car node syncs with one island, drives, syncs with the other on arrival, and carries replies and delivery receipts back.

## Who it's for

Regions with separate meshes that want to exchange messages, files and offline-content updates when the internet is down or not there. It is also a memorable demonstration for linking projects.

## What exists today

| Project | What it does well | Gap |
|---|---|---|
| AREDN tunnels and supernodes | Link islands over the internet; the normal answer | Useless when the internet is down, which is the case this app is for |
| DakNet | Buses carried data to about 400 villages; it worked because the buses already ran fixed routes | Not ham, not maintained |
| Serval Rhizome | Flood-based store-and-forward | Dormant since 2016 |
| dtn7-rs, uD3TN, NASA HDTN (Bundle Protocol v7) | Standard delay-tolerant networking | Implementations disagree with each other in ESA testing; dtn7-rs is maintained by a small academic team |
| RADIANT (AMSAT-UK) | BPv7 over ham links | Early stage, satellite-oriented |
| LXMF propagation nodes, paper messages | Mature store-and-forward | Encrypted, so not usable on ham RF; messages only |

No one carries AREDN data between islands by car. New.

## What makes ours the best

- **Routes like DakNet, not like Serval.** Couriers register their regular trips (a student driving home on Fridays). The app plans what goes with whom instead of copying everything to every car.
- **Tonight's manifest.** Before a trip, the driver sees what fits on this drive, ranked by priority and age, and how long items have been waiting.
- **Receipts come back by any path.** A delivery receipt is a signed event, so it returns with the next car, over a tunnel if the internet is back, or over a new RF link.
- **Measured corridor times.** A page shows how long delivery actually takes between islands, by priority.
- **Same format everywhere.** Mule transfers use the core's normal sync, limited to the event classes the car should carry, so nothing special is needed at either end.

## Changes from the v0.1 spec

1. Scheduled couriers and a contact plan instead of "carry everything going your way".
2. BPv7 stays an optional gateway, never the internal format.
3. Every transfer records the control operator's callsign, giving an audit trail for automatic-control questions.
4. Anything written by unlicensed spectators is excluded from mule cargo by default.
5. The pitch states plainly that this is for when the internet is down; otherwise tunnels win.

## Version 1 scope

- Two islands, one laptop playing the car (M1 from the original spec), message plus file delivery, receipts flowing back.
- Priority classes: urgent small messages before bulk content.
- Mule page: in transit, waiting, delivered, corridor times.

## Later

- real car kit (compact AREDN node, Pi, SSD, ignition-sensing 12 V power); parking-lot sync near a node
- courier schedules and tonight's manifest
- offline-content (04) updates carried between regions
- BPv7 gateway via dtn7-rs

## Part 97

- 97.221(b) permits automatic control for data on bands 6 m and shorter, which covers AREDN's bands, so a parked or moving car node exchanging data looks permitted. This is a reading, not legal advice; confirm with the AREDN forum or ARRL before running unattended.
- Content must be Part 97-clean: no business traffic, signed not encrypted.
- Third-party traffic (97.115) from non-hams is excluded by default.
- Station identification (97.119) for the car node still needs checking.

## Risks

- Couriers: it only works if hams actually drive the route. Start with people who already commute.
- Short sync windows in parking lots: transfers must resume cleanly, and urgent items go first.
- A node left in a car: the data is public and signed anyway; the car's key is a device key that can be revoked on its own.

## Success measures

- One real message SLO to LA and a receipt back, with no internet used.
- Median urgent-message delivery time on the corridor, published.

## Open questions

- Who drives the 101 regularly?
- Which LA and Bay Area nodes are reachable from public parking?
