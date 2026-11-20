# 10 County-wide passive radar

Repo: `ridgewarden-radar` | Reviewed 2026-11-19 | Status: version 1 built, not yet released; lowest priority

## What it is

Radios at mesh sites detect aircraft by listening for FM broadcast signals bouncing off them. Each site sends only small detection reports over the mesh; a hub combines several sites into tracks on a map. Receive-only.

## Who it's for

University EE and DSP students and faculty, aviation enthusiasts, and the club as a demonstration. Research and education, not operations.

## What exists today

| Project | What it does well | Gap |
|---|---|---|
| blah2 (MIT, about 500 stars) | Real-time delay-Doppler maps on RSPduo, USRP, paired HackRFs or RTL-SDRs, KrakenSDR | Tracker is "future work" |
| adsb2dd | Turns ADS-B data into expected delay-Doppler, for checking detections | Checking only |
| 3lips | Locates targets from several blah2 sites | Early versions matched targets using ADS-B, so it can't track aircraft without it |
| KrakenSDR passive radar | Five coherent channels in one box | 125 kHz bandwidth; passive radar code removed from the main repo; last update 2022 |
| SkyWatch (Harvard) | Multi-site FM radar, up to 150 km | Research system, no open code |

The hobby multi-site stack already exists. What's missing is a tracker that works without ADS-B, transport over a ham mesh with no internet, and polish. This app is integration, not invention.

## What makes ours the best

- **Adopts the existing open tools and their formats** instead of rewriting them, and contributes the missing tracker back.
- **Runs over the mesh,** sending detections only (kilobits, never raw signals or audio).
- **Honest performance page:** detection probability and range per site, checked against ADS-B.

## Changes from the v0.1 spec

1. Built on blah2, adsb2dd and 3lips.
2. Realistic expectations: hobby hardware sees aircraft over tens of kilometres at best; FM gives about 1.5 to 3 km range resolution. ATSC TV signals give better resolution and are worth adding.
3. Compute: a mini PC rather than a Pi 5 until someone shows a Pi 5 keeping up.
4. Priority dropped to last: high cost per site, siting is hard (a site that sees the FM tower well for the reference channel is often bad for the surveillance channel), and little practical value for a club.

## Version 1 scope

Single site at the shack or a home station: RSPduo or KrakenSDR, blah2, ADS-B truth overlay from a club PiAware feed, detection statistics.

Built (labelled experimental): reads blah2's detection, config and timing API; ADS-B truth from dump1090-fa / readsb / tar1090 `aircraft.json` with expected bistatic range and Doppler for blah2's geometry; gated matching per CPI; performance page with Pd by range bin, by aircraft and by hour of day, aircraft seen against the 80% goal, unmatched-detection rate with the Pd chance alone would give, and reliable range over a chosen window; range-Doppler plot of detections against ADS-B tracks; setup page with whether blah2 keeps up with its CPI; detection reports over the mesh as `radar.detections` telemetry and site geometry as `radar.site`; a per-site tracker (Kalman filter in bistatic range and range rate, M-of-N confirmation); multi-site location without ADS-B (least-squares position, velocity and height with error ellipses, association across sites, fused tracks); multi-site accuracy against ADS-B and a count of fused tracks with no ADS-B (the second success measure); a live map with track ellipses, fixes and ADS-B; options to hide aircraft ids and fixes no ADS-B aircraft explains; a `simulate` command serving blah2's API and `aircraft.json` for one or more sites seeing the same synthetic aircraft.

Not done: running against real hardware (an RSPduo or KrakenSDR with blah2 and a PiAware feed at two or more sites), since none is available here; the write-up or paper with a faculty partner (not software); contributing the tracker upstream to blah2 (the owner's call); anything ATSC-specific beyond what blah2 itself supports.

## Later

- detection reports over the mesh; live map
- second site, multi-site tracking without ADS-B
- write-up or paper with a faculty partner

## Part 97

- Receive-only; no license needed to receive.
- 97.113(c): detections only over the mesh, never broadcast audio or signals.
- Don't publish tracking of specific aircraft beyond what public ADS-B already shows.

## Risks

- Direct-path signal from the FM tower swamps the echo; siting decides everything.
- Programme content matters: talk radio and silence weaken detections.
- Hardware cost and compute at remote sites.

## Success measures

- 80% or more of ADS-B aircraft within range detected at one site.
- One aircraft without ADS-B tracked from two or more sites.

## Open questions

- Is there a faculty partner who wants this? Without one, keep it parked.
- Best FM and ATSC illuminator geometry for SLO County.
