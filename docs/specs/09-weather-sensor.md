# 09 Mesh as a weather sensor

Repo: `ridgewarden-weather` | Reviewed 2026-11-19 | Status: version 1 built, not yet released

## What it is

AREDN links between mountaintops measure signal strength around the clock. Weather changes those numbers. This app records them, removes the normal daily pattern, and looks for weather effects: wet antennas during rain, and marine-layer ducting and fading along the coast. It needs no new hardware.

## Who it's for

Weather enthusiasts, local forecasters, researchers, and Londopy's personal weather model. It's a research project first; it may produce a negative result, which is still worth publishing.

## What exists today

| Project | What it does well | Gap |
|---|---|---|
| OpenSense (EU COST action, 2021-2025) | Rainfall from commercial microwave links; open tools (pycomlink, poligrain) and data standards | Works at 10-40 GHz, where rain is strong; nothing on ham networks |
| NYC Mesh OpenMesh dataset | A community network used as a rain sensor; 103 links | Rain mostly visible on 60-70 GHz links; at 5 GHz, effects were small and mostly wet-antenna loss |
| 5 GHz rain studies (Ecuador, Japan) | Measured 5 GHz links in rain | Path rain loss too small to use; wet-antenna loss was the visible effect |

Doing this on a ham mesh is new. Detecting ducting from link signal levels wasn't found in any paper.

## What makes ours the best

- **Honest about the physics.** At 5 GHz, rain itself costs about 0.01 dB/km at 10 mm/h, too little to map rainfall. The app doesn't pretend; it measures what is detectable.
- **Two signals that are real:** wet antennas (a few dB when rain hits the dish or radome) as a "rain on site" detector, and marine-layer ducting and afternoon fading, which the California coastal inversion is known to cause.
- **A public ham-network dataset** in OpenSense format: the first of its kind.

## Changes from the v0.1 spec

1. Reframed from "estimate rainfall along each link" to "rain-on-antenna detector plus a marine-layer fading index".
2. Not an app clubs install. It runs on top of Link Doctor's data at one hub, as a research pipeline with a results page.
3. Ground truth: CWOP and NWS rain, plus marine-layer depth from upper-air soundings (Vandenberg, if available).

## Version 1 scope

- poll every backbone link once a minute (via the core's crawl), store SNR and noise
- remove the daily cycle per link
- join with CWOP/NWS rain and soundings; plot events
- export the dataset in OpenSense format

Built: link levels (RSL, noise, SNR) from the core's polls at whatever sampling the crawl gives, hourly medians kept 400 days and raw samples 90; a per-link hour-of-day baseline (median over 21 days) and residuals; continuous detection every 10 minutes of wet-antenna, widespread-drop (a front, instead of being discarded) and ducting/fading candidates by explicit, tunable rules, kept with their evidence; rain from APRS-IS (CWOP) and NWS observations, lightning strikes from ridgewarden-lightning, soundings from the University of Wyoming archive with inversion and refractivity gradient; wind from APRS reports kept with each reading, with candidates flagged where nearby stations report gusts of 30 mph or more (wind moving a dish, the Risks item); a live marine-layer indicator with evidence and confidence; candidates published as signed weather.candidate events (the /api/events shape) and fetched rain readings as weather.rain telemetry; a results page testing the success measure (rain against dry hours, night against day, with 95% intervals); a residual chart per link with rain, sounding and candidate markers, link lengths, events and methods pages; export as a NetCDF classic file in the OpenSense CML conventions. Needs real backbone data (a hub core crawling every 60 s, agreed with the node owners) and a week of history before anything is found.

Not done: the paper (not software); feeding the personal weather model (no such app; candidates are published as events instead); AREDN Prometheus metrics as a cheaper source (needs the core to poll them); publishing the dataset publicly (a person uploads the file the app writes).

## Later

- automatic event detection and a live "marine layer" indicator
- feed into the personal weather model
- paper with a faculty advisor

## Part 97

None: it only reads measurements the nodes already produce.

## Risks

- The effects may be too small to separate from equipment temperature, rate changes and dish movement in wind.
- Polling every link each minute is more load than Link Doctor needs; agree it with node owners or use AREDN's Prometheus metrics where cheaper.

## Success measures

- A statistically clear effect (wet antenna, ducting or fading) on at least one link class, or a well-documented negative result.
- The dataset published.

## Open questions

- Which SLO links are longest and best placed across the marine layer?
- Are Vandenberg soundings available in a usable form?
- Faculty advisor in EE or atmospheric science?
