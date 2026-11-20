# 03 Mesh planner and digital twin

Repo: `ridgewarden-planner` | Reviewed 2026-11-19 | Status: version 1 built, not yet released

## What it is

A model of the mesh that combines terrain, predicted radio paths and the real measured links. It answers "what breaks if this site dies?" and "where should the next node go?", and it gets more accurate the longer it runs, because every real link corrects its predictions.

## Who it's for

Backbone teams, the SLO-to-LA and SLO-to-Bay linking efforts, and grant writers who need numbers about redundancy.

## What exists today

| Tool | What it does well | Gap |
|---|---|---|
| Radio Mobile | The planning tool AREDN's docs recommend; Longley-Rice; free for hams | Windows, link-by-link, knows nothing about the live network |
| CloudRF | Polished, GPU multisite modelling, "best site" search, calibration from drive tests (4-8 dB error) | Commercial (best-site search from £144/month); calibrates from drive tests, not network telemetry; no failure what-ifs |
| Signal-Server forks (loorisr, W3AXL) | Open ITM engine, Copernicus terrain, fast multithreaded mode | Engines, not planners |
| Meshtastic Site Planner | Browser-based, multi-site, exports GeoJSON | LoRa only; validated against older software, not field data |
| Mesh Community Planner | "Critical node" detection, resilience metrics | LoRa only; non-commercial license; no calibration or site ranking |
| HeyWhatsThat | Quick path profiles | No API found; single paths |

Nothing found combines the live AREDN network with terrain predictions, calibrates predictions against what the links actually measure, or ranks new sites by the redundancy they add.

## What makes ours the best

- **Calibration is the main feature.** Propagation models are weakest exactly where hams need them: NTIA's own comparison found poor agreement with measurements. Every live link becomes a free measurement. The planner fits a correction per link and per region and shows predictions as "18 dB ± 4, from 300 samples".
- **Failure what-ifs on the real network.** Kill any site or link and see which sites and services lose contact, using the same graph Link Doctor uses.
- **Sites ranked by redundancy, not coverage.** A candidate hilltop scores by how many single points of failure it removes, using CloudRF's "best site" idea aimed at resilience.
- **Works offline.** Terrain tiles for the region are fetched once and shared across the mesh.

## Changes from the v0.1 spec

1. Calibration moved from a feature to the centre of the design.
2. Terrain: Copernicus GLO-30 is a surface model (it includes trees and buildings) on the EGM2008 datum. GPS antenna heights are ellipsoidal and must be converted. These corrections are explicit in the design.
3. Engine: wrap NTIA's official ITM code (C++, via FFI) and cross-check against the loorisr Signal-Server fork; no Rust port of ITM exists.
4. Crawl: slow and from one hub, using the core's crawl, per AREDN's guidance on polling load.
5. Ship the what-if first (it needs no terrain), then predictions, then calibration and ranking.

## Version 1 scope

Built: live graph from the core's polls and crawl (Link Doctor's link analysis, reused), a map from node positions with no JavaScript or tiles, single points of failure with what each cuts off, interactive "kill any sites and links" by clicking the map or a form, a plain-language impact report (sites that lose contact, which still reach each other, advertised services that become unreachable from here, copies of the replicated chat from 02 left reachable), GeoJSON export; terrain from SRTM-format tiles shared once over the mesh through the blob store, with surface-model and geoid/ellipsoid corrections explicit (antenna heights above the surface, GPS altitudes through a configured geoid height); ITM point-to-point ported from NTIA's reference code and checked against its example cases; per-link calibration against measured signal with a correction per region and mesh-wide, a outlier-resistant spread as the error bar, and leave-one-out error before and after shown on the page; corrected predictions for candidate links, with links predicted unworkable left out of the score; candidate ranking with filters for access, power, lease and owner; hilltop search; PDF report.

Not done: fetching terrain from the internet inside the app (tiles are downloaded by hand and imported, because adding an HTTPS client was refused in this pass); a cross-check against the Signal-Server fork (not available here; NTIA's own cases are the reference instead); "the backbone team uses it to choose a site" is a human outcome no software can record.

## Later

- terrain plus ITM predictions for the SLO-LA and SLO-Bay corridors
- per-link calibration with uncertainty; corrected predictions for candidate links
- candidate hilltop ranking with filters for access, power, lease and owner
- PDF report for grant applications
- tie-in with 02: "service Y loses its replicas if site X fails"

## Part 97

None.

## Risks

- Terrain for the corridors is tens of GB; store it once at the hub and share through the blob store.
- ITM is undefined below about 1 km and optimistic in clutter; calibration and honest error bars are the answer.
- The real limits are human (site access, leases, power); the ranking needs those as inputs, not just radio.

## Success measures

- The backbone team uses it to choose or justify at least one new site.
- Prediction error for real links shrinks after calibration, and the page shows by how much.

## Open questions

- Where do site attributes (owner, lease, power) live today? A club wiki?
- Link with the separate nexus digital-twin research project?
