NAME       := ish
HOST       := $(shell rustc -vV | awk '/^host:/ {print $$2}')
TARGET     ?= $(subst -unknown-linux-gnu,-unknown-linux-musl,$(HOST))
MUSL_LOADER := $(if $(findstring x86_64,$(TARGET)),/lib/ld-musl-x86_64.so.1,/lib/ld-musl-aarch64.so.1)
MUSL_NATIVE_RUSTFLAGS := $(if $(findstring -linux-musl,$(TARGET)),-L native=/usr/lib)
TARGET_ENV := $(shell echo $(TARGET) | tr '[:lower:]-' '[:upper:]_')
MUSL_CRT_DIR := /usr/lib/e-crt/$(TARGET)
LLVM_BIN   := $(shell rustc --print sysroot)/lib/rustlib/$(TARGET)/bin
PGO_DIR    := $(CURDIR)/target/pgo-profiles
PGO_MERGED := $(PGO_DIR)/merged.profdata

.PHONY: setup build release release-dynamic verify-release verify-release-dynamic release-pgo pgo-profile bench-pgo install test-ci pc bump-version

build:
	cargo build

release:
	cargo clean -p $(NAME) --release --target $(TARGET)
	RUSTFLAGS="$(MUSL_NATIVE_RUSTFLAGS) -Zlocation-detail=none -Zunstable-options -Cpanic=immediate-abort" \
	cargo build --release \
	  -Z build-std=std \
	  -Z build-std-features= \
	  --target $(TARGET)

release-dynamic:
	cargo clean -p $(NAME) --release --target $(TARGET)
	CARGO_TARGET_$(TARGET_ENV)_LINKER=clang \
	RUSTFLAGS="$(MUSL_NATIVE_RUSTFLAGS) -Zlocation-detail=none -Zunstable-options -Cpanic=immediate-abort -Ctarget-feature=-crt-static -Clink-arg=-B$(MUSL_CRT_DIR) -Clink-arg=-dynamic-linker=$(MUSL_LOADER)" \
	cargo build --release \
	  -Z build-std=std \
	  -Z build-std-features= \
	  --target $(TARGET)

verify-release:
	@test -f "target/$(TARGET)/release/$(NAME)"
	@if echo "$(TARGET)" | grep -q -- '-linux-musl$$'; then \
		command -v readelf >/dev/null || { echo 'readelf is required for release verification'; exit 1; }; \
		file "target/$(TARGET)/release/$(NAME)" | grep -Eq 'static-pie linked|statically linked' || { echo 'release is not statically linked'; exit 1; }; \
		file "target/$(TARGET)/release/$(NAME)" | grep -q 'stripped' || { echo 'release is not stripped'; exit 1; }; \
		! readelf -l "target/$(TARGET)/release/$(NAME)" | grep -q INTERP || { echo 'release has a dynamic ELF interpreter'; exit 1; }; \
		! readelf -d "target/$(TARGET)/release/$(NAME)" | grep -q NEEDED || { echo 'release has dynamic dependencies'; exit 1; }; \
	else echo "Skipping ELF checks for $(TARGET)"; fi

verify-release-dynamic:
	@test -f "target/$(TARGET)/release/$(NAME)"
	@if echo "$(TARGET)" | grep -q -- '-linux-musl$$'; then \
		command -v readelf >/dev/null || { echo 'readelf is required for release verification'; exit 1; }; \
		file "target/$(TARGET)/release/$(NAME)" | grep -q 'dynamically linked' || { echo 'release is not dynamically linked'; exit 1; }; \
		file "target/$(TARGET)/release/$(NAME)" | grep -q 'stripped' || { echo 'release is not stripped'; exit 1; }; \
		readelf -l "target/$(TARGET)/release/$(NAME)" | grep -q '/lib/ld-musl-' || { echo 'release does not use the musl loader'; exit 1; }; \
		readelf -d "target/$(TARGET)/release/$(NAME)" | grep -q NEEDED || { echo 'release has no dynamic dependencies'; exit 1; }; \
	else echo "Skipping ELF checks for $(TARGET)"; fi

lint:
	cargo fmt --all
	cargo clippy --fix --allow-dirty --all-targets --all-features -- --deny warnings

# Collect PGO profiles from benchmarks — only re-run when hot paths change.
# No build-std or -Cpanic=immediate-abort here: the profiler runtime needs unwinding.
pgo-profile:
	rm -rf $(PGO_DIR) && mkdir -p $(PGO_DIR)
	RUSTFLAGS="-Cprofile-generate=$(PGO_DIR)" \
	cargo bench --bench bench -- --profile-time 1 "parse|expand|prompt|path_lookup|completion|history|line_buffer"
	$(LLVM_BIN)/llvm-profdata merge -o $(PGO_MERGED) $(PGO_DIR)

# PGO-optimized release: uses gathered profiles + all aggressive flags.
release-pgo: $(PGO_MERGED)
	cargo clean -p $(NAME) --release --target $(TARGET)
	RUSTFLAGS="-Cprofile-use=$(PGO_MERGED) -Zlocation-detail=none -Zunstable-options -Cpanic=immediate-abort" \
	cargo build --release \
	  -Z build-std=std \
	  -Z build-std-features= \
	  --target $(TARGET)

# Benchmark regular release vs PGO. Requires: critcmp (cargo install critcmp)
bench-pgo: $(PGO_MERGED)
	cargo bench --bench bench -- --save-baseline regular 2>/dev/null
	RUSTFLAGS="-Cprofile-use=$(PGO_MERGED)" \
	cargo bench --bench bench -- --save-baseline pgo 2>/dev/null
	critcmp regular pgo

$(PGO_MERGED):
	$(MAKE) pgo-profile

install: release-pgo
	cp target/$(TARGET)/release/$(NAME) ~/usr/bin/$(NAME)
	codesign -fs - ~/usr/bin/$(NAME)

# So we don't do duplicate work (building both debug and release) in CI.
test-ci:
	@OUT=$$(cargo test --quiet --release -- --test-threads=1 2>&1) || { echo "$$OUT"; exit 1; }
