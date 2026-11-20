# 08 Status page

Repo: `ridgewarden-status` | Reviewed 2026-11-19 | Status: version 1 built, not yet released

## What it is

"Is the mesh up?" answered at a glance for served agencies: green, yellow or red per site and key service, one headline in plain words, an incident notice, and a page that prints on one sheet. It's a read-only view of 06 and 07, so the three never disagree.

## Who it's for

Emergency operations centre staff, ARES and RACES leadership, county emergency managers and club leadership. People who have never heard of Babel.

## What exists today

| Tool | What it does well | Gap |
|---|---|---|
| aredn-distributed-monitor | Agency tiles with plain-language problems; wall display; static HTML export; printable club report; badges; incidents with open, monitoring and resolved states; pinned notes; maintenance windows; offline field mode | Written mostly for hams; one collector, so it goes stale if that box is cut off; no signed notices; no subscriptions for agencies; no public incident history |
| Atlassian Statuspage, Instatus | The standard for status pages: incident stages (investigating, identified, monitoring, resolved), subscriptions, maintenance notices | Internet services, not a radio network |
| AREDN's alert banner | A yellow notice on every node page, with optional local repositories for meshes without internet | One message for the whole mesh, not a status page |

## What makes ours the best

- **Every site serves it,** so an EOC can open the nearest site's copy even when the mesh is split, and each side shows the other as cut off rather than going stale.
- **Notices carry a signature,** so staff know which ham posted them, and the notice can also go out on AREDN's own banner feed so it shows on node pages.
- **Written for agencies:** one headline, no jargon, "unknown" instead of a hopeful green, and a print layout for briefings and binders.
- **Incident history** in the Statuspage style, so a briefing can show what happened and when.

## Must match (or we look worse)

- incidents with stages and a timeline, one notification per incident
- maintenance notices
- wall display mode
- embeds and badges; a static HTML fragment for club websites
- offline caching for phones in the field

## Changes from the v0.1 spec

1. Ask the agencies first. The original spec said so, and no evidence was found of what EOCs want from a network display. Show the county's emergency communications group the current page and ask before polishing.
2. Incidents and their history added to v1.
3. Publish notices through AREDN's local alert-message feed as well as on the page.
4. Optional read-only internet mirror, behind Anubis per Londopy's hosting conventions.

## Version 1 scope

Built: site and service tiles, headline from the mesh's own reachability list, signed notice, print layout, JSON, incidents with stages and a 90-day history (one alert per incident), maintenance notices, a wall display, a badge and an embeddable fragment, the AREDN banner feed (`<node>.txt`, group files and `all.txt` for `aredn.@alerts[0].localpath`), offline use (a saved one-file copy everywhere, and a service worker on https), and the Later items: subscriptions for agency contacts (email, email-to-SMS gateways, webhooks), a map view, and the internet mirror (static files pushed off the mesh, with an Anubis example). An about page counts views as the nearest measurable sign of use.

Not done: agency feedback, which is people asking agencies, not software; and the success measure itself, which no app can observe (the view counts stand in for it). Service workers need https, so on the mesh's plain-http pages offline use is the saved copy only.

## Later

- subscriptions (email, SMS gateway where available) for agency contacts
- map view
- internet mirror

## Part 97

The page reports network status; it is not a channel for agency business traffic.

## Risks

- A wrong green erodes trust fast; the page prefers "unknown".
- Building what we think agencies want instead of what they want: ask first.

## Success measures

- The county's emergency communications group or the County references it in an exercise or briefing.

## Open questions

- What do the agencies actually want to see?
