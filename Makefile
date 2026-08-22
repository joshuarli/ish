NAME       := ish
TEST_BINARY_ENV := ISH_TEST_BINARY
TEST_TARGETS := --lib --tests
HOST       := $(shell rustc -vV | awk '/^host:/ {print $$2}')
TARGET     ?= $(subst -unknown-linux-gnu,-unknown-linux-musl,$(HOST))
CARGO_CMD  := $(if $(findstring -linux-musl,$(TARGET)),$(shell command -v musl-cargo 2>/dev/null || printf cargo),cargo)
LLVM_PROFDATA := $(shell command -v llvm-profdata 2>/dev/null || printf '%s/lib/rustlib/%s/bin/llvm-profdata' "$(shell rustc --print sysroot)" "$(HOST)")
PGO_DIR    := $(CURDIR)/target/pgo-profiles
PGO_MERGED := $(PGO_DIR)/merged.profdata
PGO_BUILD_DIR := $(CURDIR)/target/pgo-build
PGO_BINARY := $(PGO_BUILD_DIR)/$(TARGET)/release/$(NAME)

# Docker's musl-cargo wrapper owns linker, CRT, loader, and profile-runtime
# flags. The Makefile selects only the build mode and keeps Cargo invocations
# readable on macOS as well as in the Linux release image.
cargo = $(if $(findstring -linux-musl,$(TARGET)),,$(if $(filter release-static release-dynamic static-profile dynamic-profile,$(1)),RUSTFLAGS="-Zlocation-detail=none -Zunstable-options -Cpanic=immediate-abort")) MUSL_TARGET="$(TARGET)" MUSL_BUILD_MODE="$(1)" MUSL_PROFILE_DIR="$(PGO_DIR)" $(CARGO_CMD)

RUSTYBENCH ?= cargo run --quiet --manifest-path ../rustybench/Cargo.toml --

.PHONY: build test test-ci release verify-release verify-release-dynamic bench bench-fast bench-syscalls release-pgo release-pgo-linux release-pgo-linux-static pgo-instrument pgo-instrument-linux pgo-profile pgo-profile-linux install setup

build:
	$(call cargo,dev) build

test:
	$(call cargo,test) test --quiet

test-ci:
	@test -x "target/$(TARGET)/release/$(NAME)"
	$(TEST_BINARY_ENV)="$(CURDIR)/target/$(TARGET)/release/$(NAME)" \
	$(call cargo,test) test --quiet --release $(TEST_TARGETS)

release:
	$(call cargo,release-static) clean -p $(NAME) --release --target $(TARGET)
	$(call cargo,release-static) build --release \
	  -Z build-std=std \
	  -Z build-std-features= \
	  --target $(TARGET)

verify-release:
	@test -f "target/$(TARGET)/release/$(NAME)"
	@if command -v otool >/dev/null 2>&1 && otool -l "target/$(TARGET)/release/$(NAME)" 2>/dev/null | grep -q '__llvm_prf'; then \
		echo 'release still contains PGO profile sections; rebuild with make release-pgo' >&2; \
		exit 1; \
	fi
	@if strings "target/$(TARGET)/release/$(NAME)" 2>/dev/null | grep -q 'LLVM Profile'; then \
		echo 'release still contains the LLVM profile runtime; profile use must be limited to the application crate' >&2; \
		exit 1; \
	fi
	@if echo "$(TARGET)" | grep -q -- '-linux-musl$$'; then \
		command -v readelf >/dev/null || { echo 'readelf is required for release verification'; exit 1; }; \
		file "target/$(TARGET)/release/$(NAME)" | grep -Eq 'static-pie linked|statically linked' || { echo 'release is not statically linked'; exit 1; }; \
		file "target/$(TARGET)/release/$(NAME)" | grep -q 'stripped' || { echo 'release is not stripped'; exit 1; }; \
		! readelf -l "target/$(TARGET)/release/$(NAME)" | grep -q INTERP || { echo 'release has a dynamic ELF interpreter'; exit 1; }; \
		! readelf -d "target/$(TARGET)/release/$(NAME)" | grep -q NEEDED || { echo 'release has dynamic dependencies'; exit 1; }; \
	else \
		echo "Skipping ELF checks for $(TARGET)"; \
	fi

verify-release-dynamic:
	@test -f "target/$(TARGET)/release/$(NAME)"
	@if echo "$(TARGET)" | grep -q -- '-linux-musl$$'; then \
		command -v readelf >/dev/null || { echo 'readelf is required for release verification'; exit 1; }; \
		file "target/$(TARGET)/release/$(NAME)" | grep -q 'dynamically linked' || { echo 'release is not dynamically linked'; exit 1; }; \
		file "target/$(TARGET)/release/$(NAME)" | grep -q 'stripped' || { echo 'release is not stripped'; exit 1; }; \
		readelf -l "target/$(TARGET)/release/$(NAME)" | grep -q '/lib/ld-musl-' || { echo 'release does not use the musl loader'; exit 1; }; \
		readelf -d "target/$(TARGET)/release/$(NAME)" | grep -q NEEDED || { echo 'release has no dynamic dependencies'; exit 1; }; \
	else \
		echo "Skipping ELF checks for $(TARGET)"; \
	fi

