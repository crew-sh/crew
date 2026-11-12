#!/bin/sh
# Installs crew from the latest GitHub release: curl -fsSL https://crew.umceko.dev/install.sh | sh
# CREW_INSTALL_DIR picks the directory (default ~/.local/bin); CREW_VERSION pins a release such as v0.1.0.
set -eu

repo=${CREW_REPO:-crew-sh/crew}
dir=${CREW_INSTALL_DIR:-$HOME/.local/bin}
version=${CREW_VERSION:-latest}

say() { printf 'crew install: %s\n' "$*"; }
die() { printf 'crew install: %s\n' "$*" >&2; exit 1; }

[ "$(uname -s)" = Linux ] || die "crew runs on Linux with a systemd user session only"
case "$(uname -m)" in
    x86_64 | amd64) arch=x86_64 ;;
    aarch64 | arm64) arch=aarch64 ;;
    *) die "no prebuilt binaries for $(uname -m); build from source: cargo install --locked --git https://github.com/$repo crew crew-usher crew-queue crew-ledger crew-depot" ;;
esac

if [ "$version" = latest ]; then
    base=${CREW_RELEASE_URL:-https://github.com/$repo/releases/latest/download}
else
    base=${CREW_RELEASE_URL:-https://github.com/$repo/releases/download/$version}
fi
asset=crew-$arch-unknown-linux-musl.tar.gz

if command -v curl > /dev/null 2>&1; then
    fetch() { curl -fsSL --retry 3 -o "$2" "$1"; }
elif command -v wget > /dev/null 2>&1; then
    fetch() { wget -q -O "$2" "$1"; }
else
    die "curl or wget is needed"
fi
if command -v sha256sum > /dev/null 2>&1; then
    sum() { sha256sum "$1" | cut -d ' ' -f 1; }
elif command -v shasum > /dev/null 2>&1; then
    sum() { shasum -a 256 "$1" | cut -d ' ' -f 1; }
else
    die "sha256sum or shasum is needed to check the download"
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM

say "downloading $asset ($version)"
fetch "$base/$asset" "$work/$asset" || die "could not download $base/$asset"
fetch "$base/SHA256SUMS" "$work/SHA256SUMS" || die "could not download $base/SHA256SUMS"

want=$(awk -v a="$asset" '$2 == a || $2 == "*" a { print $1; exit }' "$work/SHA256SUMS")
[ -n "$want" ] || die "SHA256SUMS has no entry for $asset"
have=$(sum "$work/$asset")
[ "$want" = "$have" ] || die "checksum mismatch for $asset: expected $want, got $have; nothing was installed"

tar -xzf "$work/$asset" -C "$work"
src=$work/crew-$arch-unknown-linux-musl
mkdir -p "$dir"
for bin in crew crew-usher crew-plan crew-queue crew-depot; do
    [ -x "$src/$bin" ] || die "the release has no $bin"
done
for bin in crew crew-usher crew-plan crew-queue crew-depot; do
    # A copy beside the target, then a rename, so a running usher is replaced without being cut off.
    cp "$src/$bin" "$dir/.$bin.new"
    chmod 0755 "$dir/.$bin.new"
    mv -f "$dir/.$bin.new" "$dir/$bin"
done

data=${XDG_DATA_HOME:-$HOME/.local/share}/crew
mkdir -p "$data"
sed "s|^ExecStart=.*|ExecStart=$dir/crew-usher|" "$src/crew-usher.service" > "$data/crew-usher.service"

say "installed crew, crew-usher, crew-plan, crew-queue and crew-depot into $dir"
case ":$PATH:" in
    *":$dir:"*) ;;
    *) say "$dir is not on your PATH; add it, for example: export PATH=\"$dir:\$PATH\"" ;;
esac
cat << EOF

To run the usher as a user service:

  mkdir -p ~/.config/systemd/user
  cp $data/crew-usher.service ~/.config/systemd/user/
  systemctl --user daemon-reload
  systemctl --user enable --now crew-usher

Then try: crew run -- cargo build
EOF
