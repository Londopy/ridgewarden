# Changelog

All notable changes to ridgewarden are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). The section for a version is
what appears on its GitHub release page. Validated in CI by
[patchnotes](https://github.com/Londopy/patchnotes).

## [Unreleased]

### Added

- Node poller: reads `/a/sysinfo` from the AREDN node on the LAN (read-only, 60 s minimum
  interval), parses 3.x and 4.x response shapes (ignoring 4.x neighbors that are listed but
  disconnected), and keeps raw snapshots in SQLite.
- "What changed" report comparing the node's view now with 24 hours ago (`ridgewarden diff`
  and the dashboard's Changes page).
- Signed append-only event log: Ed25519 signatures over BLAKE3 ids, per-author hash chains,
  Lamport clocks, fork detection and key revocation.
- Pull-only gossip sync between agents with fork and digest repair, so every agent ends up
  with the same events; peers from config or discovered from verified `ridgewarden` adverts.
- `[sync] telemetry = false` skips other stations' polls and probes, for a data-mule car.
- Trusted keys (`[trust]`): apps believe only this station and pinned club keys.
- Local API for apps: an event feed, `whoami`, and a token-protected, loopback-only append.
- Optional crawl of other backbone nodes for their links.
- `ridgewarden-client` crate for building apps on the core, with shared alerts (ntfy,
  Discord, webhooks, acknowledge links, quiet hours, maintenance windows, silences) and
  server-drawn SVG charts.
- `ridgewarden-sim`, a fake AREDN mesh for testing without touching real nodes.
- Telemetry log per station with signed checkpoints, so polls and probes are pruned
  everywhere after 30 days (`[retention]`) while every site still agrees on what it holds.
- `ridgewarden invite` and `ridgewarden join`: one signed line to bring a site into a club.
- Club trust list as a signed, versioned event (`ridgewarden trust add/remove/list`), and
  `[trust] blocked` for stations a site refuses to store.
- Blob store: files shared over the mesh in verified 1 MiB chunks, resumable, rate-limited
  and limited to a time window (`ridgewarden blob`, `[blobs]`), fetched in priority order,
  and open to apps through the local API (`/local/v1/blob/add`, `want`, `export`).
- `ridgewarden update`: signed releases from GitHub or from the mesh, installed only after
  confirmation; `verify-release`, `release-key` and `release-sign` for checking and making
  releases.
- Downloads: static Linux builds for x86_64 and Raspberry Pi, `.deb` packages, a Docker
  image, `install.sh`, and signed `SHA256SUMS.txt`.
- `ridgewarden invite` also prints the invite as a QR code (its own ISO/IEC 18004 encoder,
  levels M and L), and the dashboard has an invite page with the code as SVG, shown only
  on the machine itself.
- `ridgewarden status`: whether the agent is running, when the node last answered, how
  each peer's last pull went, and disk use per class.
- Disk use per class (main log, telemetry, blobs, database file, exports and imports) on
  the dashboard, with the main log's yearly growth and where telemetry levels off at the
  configured retention.
- `ridgewarden blob unwant`, `POST /local/v1/blob/unwant` and `CoreClient::blob_unwant`:
  stop fetching or keeping a blob and delete its chunks, keeping any chunk another wanted
  or held blob shares.
- `GET /v1/blobs?root=` and `CoreClient::blob_status`: one blob's state for apps waiting on
  it; `GET /v1/peers` for the peers' last pull.
- Preferred sources for a blob: `POST /local/v1/blob/want` takes `peers`, an ordered list
  of core addresses asked for its chunks first (`CoreClient::blob_want_from`,
  `ridgewarden blob want --from`). Others are asked only for chunks those don't have, or
  after one fails; a failed peer sits out the rest of the fetch round. With `only`
  (`blob_want_only_from`, `--only`) no other peer is asked at all.
- `[blobs.peer_rate_kbps]`: a lower rate for chunks fetched from a given peer, for slow
  links, under the overall `rate_kbps`.
- `node.poll` events carry `trackers` (the node sent 4.x link-quality trackers) and
  `stale_skipped` (trackers left out because their neighbor went quiet).
- `install-all.sh`: the core and any of the twelve apps in one go, each app's release
  checked with the core's release key before its installer runs.
- 20 seeded random partition drills in the tests (several sites, flaky links, cloned keys,
  telemetry pruning), checking that every site ends identical with nothing lost.

### Changed

- Files apps hand over in `imports` must be plain files (links are refused), exports are
  written as new files that never follow a link, and the station key is set back to
  owner-only on every start, so an app in the agent's group can't reach the key that way.
- The app token (`local.token`) is readable by the agent's group, and `imports` and
  `exports` are group-writable, so each app runs as its own user in that group: it can
  sign events through the core but can't read the station key.
- Peer addresses are kept in one spelling (lower-case host, no default port), so a
  per-peer rate or preferred peer matches however it was written.
- A blob fetch round stops after about a minute even when slow links are capped, and a
  chunk's transfer time counts toward its pause, so capped links get close to their cap.

- `GET /v1/blobs` lists complete and wanted blobs first, so manifests nobody wanted can't
  push them out of its 1000 entries.
- Links in the shared page style have readable colors in light and dark mode, and focused
  controls show an outline.

### Fixed

- Blob chunk fetches no longer follow redirects, so a peer can't point this core at
  another address.
- A blob fetched from a trusted station (by `auto_want` or wanted by hand) is fetched by
  that station's manifest, even when someone else's manifest for the same file arrived
  first; manifests with an impossible size or index length are ignored.
- A chunk found damaged during an export sends the blob back to fetching instead of
  leaving it marked complete.

[Unreleased]: https://github.com/Londopy/ridgewarden/commits/main
