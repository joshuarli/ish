#!/bin/sh
set -eu
root=$(CDPATH= cd "$(dirname "$0")/.." && pwd)
mkdir -p "$root/.work/tmp"
scratch=$(mktemp -d "$root/.work/tmp/vendor-fz-test.XXXXXXXX")
trap 'rm -rf -- "$scratch"' EXIT HUP INT TERM
mkdir -p "$scratch/upstream/src" "$scratch/upstream/tests" "$scratch/upstream/docs" \
    "$scratch/consumer/src" "$scratch/consumer/scripts" "$scratch/consumer/vendor"
cat > "$scratch/upstream/Cargo.toml" <<'EOF'
[package]
name = "fz"
version = "0.0.0"
edition = "2024"

[[test]]
name = "foreground"
harness = false
EOF
printf 'pub fn fixture() {}\n' > "$scratch/upstream/src/lib.rs"
printf 'fn main() {}\n' > "$scratch/upstream/src/main.rs"
printf 'fn main() {}\n' > "$scratch/upstream/tests/foreground.rs"
for path in LICENSE README.md docs/testing.md; do printf 'fixture\n' > "$scratch/upstream/$path"; done
git -C "$scratch/upstream" init -q
git -C "$scratch/upstream" add -f Cargo.toml src tests docs LICENSE README.md
git -C "$scratch/upstream" -c core.hooksPath=/dev/null -c commit.gpgsign=false \
    -c user.name=Fixture -c user.email=fixture@example.invalid commit -qm fixture
git -C "$scratch/consumer" init -q
cp "$root/scripts/vendor-fz.sh" "$scratch/consumer/scripts/vendor-fz.sh"
cat > "$scratch/consumer/Cargo.toml" <<'EOF'
[package]
name = "vendor-consumer"
version = "0.0.0"
edition = "2024"

[dependencies]
fz = { path = "vendor/fz" }

[workspace]
exclude = ["vendor/fz"]
EOF
printf 'fn main() { fz::fixture(); }\n' > "$scratch/consumer/src/main.rs"
sh "$scratch/consumer/scripts/vendor-fz.sh" "$scratch/upstream" HEAD
# Isolate the nested fixture from the checkout's enclosing Cargo workspace.
printf '\n[workspace]\n' >> "$scratch/consumer/vendor/fz/Cargo.toml"
CARGO_BUILD_JOBS=1 cargo test --offline --manifest-path "$scratch/consumer/vendor/fz/Cargo.toml"
CARGO_BUILD_JOBS=1 cargo test --offline --manifest-path "$scratch/consumer/Cargo.toml"
