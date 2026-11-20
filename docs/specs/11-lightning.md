# 11 Lightning detection without the internet

Repo: `ridgewarden-lightning` | Reviewed 2026-11-19 | Status: version 1 built, not yet released

## What it is

Cheap very-low-frequency (VLF) receivers at mesh sites hear lightning, timestamp each strike against GPS, and send a few bytes per strike over the mesh. A hub locates strikes from four or more sites and alerts when lightning lands near a site or in fire-prone backcountry, with or without the internet.

## Who it's for

County ARES and emergency communications groups, fire-watch volunteers, the club, and weather enthusiasts. Dry lightning starts backcountry fires in SLO County: in northern and central California, 46% of cloud-to-ground strikes have been dry.

## What exists today

| Project | What it does well | Gap |
|---|---|---|
| Blitzortung | 500+ volunteer stations, self-build kit under 300 EUR | Locations are computed only on its central servers; closed code; non-commercial terms; nothing works without the internet |
| vlfrx-tools (abelian.org) | Open source, Pi plus sound card plus GPS; computes arrival times and solves locations; already runs a six-receiver network in India | Toolkit, not a product: no alerting, no fire-watch, no mesh transport |
| HamSCI VLF receivers | GPS pulses fed into a sound-card channel for sample-accurate timing; a proposed six-site network | Research, not deployed as an alert system |
| WWLLN | Global coverage | About 15% detection efficiency; too coarse for a county |
| AS3935 chip boards | Cheap and easy | Distance guessed in 1-4 km steps from signal energy, no bearing, no timestamp: can't locate anything |

Nobody runs an offline, local lightning network with alerts. This fills a real gap.

## What makes ours the best

- **Works with the internet down,** which is when fire weather and outages tend to coincide.
- **Fire-risk alerts, not just dots on a map.** A strike inside a watch area with under 2.5 mm of recent rain (the research threshold for "dry") raises a dry-lightning alert, using rain gauges, CWOP stations or the weather app's data.
- **Every strike shows its error ellipse,** so a fire spotter knows how far to look.
- **Doesn't compete with Blitzortung.** Sites with internet can feed Blitzortung too, so clubs help both networks instead of choosing.

## Changes from the v0.1 spec

1. **Build on vlfrx-tools** (or its data formats) instead of writing receiver code from scratch.
2. **Realistic target: 1 to 3 km** inside the ring of receivers with 4 to 6 sites. Professional networks reach 0.4 to 1 km.
3. **Sites chosen for quiet, not height.** VLF receivers want to be at least half a mile from power lines, ideally miles. Mountaintop radio sites with commercial power are close to the worst case, so plan for remote antennas and hum cancellation, and prefer quiet sites with a mesh link.
4. **Dry-lightning rule made concrete** (under 2.5 mm, with the rain source shown on the alert).

## Version 1 scope

- three or four receivers with GPS timing into a sound card
- on-site trigger and arrival-time extraction (vlfrx-tools)
- strike reports over the mesh as signed events
- hub solver with error ellipses; a map and strike history page
- when the internet is up, compare against public lightning maps and publish the agreement

Built: reading vttoga's TOGA output (file, `-d` directory or command) with a `check-source` command; an optional gpsd fix check, with arrivals dropped while the fix is lost and a warning when arrival times drift from the clock or the residuals show a site's timing is off; arrivals reported as signed telemetry events, each site name tied to its station's key; a solver on a spherical earth with 95% error ellipses from each receiver's timing error (least squares with residuals from four or more sites, exact fits from three only when unambiguous), working off the database lock with a time budget so six or more sites and county-wide storms fit; strikes logged through the core; an offline SVG map with ellipses and the ring of receivers, strike history, per-strike pages, receiver status per site; an Accuracy page with a coverage map of the expected ellipse across the region from the same covariance, the accuracy inside the ring against the 1 to 3 km target, the timing the residuals show, the worst single-site loss and each watch area's worst case; `compare`, which measures agreement with a reference CSV and logs it through the core (the success measure made measurable); `test-alert` for checking delivery with the internet unplugged; GeoJSON and CSV export, with strikes as `lightning.strikes` events other apps (the weather app, the personal weather model) can follow; a simulator for synthetic strikes; and watch areas with alerts and the dry-lightning flag using APRS/CWOP rain gauges over APRS-IS and the weather app's rain events (APRS and NWS) over the mesh.

Not done: automatic comparison with public lightning maps, because no source is both free and documented for this use (Blitzortung has no open API and non-commercial terms; NLDN, Earth Networks and WWLLN are paid; NOAA's GLM is free but only as NetCDF-4 files at about 8 km), so `compare` takes a reference file instead; the Blitzortung feed, because Blitzortung only accepts data from its own receiver kits and firmware; choosing quiet sites and antennas, which is fieldwork; and running against real receivers, which needs the hardware.

## Later

- watch-area alerts and the dry-lightning flag
- six or more sites; county-wide coverage
- export to the weather app and the personal weather model
- optional Blitzortung feed from sites with internet

## Part 97

Receive-only, reports are plain data. No issues.

## Risks

- Electrical noise at sites decides success; budget time for antenna placement.
- Geometry: accuracy falls off outside the ring of receivers.
- Timing must stay in microseconds; check the GPS feed continuously and drop a site's data when it isn't locked.

## Success measures

- Strike locations within 3 km of public maps for local storms.
- An alert delivered over the mesh with the internet unplugged.

## Open questions

- Which sites are electrically quiet enough?
- Confirm vlfrx-tools' license before building on it.
