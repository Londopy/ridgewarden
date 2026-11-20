# 05 Radio-topology game

Repo: `ridgewarden-game` | Reviewed 2026-11-19 | Status: version 1 built, not yet released

Built: Capture the Site with teams, LAN check-ins countersigned by the site's core, chains scored from live links with decay, builder bonus (days seen, 90-day window), diminishing repeat captures, encounters credited to both sides, travel-time limits with a warning-first three-strike ladder, live map and leaderboard, admin start/end/teams; rotating on-air code words for public sites (admin-seeded, revealed by signed events); fox hunts proven by fox code words, with bearings shared as spots; spots; seasons and a season archive; POTA-style awards; a numbers page for the success measures; a per-game debrief of dropped links; `safe_sites` so only agreed sites can be in a game; incremental scoring for long games.

Not done: driving a transmitter (the app writes the code word schedule; sending it on air is people or a fox controller), the "new ham got licensed" measure (ask them), and the open questions (which nodes are safe, name and art), which are decisions, not software.

## What it is

A territory game played on the real mesh. Sites are territory, live RF links are the roads between them, and a team scores for holding chains of connected sites. Territory only holds while its links stay up, so keeping the network healthy is how you win. Building a node that adds a new link earns the biggest bonus.

## Who it's for

Clubs that want new members and a reason for people to learn their network: club meetings, Field Day, school and Scout events, ARES teams wanting a network-awareness exercise.

## What exists today

| Project | What it does well | Gap |
|---|---|---|
| POTA / SOTA | Points, awards, spotting; the activator's log gives the hunter credit. Hugely popular. | Not about network infrastructure |
| ARRL Volunteers On The Air | Points per contact from LoTW uploads | Event-based, no network play |
| ARDF / fox hunts | The established radio sport | Scoring is manual or orienteering tools; no persistent game |
| TECP (airsoft on Meshtastic) | Capture points on a Pi, reverting to neutral on a timer | Airsoft, unlicensed radio, not a real topology |
| Ingress / Pokemon Go | Proven territory design: decay, factions, anti-cheat | Commercial, GPS-based, nothing to do with radio |

No game was found whose map is a real radio network. This one is new.

## What makes ours the best

- **Decay is real link health, not a timer.** Ingress decays territory about 15% a day to force upkeep. Here a chain scores only while its RF links are up in the mesh's own data, so upkeep means real maintenance.
- **Builder bonus that can't be faked.** A new link scores only after it has shown up in the mesh's link data for several days.
- **Proof of presence that's stronger than GPS.** A check-in has to come from the site node's own LAN and is countersigned by that site's core. Travel-time limits between sites catch impossible hops.
- **POTA's good habits.** Spotting, credit for both sides of an encounter, and points weighted so play spreads across many sites instead of piling onto two.

## Changes from the v0.1 spec

1. Decay tied to live links (above) instead of an hours-based timer.
2. Builder bonus requires the link to persist for N days in link data.
3. Fox-hunt finds proven by a signed beacon code word, not a typed string anyone could share.
4. Anti-cheat: travel-time limits between check-ins, and a warning-first, three-strike ladder. Niantic found over 90% of cheaters stopped after a first warning.
5. Spectators only read signed game events and never transmit anything, which keeps them out of 97.115 third-party questions.

## Version 1 scope

"Capture the Site" for one club meeting:
- teams, check-in at a site (LAN-origin plus site countersignature)
- chains scored from live links, decaying when a link drops
- live map and leaderboard page, web spectators
- admin: start and end a session, set teams

## Later

- seasons, fox-hunt integration (a club's annual hunt), builder bonus
- rotating on-air code words for check-ins at public sites
- POTA-style awards and a season archive

## Part 97

- No prizes of monetary value (97.113(a)(3)).
- RF players are licensed; spectators never transmit.
- Everything is signed and unencrypted.
- Frame it as training and network familiarisation. Ask the AREDN forum and the club before running it publicly.

## Risks

- Site access: many sites are on private land. Only club-controlled or publicly safe nodes can be check-in points; the config marks which.
- Community reception: some hams may see a game as frivolous. Pitch it as recruiting and network training, and show it at a club meeting first.

## Success measures

- 10 or more players in one session.
- At least one new ham says it's part of why they got licensed.
- At least one new node built because of the game.

## Open questions

- Which nodes are safe and legal to visit?
- Name and art style.
