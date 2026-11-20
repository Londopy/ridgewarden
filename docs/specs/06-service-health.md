# 06 Service health

Repo: `ridgewarden-health` | Reviewed 2026-11-19 | Status: version 1 built, not yet released

Built: everything in "Must match", "Changes from the v0.1 spec" 1 to 3, "Later" and the "Risks" mitigations: multi-site probing and all six states, thresholds, the cut-off check, uptime with rollups, service pages, status and keyword checks, maintenance and silences, email/ntfy/Discord/webhook alerts with acknowledge links and quiet hours, push heartbeats shared as signed events, AREDN's full pass rule (HTTP 200/401 with redirects, TCP within 2 s, then UDP) with the reason and fix, Winlink prompt and MeshChat API checks, per-owner alert routing, a cap on sites per service, per-service overrides, and the page counting stale and wrongly unpublished services found.

Not done: the AREDN 4.26 on-node service watch (change 4; needs a package on every node, and the spec makes it conditional); measuring whether an alert beat a user's report (the app can't know when people noticed; the page says so); the open questions (decisions for people, not software).

## What it is

Every site probes every service the mesh advertises and logs the results through its core, so each site sees every site's results. A service that answers from one site and not another is a link problem, not a service problem, and the page says so.

## Who it's for

Service owners (Winlink, chat, cameras, PBX), mesh admins, and served agencies through the status page.

## What exists today

| Tool | What it does well | Gap |
|---|---|---|
| aredn-distributed-monitor | HTTP and TCP checks per advertised service; alerts only when the host node is still up; 400 days of daily tallies; uptime for 24 h, 30 d and 1 y; response-time charts; silence or ignore with a period | One collector, so no view from several sites; no detection of services AREDN unpublished but which still work; keyword checks still on its to-do list |
| AREDN's hourly check | Built into every node | Drops working services (blocked ping, unreachable forwarded ports) and keeps dead ones listed for an hour or more; says nothing about why |
| Uptime Kuma, Gatus | Keyword and status checks, thresholds, maintenance windows, badges; Gatus skips checks when the monitor itself is cut off | Not mesh-aware, single location |

## What makes ours the best

- **Seen from every site.** PARTIAL ("works from Shack and Lopez, not from Paso") exists only here. It is also the multi-collector feature the competitor has planned but not built.
- **Explains AREDN's own decisions.** The node's hourly check only accepts HTTP 200 or 401. When a working service gets unpublished, the page says why ("returns 404", "ping blocked by Windows firewall") and how to fix it. Nobody else does this, and the forum has threads asking.
- **Keeps working through a split,** with every site's results on every site.

## Must match (or we look worse)

- failure and success thresholds before a state changes (Gatus defaults: 3 failures, 2 successes)
- skip all verdicts when our own site is cut off, rather than calling everything DOWN
- uptime for 24 h, 30 d and 1 y; response-time charts; a recent-checks bar
- keyword and status-code checks per service
- maintenance windows and silencing with a reason and a review date
- push heartbeats for things that can't be probed
- alerts: email, Discord, ntfy and webhooks, with acknowledge links and quiet hours

## Changes from the v0.1 spec

1. Mirror AREDN's own pass rule (200/401, TCP then UDP) and report the reason the node would drop a service.
2. Add the "must match" list above to v1, not later.
3. Protocol-specific checks: a real Winlink (CMS) handshake and MeshChat's API rather than "port open".
4. Watch service changes using AREDN 4.26's on-node service watch API if a node package becomes worthwhile; until then, poll.

## Version 1 scope

Built: multi-site probing, UP / PARTIAL / DOWN / STALE / UNPUBLISHED-BUT-ALIVE / UNKNOWN, timelines, JSON API, failure and recovery thresholds, the self-cut-off check, uptime for 24 h / 30 d / 1 y with daily rollups, a service page with a response-time chart and recent checks, status-code and keyword checks, maintenance windows and silences with review dates, alerts (ntfy, Discord, webhooks, acknowledge links, quiet hours), and the "why AREDN dropped it" explanation.

Also built since: email alerts (through the box's sendmail), push heartbeats, and everything under Later.

## Later

- protocol checks for Winlink and MeshChat
- push heartbeats
- per-owner alert routing

## Part 97

None beyond plain, signed, polite traffic.

## Risks

- Probe load as the number of sites grows: cap sites per service.
- Services advertised as HTTP that aren't: per-service overrides.

## Success measures

- Finds at least one stale and one wrongly unpublished service on the first mesh it runs on, in the first week.
- A service owner hears about an outage from an alert before a user reports it.

## Open questions

- Work with aredn-distributed-monitor (see the review summary) rather than compete?
- Report wrongful unpublishing upstream to the AREDN developers with data?
