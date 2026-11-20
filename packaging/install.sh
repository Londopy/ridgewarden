#!/bin/sh
# Install the ridgewarden core agent on Linux (Raspberry Pi 64-bit or x86_64).
#
#   curl -fsSL https://github.com/Londopy/ridgewarden/releases/latest/download/install.sh | sudo sh
#
# Optional: RIDGEWARDEN_VERSION=0.2.0 to pin a version, RIDGEWARDEN_CALLSIGN=N0CALL to also
# create the config and start the service.
set -eu

REPO=Londopy/ridgewarden
BIN=/usr/local/bin/ridgewarden

if [ "$(id -u)" -ne 0 ]; then
    echo "Run as root (with sudo)." >&2
    exit 1
fi

case "$(uname -m)" in
    x86_64 | amd64) arch=x86_64 ;;
    aarch64 | arm64) arch=aarch64 ;;
    *)
        echo "No build for $(uname -m). 64-bit Raspberry Pi OS and x86_64 Linux are supported." >&2
        exit 1
        ;;
esac

if [ -n "${RIDGEWARDEN_VERSION:-}" ]; then
    tag="v$RIDGEWARDEN_VERSION"
else
    tag=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" |
        sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)
fi
[ -n "$tag" ] || { echo "Could not find the latest release." >&2; exit 1; }
ver=${tag#v}
file="ridgewarden-v$ver-$arch-linux.tar.gz"
base="https://github.com/$REPO/releases/download/$tag"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
echo "Downloading ridgewarden $tag for $arch"
for f in "$file" SHA256SUMS.txt SHA256SUMS.txt.sig; do
    curl -fsSL -o "$tmp/$f" "$base/$f"
done

# checksum with the system's tools, then the release signature. The signature check uses
# the key inside the binary we just downloaded, so on a first install it only shows the
# files belong together; it can't prove who made them. Once installed, `ridgewarden
# update` checks every later release against the key this install already has.
(cd "$tmp" && grep " $file\$" SHA256SUMS.txt | sha256sum -c -)
tar -xzf "$tmp/$file" -C "$tmp"
"$tmp/ridgewarden-v$ver-$arch-linux/ridgewarden" verify-release "$tmp/SHA256SUMS.txt"

install -m 755 "$tmp/ridgewarden-v$ver-$arch-linux/ridgewarden" "$BIN"

if ! id ridgewarden >/dev/null 2>&1; then
    useradd --system --home-dir /var/lib/ridgewarden --shell /usr/sbin/nologin ridgewarden
fi
install -d -m 755 /etc/ridgewarden
install -d -m 750 -o ridgewarden -g ridgewarden /var/lib/ridgewarden

if [ -d /run/systemd/system ]; then
    sed "s#/usr/bin/ridgewarden#$BIN#" "$tmp/ridgewarden-v$ver-$arch-linux/ridgewarden.service" \
        >/etc/systemd/system/ridgewarden.service
    systemctl daemon-reload
fi

if [ -f /etc/ridgewarden/ridgewarden.toml ]; then
    # an upgrade: a running agent must restart for the new version to take over
    [ -d /run/systemd/system ] && systemctl try-restart ridgewarden
    echo "Upgraded to $tag."
elif [ -n "${RIDGEWARDEN_CALLSIGN:-}" ]; then
    "$BIN" --config /etc/ridgewarden/ridgewarden.toml init \
        --callsign "$RIDGEWARDEN_CALLSIGN" --data-dir /var/lib/ridgewarden
    chown -R ridgewarden:ridgewarden /var/lib/ridgewarden
    chmod 644 /etc/ridgewarden/ridgewarden.toml
    [ -d /run/systemd/system ] && systemctl enable --now ridgewarden
    echo "ridgewarden is running. Dashboard: http://$(hostname):8790/"
else
    echo "Installed $BIN ($tag). Next:"
    echo "  sudo ridgewarden --config /etc/ridgewarden/ridgewarden.toml init --callsign YOURCALL --data-dir /var/lib/ridgewarden"
    echo "  sudo chown -R ridgewarden:ridgewarden /var/lib/ridgewarden"
    echo "  sudo systemctl enable --now ridgewarden"
fi
