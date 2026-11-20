# Ridgewarden specs

Design review of the 2026-11-17 spec sheets, done 2026-11-19. Each sheet was checked against what already exists, rewritten around what would make it the best available, and trimmed to a first version. Research covered GitHub, project docs, the AREDN docs and forum, and published papers; anything not found is an absence in search results, not proof. Check the AREDN forum before announcing any "first".

| # | App | Repo | Verdict | Built |
|---|---|---|---|---|
| 00 | [Core agent](00-core.md) | `ridgewarden` | New for AREDN; design closest to Secure Scuttlebutt, plus the lessons it learned the hard way | version 1 |
| 01 | [SDR network](01-sdr-network.md) | `ridgewarden-sdr` | New: no mesh-local SDR directory or amateur VHF/UHF TDOA network exists | version 1 |
| 02 | [Services that survive site loss](02-failover.md) | `ridgewarden-failover` | New; start with a MeshChat-compatible backend | version 1 |
| 03 | [Planner](03-planner.md) | `ridgewarden-planner` | New: nobody calibrates predictions against a live mesh | version 1 |
| 04 | [Offline internet](04-offline-internet.md) | `ridgewarden-library` | Fills a gap: reuses Kiwix and Protomaps, adds mesh-wide replication | version 1 |
| 05 | [Topology game](05-topology-game.md) | `ridgewarden-game` | New: no game uses a real radio network as its map | version 1 |
| 06 | [Service health](06-service-health.md) | `ridgewarden-health` | Overlaps aredn-distributed-monitor; ours leads on multi-site views | version 1 |
| 07 | [Link Doctor](07-link-doctor.md) | `ridgewarden-linkdoctor` | Overlaps aredn-distributed-monitor, which is ahead on analysis depth | version 1 |
| 08 | [Status page](08-status-page.md) | `ridgewarden-status` | Overlaps aredn-distributed-monitor; ours leads on outage-proof and agency-facing | version 1 |
| 09 | [Weather sensor](09-weather-sensor.md) | `ridgewarden-weather` | Research project; reframed because 5 GHz links can't measure rainfall | version 1, experimental |
| 10 | [Passive radar](10-passive-radar.md) | `ridgewarden-radar` | Integration of existing open tools; parked unless a faculty partner wants it | version 1, experimental |
| 11 | [Lightning](11-lightning.md) | `ridgewarden-lightning` | Fills a real gap: offline, local, with fire alerts; builds on vlfrx-tools | version 1 |
| 12 | [Data mules](12-data-mules.md) | `ridgewarden-mule` | New for AREDN; scheduled couriers like DakNet | version 1 |

## Tiers

**Lead with these.** Clearly new or clearly best, and achievable:
- 00 core, with retention, invites and the blob store
- 03 planner, starting with failure what-ifs on the live network
- 04 offline internet
- 11 lightning
- 05 game

**Worth building, but narrower than first written:**
- 01 SDR: directory and signal-strength hunting first; TDOA later, with a realistic 0.5 to 2 km target
- 02 failover: one service (MeshChat-compatible chat) until it works
- 12 data mules: after the blob store; confirm the Part 97 reading first

**Experiments, not products:** 09 weather (a research pipeline on Link Doctor's data) and 10 radar (parked).

## The decision about 06, 07 and 08

aredn-distributed-monitor (one author, started May 2026, very active) already does most of what 07 and 08 do on a single collector. That includes 400-day link history with charts, drift fitting, single points of failure ranked by risk, incidents, a wall display and alerts. It is ahead today. It has no users to speak of yet, so nobody is locked in.

What it lacks is exactly what the ridgewarden core provides: views from several sites, and a log that every site holds through a split. Its multi-collector kit is an unbuilt to-do item.

Three ways forward:
1. **Compete:** catch up on its feature list (the "must match" sections in 06 to 08) and lead on multi-site and outage-proof. Most work, and it splits a small community.
2. **Collaborate:** offer the ridgewarden core as its multi-site backend. Its pages and analysis would run on our signed, partition-tolerant log, and we'd stop building monitoring pages it already has. Least duplicated work.
3. **Narrow:** keep only what's unique in ours: PARTIAL and "why AREDN dropped it" in 06, the workday list in 07, the agency page and signed notices in 08. Contribute or link the rest.

Recommendation: contact the author first (option 2). If that doesn't work out, take option 3, not 1.

## Build order

1. Core v1: retention, invites and the signed trust list, blob store, packaging and signed updates.
2. The 06 to 08 decision above, then whichever version of them follows.
3. 03 planner (what-if first), then 04 offline internet.
4. 11 lightning and 05 game.
5. 01 SDR directory, 02 failover with chat, 12 data mules.
6. Experiments as time allows: 09, then 10 if a partner appears.

## Before building more

- Show the current pages to the local backbone team and the county's emergency communications group, and ask what they need.
- Post on the AREDN forum about the suite and ask about similar work.
- Get a second code review of the core's sync and trust fixes before the first commit.
