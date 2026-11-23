# 01 Mountaintop SDR network

Repo: `ridgewarden-sdr` | Reviewed 2026-11-19 | Status: version 1 built, not yet released

Built: receiver directory and offline map from signed events; Part 97 band filtering with `check-owrx` and the two-instance OpenWebRX+ setup, with the running instance's own profile list (`status.json`) checked too before its link is published; coordinated signal-strength capture; TDOA from RTL-SDR IQ snippets aligned on an amateur reference transmitter with drift removal, GCC-PHAT and quality gating, solved at 3+ sites (clocks on NTP only start the recordings together and bound the search; the timing comes from the reference, never from a clock); KrakenSDR bearings from the "Kraken App" output; one fix from all three with a 95% error ellipse, ring-of-receivers and ambiguity warnings; the weekly beacon test publishing miss distance and its time differences, with an accuracy page and a timing page plotting each correlation peak against the known delay; success measures shown in the app (sites listening, neighborhood-sized fixes, miss-distance trend).

Not done: NOAA Weather Radio and ATSC references, by decision (their recordings can't cross amateur RF, and NOAA is 14 MHz from the nearest amateur band, too far to share a 2 MHz recording with the target); 10 MHz/PPS receivers for the 500 m stretch target (hardware); the live fox-hunt demo, the university write-up and partnership, and asking the people running the mesh and the AREDN forum about non-amateur waterfalls (not software); no capture yet on real dongles, a real KrakenSDR or a real OpenWebRX+ instance, so the 0.5 to 2 km target is still unmeasured.

## What it is

Software-defined radios at mesh sites that any ham on the mesh can find and tune from a browser, plus a way to locate an interfering transmitter from several sites at once.

## Who it's for

Repeater owners and trustees chasing interference, the frequency coordinator (NARCC), fox hunters, and a university direction-finding group as a research partner.

## What exists today

| Project | What it does well | Gap |
|---|---|---|
| OpenWebRX+ | The standard browser SDR: bookmarks, background decoding, several SDRs per box, a map of public web SDRs | No federation between instances, no API; nothing mesh-local |
| Receiverbook | Directory of public web SDRs | Central, internet-only, unsigned |
| KiwiSDR TDoA | Working multi-site transmitter location with GPS timestamps; open solver code | HF only (0-30 MHz), internet-hosted, "under 10 km" in good cases |
| Panoradio (DC9ST) | RTL-SDR TDOA at UHF with no GPS, by locking every receiver to a known reference transmitter | Proof of concept, three receivers, no published accuracy; FM correlated poorly |
| KrakenSDR | Five-channel direction finding, $499 | "Several degrees" accuracy, sensitive to multipath; no GPS or TDOA support; software moves slowly |

No mesh-local SDR directory and no amateur VHF/UHF TDOA network were found anywhere.

## What makes ours the best

- **A directory that works with the internet down.** Every site's receiver registers as a signed event: location, bands, antenna, live status. One map shows every SDR on the mesh, and it is the only directory of its kind.
- **Part 97 handled for the operator.** Only amateur-band profiles are offered over RF links (97.113(c)); other bands stay on the site LAN or the internet.
- **Interference hunting that degrades gracefully.** Signal-strength comparison across sites works from day one with any SDR. Bearings from a KrakenSDR add to it. TDOA adds precision where the hardware allows.
- **Honest accuracy.** Each fix shows an error ellipse, and a weekly test against a beacon at a surveyed spot publishes the actual miss distance.

## Changes from the v0.1 spec

1. **Timing method.** Reading the RTL-SDR's clock from USB is useless, and feeding GPS pulses into it still leaves about 2 microseconds of error (600 m). The design switches to Panoradio's method: every site captures one shared reference transmitter inside the same snippet (a beacon, NOAA Weather Radio or an ATSC TV station), and per-snippet drift is estimated and removed.
2. **Realistic target.** 0.5 to 2 km inside the ring of receivers with RTL-SDRs. The 500 m goal needs a calibration beacon or SDRs with real 10 MHz and PPS inputs; it is a stretch target, not v1.
3. **Signal choice.** Best targets are digital bursts (DMR, P25, data) and key-up transients. A carrier with only a CTCSS tone has almost nothing to correlate, so the app says so instead of returning a bad fix. Correlation uses GCC-PHAT with quality gating.
4. **Order of work.** Directory and signal-strength hunting first; TDOA second.

## Version 1 scope

- receiver registration and directory map (signed events)
- OpenWebRX+ at two sites, amateur-band profiles only over RF
- coordinated signal-strength capture: "which site hears this loudest, and how loud"

## Later

- TDOA with reference-transmitter alignment, 3+ sites, solver with error ellipses
- KrakenSDR bearings blended in
- live fox-hunt demo; write-up with a university DF group

## Part 97

- Receive-only stations need no license.
- 97.113(c): no retransmitting non-amateur signals over RF; band filtering is built in.
- Waterfalls of non-amateur bands over the mesh: likely low risk as measurement data, but ask the people running the mesh and the AREDN forum first.

## Risks

- Mountaintop sites are RF-hot: band-pass filters are required, and desense from co-located transmitters will limit some sites.
- Multipath off ridges, and poor accuracy outside the ring of receivers.
- IQ snippets are large (an RTL-SDR at 2.4 MS/s is about 38 Mbps); only short, decimated snippets ever cross the mesh.

## Success measures

- Directory used by the mesh group; at least two sites listening.
- One real interference case narrowed to a neighborhood.
- Published miss distance against the beacon, improving over time.

## Open questions

- Which sites have power, space and owner permission for an SDR?
- Formal partnership with a university DF project?