lint:
	cargo fmt --all
	cargo clippy --fix --allow-dirty --all-targets --all-features -- --deny warnings

bench:
	@$(RUSTYBENCH) baseline --root "$(CURDIR)" --baseline "$(CURDIR)/benches/baseline.json" -- cargo bench --bench bench

bench-fast:
	@$(RUSTYBENCH) baseline --root "$(CURDIR)" --baseline "$(CURDIR)/benches/fast-baseline.json" --fast -- cargo bench --bench bench

bench-syscalls:
	@$(RUSTYBENCH) syscalls --root "$(CURDIR)"

# Build the release-shaped application binary with Docker's profile mode. The
# PTY driver and its test dependencies are built later without instrumentation.
pgo-instrument:
	rm -rf "$(PGO_BUILD_DIR)" "$(PGO_DIR)" && mkdir -p "$(PGO_DIR)"
	CARGO_TARGET_DIR="$(PGO_BUILD_DIR)" \
	$(call cargo,static-profile) build --release \
	  -Z build-std=std \
	  -Z build-std-features= \
	  --target $(TARGET)
	@test -x "$(PGO_BINARY)"

# The dynamic Linux artifact uses Docker's dynamic-profile mode so its startup
# profile includes the deployed linking path.
pgo-instrument-linux:
	rm -rf "$(PGO_BUILD_DIR)" "$(PGO_DIR)" && mkdir -p "$(PGO_DIR)"
	CARGO_TARGET_DIR="$(PGO_BUILD_DIR)" \
	$(call cargo,dynamic-profile) build --release \
	  -Z build-std=std \
	  -Z build-std-features= \
	  --target $(TARGET)
	@test -x "$(PGO_BINARY)"

# Run only the curated PTY workload against the instrumented binary. This is
# intentionally separate from `bench`: it profiles the real event loop,
# startup, history search, and pager rendering rather than benchmark machinery.
pgo-profile: pgo-instrument
	ISH_PGO_PROFILE_DIR="$(PGO_DIR)" \
	RUSTFLAGS="" \
	ISH_TEST_BINARY="$(PGO_BINARY)" \
	$(call cargo,driver) test --release --test pty pgo_profile_startup_history_tui -- --ignored
	@test -n "$$(find "$(PGO_DIR)" -type f -name '*.profraw' -print -quit)"
	$(LLVM_PROFDATA) merge -o "$(PGO_MERGED)" "$(PGO_DIR)"/*.profraw

pgo-profile-linux: pgo-instrument-linux
	ISH_PGO_PROFILE_DIR="$(PGO_DIR)" \
	RUSTFLAGS="" \
	ISH_TEST_BINARY="$(PGO_BINARY)" \
	$(call cargo,driver) test --release --test pty pgo_profile_startup_history_tui -- --ignored
	@test -n "$$(find "$(PGO_DIR)" -type f -name '*.profraw' -print -quit)"
	$(LLVM_PROFDATA) merge -o "$(PGO_MERGED)" "$(PGO_DIR)"/*.profraw

# PGO-optimized release: build dependencies and build-std without profile
# runtime support, then apply the merged profile to the final application
# compilation. The profile is never passed to the benchmark or PTY harness.
release-pgo: pgo-profile
	$(call cargo,release-static) build --release \
	  -Z build-std=std \
	  -Z build-std-features= \
	  --target $(TARGET)
	$(call cargo,release-static) rustc --release \
	  -Z build-std=std \
	  -Z build-std-features= \
	  --target $(TARGET) --bin $(NAME) -- \
	  -Cprofile-use=$(PGO_MERGED) \
	  -Cllvm-args=-pgo-warn-missing-function

release-pgo-linux: pgo-profile-linux
	$(call cargo,release-dynamic) build --release \
	  -Z build-std=std \
	  -Z build-std-features= \
	  --target $(TARGET)
	$(call cargo,release-dynamic) rustc --release \
	  -Z build-std=std \
	  -Z build-std-features= \
	  --target $(TARGET) --bin $(NAME) -- \
	  -Cprofile-use=$(PGO_MERGED) \
	  -Cllvm-args=-pgo-warn-missing-function

release-pgo-linux-static: pgo-profile
	$(call cargo,release-static) build --release \
	  -Z build-std=std \
	  -Z build-std-features= \
	  --target $(TARGET)
	$(call cargo,release-static) rustc --release \
	  -Z build-std=std \
	  -Z build-std-features= \
	  --target $(TARGET) --bin $(NAME) -- \
	  -Cprofile-use=$(PGO_MERGED) \
	  -Cllvm-args=-pgo-warn-missing-function

install: release-pgo
	cp target/$(TARGET)/release/$(NAME) ~/usr/bin/$(NAME)
	@if test "$$(uname -s)" = Darwin; then \
		codesign -fs - ~/usr/bin/$(NAME); \
	fi
