#!/bin/sh
# Install the ridgewarden core and the apps you name, or the ten main ones.
#
#   curl -fsSL https://github.com/Londopy/ridgewarden/releases/latest/download/install-all.sh | sudo sh
#   ... | sudo sh -s -- health status planner
#   ... | sudo sh -s -- all          # also the two experimental apps, weather and radar
#
# RIDGEWARDEN_CALLSIGN=N0CALL also sets the core up, as with the core's own install.sh.
# Each app's install.sh does the work; before it runs, its release's checksums are
# checked with the installed core's release key, and the script against those checksums.
# On a box with no core yet, the core comes from its own installer over HTTPS, so the app
# checks then rest on the key in that freshly fetched core: they prove the apps came from
# the same release key, not more than HTTPS to GitHub proved for the core itself.
set -eu

MAIN="health linkdoctor status planner library lightning game sdr failover mule"
ALL="$MAIN weather radar"

if [ "$(id -u)" -ne 0 ]; then
    echo "Run as root (with sudo)." >&2
    exit 1
fi

case "${1:-}" in
    "") apps=$MAIN ;;
    all) apps=$ALL ;;
    *) apps="$*" ;;
esac
for a in $apps; do
    case " $ALL " in
        *" $a "*) ;;
        *) echo "No app called $a. Apps: $ALL" >&2; exit 1 ;;
    esac
done

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

fetch() {
    curl -fsSL --connect-timeout 20 --max-time 300 -o "$1" "$2"
}

# the whole check hangs on this binary, so take it from where the installers put it,
# never from whatever PATH finds first
find_core() {
    for c in /usr/local/bin/ridgewarden /usr/bin/ridgewarden; do
        [ -x "$c" ] && { echo "$c"; return 0; }
    done
    # nothing found is an answer, not a failure (set -e would stop the script here)
    return 0
}

core=$(find_core)
if [ -z "$core" ]; then
    echo "Installing the ridgewarden core"
    fetch "$tmp/core.sh" https://github.com/Londopy/ridgewarden/releases/latest/download/install.sh
    sh "$tmp/core.sh" </dev/null
    core=$(find_core)
    [ -n "$core" ] || { echo "The core didn't install; not installing apps." >&2; exit 1; }
fi

for a in $apps; do
    repo=ridgewarden-$a
    base=https://github.com/Londopy/$repo/releases/latest/download
    mkdir -p "$tmp/$a"
    for f in install.sh SHA256SUMS.txt SHA256SUMS.txt.sig; do
        fetch "$tmp/$a/$f" "$base/$f"
    done
    # the signature proves the checksums came from the release key; the checksum proves the script
    "$core" verify-release "$tmp/$a/SHA256SUMS.txt"
    (cd "$tmp/$a" && grep ' install.sh$' SHA256SUMS.txt | sha256sum -c -)
    echo "Installing $repo"
    # the app's installer fetches the latest release again; if one is published meanwhile it
    # is still checked against its own signed checksums
    sh "$tmp/$a/install.sh" </dev/null
done

echo "Done: ridgewarden and $(echo "$apps" | wc -w) apps."
