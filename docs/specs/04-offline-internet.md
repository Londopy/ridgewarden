# 04 Mesh-wide offline internet

Repo: `ridgewarden-library` | Reviewed 2026-11-19 | Status: version 1 built, not yet released

## What it is

Wikipedia, regional maps and National Weather Service alerts copied to sites across the mesh, refreshed whenever any site has internet. When the internet goes down, every site still has maps, reference material and the latest alerts.

## Who it's for

County ARES and RACES, shelters, emergency operations centres and field teams during outages.

## What exists today

| Tool | What it does well | Gap |
|---|---|---|
| Kiwix (kiwix-serve) | The standard offline Wikipedia; active; every file published with piece hashes and a torrent | One box at a time; no syncing between boxes |
| Internet-in-a-Box | Mature bundle of Kiwix, maps and learning content on a Pi | No box-to-box sync found |
| Kolibri | Real peer-to-peer sync of user data | Content itself isn't synced |
| Protomaps PMTiles | One-file map tiles; `pmtiles extract` cuts a region | Download only |
| Guifi.net Cloudy (2015) | Syncthing and distributed storage across a community network | Dormant |

Nobody keeps offline content replicated across an AREDN mesh. We reuse Kiwix and Protomaps rather than rebuild them; the replication is new.

## What makes ours the best

- **Verified end to end.** Kiwix publishes a SHA-256 for every file (and per-piece hashes); Protomaps publishes BLAKE3 for whole planet builds only. The fetcher checks each download against the publisher's hash and signs it into the library event; every site checks its finished copy against that hash, and the core checks every chunk. A mirror without internet trusts the fetcher for the publisher's hash; requiring two fetchers to agree (mirror.confirmations) removes that single point of trust. Regional map extracts have no publisher hash.
- **Each radio hop carries a chunk once.** Copies fan out along a tree of the best links, ranked by the weakest measured link on each path from the links the cores poll, at off-peak hours (the core's blob window), at a rate per link that the operator sets in the core (`[blobs.peer_rate_kbps]`).
- **Alerts first, always.** NWS alerts are small and urgent; they jump every queue and reach all sites in under a minute.
- **One catalog page** shows which site holds which version of what, with the "as of" date on everything.
- **The old copy keeps serving** until the new one has fully arrived and checked out.

## Changes from the v0.1 spec

1. Sizes updated: English Wikipedia without pictures is now 49 GB (mini 13 GB, full 119 GB). At 20 Mbps that is about 6 hours of airtime per hop. Tiers matter: small sites hold mini, hubs hold nopic.
2. Reuse publisher hashes as the chunk manifest.
3. Topology-aware fan-out instead of every site pulling from wherever.
4. NWS done the way the API actually behaves: poll the GeoJSON list by area with If-None-Match and a User-Agent, fetch CAP XML per new alert, use the alert id to avoid duplicates, and have one fetcher per partition so sites don't multiply load.

## Version 1 scope

- one site serving Wikipedia (mini), SLO County PMTiles and an alerts page
- automatic mirror to a second site through the blob store
- catalog page with freshness

Built: everything above, and from Later and the rest of this sheet: tiers and groups (nopic at hubs, mini at small sites) for any number of sites, topology-aware ordering of copies from the polled link graph, documents as a content kind, new versions handed to the data-mule app with trips shown, publisher-hash checks with optional confirmation by a second fetcher, alerts first (small events ahead of bulk blobs) with delivery time measured at every site against the one-minute goal, one fetcher per partition with failover when it goes quiet or loses its internet, the old copy kept until the new one checks out (kiwix-serve falls back to the last working files), dates and stale-copy warnings on every page, disk budgets and free-space checks, monthly read-back of served files against SSD failure, attribution pages, copies fanned out along the best-connected tree: each site waits for a library site on its best path to a copy (weakest measured link, then hops) and fetches first from the holders with the best path, through the core's preferred-peer list, and per-link rate caps through the core's `[blobs.peer_rate_kbps]`.

Not done: setting per-link rates from measured link quality on its own (operators set them); fetching from the nearest holders only (they are asked first, and other peers fill any gap, so a copy never stalls on one site); publisher piece hashes as the chunk manifest (Kiwix's 4 MiB SHA-1 pieces can't be the core's 1 MiB BLAKE3 chunks, so the whole-file hash is checked instead); CAP XML per alert (the GeoJSON already carries the CAP fields); a browser map viewer (would need third-party JavaScript); the open questions, which are for the people running the mesh.

## Later

- tiers across four or more sites; nopic at hubs
- topology-aware fan-out and per-link rate limits
- forms, plans and phone lists the served agencies ask for
- updates carried between regions by data mules (12)

## Part 97

- AREDN norm: a server may fetch weather and similar data and make it available on the mesh; general browsing is out of scope. This is a disaster fallback, not an ISP replacement (97.113(a)(5)).
- Plain HTTP over RF, no TLS. No music or video.
- Wikipedia (CC BY-SA) and OpenStreetMap (ODbL) allow redistribution with attribution; attribution pages are included.

## Risks

- Bulk transfer time and politeness: never crowd out emergency traffic.
- Stale content mistaken for current: dates on every page.
- SSD wear and failure at remote sites.

## Success measures

- Content usable at a site with the internet unplugged.
- Alerts reach all sites within one minute of any fetcher seeing them.

## Open questions

- Which sites can host an SSD?
- What other offline content do served agencies want?
