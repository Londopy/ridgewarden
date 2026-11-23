# 07 Link Doctor

Repo: `ridgewarden-linkdoctor` | Reviewed 2026-11-19 | Status: version 1 built, not yet released

## What it is

Long-term history of every link's quality, with warnings when a link slowly weakens, keeps dropping or is the only path to part of the network, and a ranked list of what to fix on the next site visit, in plain words.

## Who it's for

Backbone teams and tower crews deciding what to fix next trip, and the LA and Bay linking teams.

## What exists today

| Tool | What it does well | Gap |
|---|---|---|
| aredn-distributed-monitor | 30 days of raw samples, 400 days of hourly summaries (median, p10, p90); charts with weather and change markers; slow drift fitted with an outlier-resistant slope and compared with the link budget; flap detection that ignores drops it caused itself; one-way link alarms; noise-floor baselines; single points of failure ranked by risk, with a "what if this node goes down" replay; storm warnings for rain-sensitive links; antenna aim check | One collector; no ranked visit list; alerts are coded events, with plain language only in some places |
| Mesh Info, mesh-info (archived May 2026) | Per-link graphs | Graphs reset; no alerts; one archived |
| AREDN node pages | Live neighbour table, colour-coded | No history (older firmware kept about two days in RAM) |
| AREDN Prometheus exporter + Grafana | 25 per-neighbour metrics; flexible | Do-it-yourself; no analysis or alerts out of the box |

The competitor is ahead on analysis depth today. Link Doctor's room to lead is narrower than 06's.

## What makes ours the best

- **The next-workday list.** A ranked visit plan that combines single-point-of-failure risk, drift, how many people each fault affects, and groups jobs by site so one climb fixes several things. The competitor has nothing like it.
- **Plain language first.** Every alert reads as a sentence a tower crew can act on ("down 4 dB over the week, check the dish and connectors"), not a code.
- **Every site holds the history,** so a site cut off from the hub still has its own links' past.
- **Calibrated expectations** once the planner (03) exists: "should be 25 dB, measures 18".

## Must match (or we look worse)

- hourly summaries kept 400 days, with median, p10 and p90; raw samples 30 days
- per-link charts with markers for weather, changes and notes
- drift via an outlier-resistant slope (Theil-Sen) since the last radio change, not just two windows compared
- flap detection that ignores drops caused by our own polling
- one-way (asymmetric) link alarms; noise floor against an hour-of-day baseline
- single points of failure ranked by risk; "what if" replay
- the same alert channels as 06

## Changes from the v0.1 spec

1. Drift moves from a two-window comparison to an outlier-resistant slope with a fit quality, measured since the last radio change.
2. Add charts, rollups, one-way and noise-floor alarms to v1.
3. Workday list scoring made explicit: risk times people affected, grouped by site.
4. Weather cross-check before drift alerts, using NWS forecasts where the internet is up, and the weather app (09) where it isn't.

## Version 1 scope

Built: history with hourly rollups (median, p10, p90, plus each end's receive rate and Babel metric) kept 400 days and raw samples 30 days, per-link SNR / quality / noise / receive rate / Babel metric charts with firmware, note and rain markers, Theil-Sen drift measured from the last radio change over pairs of readings from the same hour of day (the seasonal Kendall slope), so a daily swing can't drown out a slow loss, and over at least a week, so a few good or bad days can't pass for one, flaps that leave out drops caused by the polling (failed or partial polls, LQM tracker hiccups the other end contradicts, radio work), loss with 4.x stale-tracker handling and a two-poll hold-down, one-way and noise-floor alarms, single points of failure ranked by risk with a What if page and a standing alert for each one the app can be sure of once it has lasted an hour (judged on the links up now; one there only while another link is down goes in that link's loss alert; `spof_sites` sets how many sites a failure must cut off), workday list scored as risk times sites affected, grouped by site, with a three-day forecast per site, partial-picture warning, alerts to ntfy, Discord, webhooks and email.

Built (beyond v1): the weather cross-check before drift and aim alerts (NWS observations and hourly forecasts from api.weather.gov, and wet-antenna, front or ducting candidates from app 09's weather.candidate events in the log, with its /api/events as a fallback), antenna aim checks with the step drawn on the chart, firmware upgrade planning (far end first, from this site), planner calibration on link pages, link summaries and the workday list (reads the planner.prediction events the planner publishes), and a track record page for the success measures (lost links warned about first, problems cleared after a noted visit).

Not done: nothing has run on real AREDN nodes, radios or a Pi in the field, only the simulator and tests, so email delivery and the thresholds are untried on real backbone data; the success measures need a real mesh over time; the open questions (aredn-distributed-monitor, NewMeshMap) are for those projects' owners.

## Later

- planner calibration in summaries (built)
- firmware upgrade planning (upgrade the far end first) (built)
- antenna aim checks (built)

## Part 97

None.

## Risks

- Weather fading mistaken for drift: cross-check before alerting.
- Firmware differences in the data: covered by the tolerant parser and simulator.

## Success measures

- Catches a weakening link before it fails, at least once.
- A tower crew plans a trip from the workday list.

## Open questions

- Work with aredn-distributed-monitor rather than compete on analysis depth?
- Would NewMeshMap's author want this data feeding it?
