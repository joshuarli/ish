#!/bin/sh
set -eu
if [ "$#" -ne 2 ]; then
    echo 'usage: scripts/vendor-fz.sh /path/to/fz COMMIT' >&2
    exit 2
fi
repo=$1
revision=$(git -C "$repo" rev-parse --verify "$2^{commit}")
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"
mkdir -p .work/tmp
stage=$(mktemp -d "$root/.work/tmp/vendor-fz.XXXXXXXX")
trap 'rm -rf -- "$stage"' EXIT HUP INT TERM
git -C "$repo" archive "$revision" Cargo.toml LICENSE README.md src docs > "$stage/source.tar"
mkdir "$stage/fz"
tar -xf "$stage/source.tar" -C "$stage/fz"
cp "$stage/fz/Cargo.toml" "$stage/fz/Cargo.toml.orig"
jaq --from toml --to toml 'del(.workspace, .profile, ."dev-dependencies", .bench) | .package.autobins = false | .package.autotests = false | .package.autobenches = false' "$stage/fz/Cargo.toml.orig" > "$stage/fz/Cargo.toml"
rm -rf -- vendor/fz
mv "$stage/fz" vendor/fz
printf 'Repository: https://github.com/laputa-systems/fz\nRevision: %s\nLicense: MIT (LICENSE)\nUpdate: scripts/vendor-fz.sh /path/to/fz COMMIT\n' "$revision" > vendor/fz/UPSTREAM
