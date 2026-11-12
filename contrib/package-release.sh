#!/bin/sh
# Packages built binaries as dist/crew-<target>.tar.gz with a .sha256 beside it: package-release.sh <target> <bin-dir>
set -eu

target=${1:?usage: package-release.sh <target> <bin-dir>}
bindir=${2:?usage: package-release.sh <target> <bin-dir>}
root=$(cd "$(dirname "$0")/.." && pwd)
out=${CREW_DIST:-$root/dist}
name=crew-$target

rm -rf "${out:?}/$name"
mkdir -p "$out/$name"
for bin in crew crew-usher crew-plan crew-queue crew-depot; do
    test -x "$bindir/$bin" || { echo "package-release: $bindir/$bin is missing" >&2; exit 1; }
    if [ -z "${CREW_ALLOW_DYNAMIC:-}" ]; then
        # A dynamic binary would not run on the machines this is for.
        file "$bindir/$bin" | grep -Eq 'statically linked|static-pie linked' || { echo "package-release: $bin is not static" >&2; exit 1; }
    fi
    install -m 0755 "$bindir/$bin" "$out/$name/$bin"
done
"$out/$name/crew" help > /dev/null
cp "$root/LICENSE-APACHE" "$root/LICENSE-MIT" "$root/README.md" "$out/$name/"
cp "$root/contrib/crew-usher.service" "$out/$name/"
tar --sort=name --owner=0 --group=0 --numeric-owner -C "$out" -czf "$out/$name.tar.gz" "$name"
(cd "$out" && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
rm -rf "${out:?}/$name"
echo "$out/$name.tar.gz"
