# 02 Services that survive site loss

Repo: `ridgewarden-failover` | Reviewed 2026-11-19 | Status: version 1 built, not yet released

Built: MeshChat-compatible chat on the core's log (stock client works unchanged; no cap, no full-database polling, survives reboot), files on the blob store with copies at every site, a status board merged as a document, per-service merge and rejoin rules, liveness from heartbeat counters and mesh link quality (clock errors tolerated), split shown on every page, directory redirect on every Pi, placement by connectivity, power and weight with fail-back hysteresis, three or more sites, a bridge to MeshChat nodes, and a `drill` command that measures the success criteria and records them at every site.

Not done: Winlink via Pat (the spec makes it conditional on a club asking); Asterisk and cameras (listed as unable to move); the drill with a club on real mountaintops (not software; the `drill` command is the tool for it); drills on real Pis over RF (run only against the simulator so far).

## What it is

Club services (chat, file drop, maps) keep running when the site hosting them loses power or the mesh splits. Both sides of a split keep serving, and their data merges when the link comes back.

## Who it's for

Served agencies, ARES and RACES teams, and AREDN groups running shared services in SLO, LA and the Bay Area. "The server was on the mountain that lost power" is a classic emergency-communications failure.

## What exists today

| Tool | What it does well | Gap |
|---|---|---|
| AREDN's own guidance | Describes manual standby servers synced with rsync | Manual; AREDN takes an hour or more to stop advertising a dead service |
| MeshChat sync | Already multi-master: instances poll each other every 10 s and merge messages | Database in /tmp, lost on reboot; capped at 500 messages; pulls the whole database each time; files not replicated |
| Consul / Raft | Strong consistency | The minority side of a split can't write: the opposite of what emergency use needs |
| Nomad | Keeps work running through a split; explicit rules for who wins on rejoin | Heavy for a Pi club; no AREDN awareness |
| k3s | Pods keep running when a node drops | A rebooted node won't start containers until it reaches its server |
| memberlist / Serf | Mature gossip membership | False "dead" verdicts on lossy links unless tuned |

Nothing does availability-first failover for AREDN.

## What makes ours the best

- **Start with MeshChat,** which hams already use. A MeshChat-compatible backend on the core's log fixes its data loss on reboot, removes the 500-message cap, stops full-database polling and replicates files. Users keep the same client.
- **Uses the mesh's own link health.** Membership timeouts are tuned for RF and fed by link quality, so a lossy 5.8 GHz hop doesn't make a healthy site look dead and bounce services around.
- **Shows the split.** The page tells users "you're on side B; 3 sites are unreachable; changes will merge when the link returns".
- **Each service declares how it merges:** append-only log, mergeable document, or files, plus an explicit rule for who wins on rejoin (borrowed from Nomad).

## Changes from the v0.1 spec

1. Narrowed to one showcase service first: MeshChat-compatible chat. File drop second.
2. Don't rely on AREDN service adverts for failover; they lag by an hour or more. Every Pi runs a small directory that redirects to wherever the service is now, because clients cache DNS.
3. Placement prefers hosts in the best-connected part of the network, using the planner's graph.
4. Realistic service list: chat and files are straightforward; Winlink (via Pat) is plausible with message ids as duplicate guards; Asterisk can replicate config and voicemail, but phones must re-register; cameras can't move (only recordings can).

## Version 1 scope

- two Pis at two sites running the MeshChat-compatible backend on the core's log
- unplug one: the other keeps serving within 60 seconds; reconnect: everything merges, nothing lost
- directory redirect on every Pi

## Later

- three or more sites, placement by capability, fail-back with hysteresis
- file drop on the blob store
- a "kill a mountaintop" drill with the club
- Winlink via Pat, if a club asks for it

## Part 97

Low risk. Gossip and replication are plain, signed data.

## Risks

- Hardest app in the suite by a wide margin; keep the scope to one service until it works.
- Pre-replicating data costs bandwidth; cap and schedule it.
- AREDN's hourly unpublishing interacting with fast moves; test carefully.

## Success measures

- A service survives loss of its host site within 60 seconds.
- 20 partition drills, zero lost messages.

## Open questions

- Would MeshChat's maintainer welcome a compatible backend, or prefer changes upstream?
- Use an existing Rust gossip crate or write a minimal one?
