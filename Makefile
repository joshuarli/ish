NAME       := ish
TEST_BINARY_ENV := ISH_TEST_BINARY
TEST_TARGETS := --lib --tests
HOST       := $(shell rustc -vV | awk '/^host:/ {print $$2}')
TARGET     ?= $(subst -unknown-linux-gnu,-unknown-linux-musl,$(HOST))
MUSL_LOADER := $(if $(findstring x86_64,$(TARGET)),/lib/ld-musl-x86_64.so.1,/lib/ld-musl-aarch64.so.1)
MUSL_NATIVE_RUSTFLAGS := $(if $(findstring -linux-musl,$(TARGET)),-L native=/usr/lib)
TARGET_ENV := $(shell echo $(TARGET) | tr '[:lower:]-' '[:upper:]_')
# Fedora's musl cross packages use /usr/<arch>-linux-musl/lib64, while the
# e-crt layout is used by other toolchains. Keep this overridable for hosts
# with a different musl sysroot layout.
MUSL_CRT_DIR ?= $(shell for dir in \
	/usr/lib/e-crt/$(TARGET) \
	/usr/$(subst -unknown,,$(TARGET))/lib64 \
	/usr/$(subst -unknown,,$(TARGET))/lib; do \
	if test -f "$$dir/crtbegin.o"; then printf '%s' "$$dir"; break; fi; \
done)
# LLVM helper binaries run on the build host; they are not installed in the
# target triple's rustlib directory when cross-compiling to musl.
LLVM_BIN   := $(shell rustc --print sysroot)/lib/rustlib/$(HOST)/bin
MUSL_LINKER := $(LLVM_BIN)/rust-lld
MUSL_TARGET_LIBDIR := $(shell rustc --print target-libdir --target $(TARGET))
PGO_DIR    := $(CURDIR)/target/pgo-profiles
PGO_MERGED := $(PGO_DIR)/merged.profdata
PGO_BUILD_DIR := $(CURDIR)/target/pgo-build
PGO_BINARY := $(PGO_BUILD_DIR)/$(TARGET)/release/$(NAME)
RELEASE_RUSTFLAGS := $(MUSL_NATIVE_RUSTFLAGS) -Zlocation-detail=none -Zunstable-options -Cpanic=immediate-abort
RELEASE_LINUX_RUSTFLAGS := $(RELEASE_RUSTFLAGS) -Ctarget-feature=-crt-static -Clink-arg=-B$(MUSL_CRT_DIR) -Clink-arg=-dynamic-linker=$(MUSL_LOADER)
PGO_RUSTFLAGS := $(RELEASE_RUSTFLAGS) -Cprofile-generate=$(PGO_DIR)
PGO_STATIC_RUSTFLAGS := $(if $(findstring -linux-musl,$(TARGET)),$(PGO_RUSTFLAGS) -Clink-arg=/usr/lib/libcompiler-rt-builtins.a,$(PGO_RUSTFLAGS))
PGO_LINUX_RUSTFLAGS := $(RELEASE_LINUX_RUSTFLAGS) -Cprofile-generate=$(PGO_DIR)

.PHONY: build test test-ci release verify-release verify-release-dynamic bench bench-syscalls release-pgo release-pgo-linux release-pgo-linux-static pgo-instrument pgo-instrument-linux pgo-profile pgo-profile-linux install setup ensure-musl-target

build:
	cargo build

test:
	cargo test --quiet

test-ci:
	@test -x "target/$(TARGET)/release/$(NAME)"
	$(TEST_BINARY_ENV)="$(CURDIR)/target/$(TARGET)/release/$(NAME)" \
	RUSTFLAGS="$(MUSL_NATIVE_RUSTFLAGS)" cargo test --quiet --release $(TEST_TARGETS)

release: ensure-musl-target
	cargo clean -p $(NAME) --release --target $(TARGET)
	CARGO_TARGET_$(TARGET_ENV)_LINKER="$(MUSL_LINKER)" \
	RUSTFLAGS="$(MUSL_NATIVE_RUSTFLAGS) -Zlocation-detail=none -Zunstable-options -Cpanic=immediate-abort" \
	cargo build --release \
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
	@scripts/bench-baseline.py

bench-syscalls:
	@scripts/bench-syscalls.py

# Build the release-shaped application binary with target-scoped profile
# instrumentation. The PTY driver and its test dependencies are built later,
# without these flags, so compiler and harness activity cannot enter the profile.
pgo-instrument: ensure-musl-target
	rm -rf "$(PGO_BUILD_DIR)" "$(PGO_DIR)" && mkdir -p "$(PGO_DIR)"
	CARGO_TARGET_DIR="$(PGO_BUILD_DIR)" \
	CARGO_TARGET_$(TARGET_ENV)_LINKER="$(MUSL_LINKER)" \
	CARGO_TARGET_$(TARGET_ENV)_RUSTFLAGS="$(PGO_STATIC_RUSTFLAGS)" \
	cargo build --release \
	  -Z build-std=std \
	  -Z build-std-features= \
	  --target $(TARGET)
	@test -x "$(PGO_BINARY)"

# The dynamic Linux artifact uses the same loader and CRT shape as the final
# release binary, so its startup profile includes the deployed linking path.
pgo-instrument-linux: ensure-musl-target
	rm -rf "$(PGO_BUILD_DIR)" "$(PGO_DIR)" && mkdir -p "$(PGO_DIR)"
	CARGO_TARGET_DIR="$(PGO_BUILD_DIR)" \
	CARGO_TARGET_$(TARGET_ENV)_LINKER=clang \
	CARGO_TARGET_$(TARGET_ENV)_RUSTFLAGS="$(PGO_LINUX_RUSTFLAGS)" \
	cargo build --release \
	  -Z build-std=std \
	  -Z build-std-features= \
	  --target $(TARGET)
	@test -x "$(PGO_BINARY)"

# Run only the curated PTY workload against the instrumented binary. This is
# intentionally separate from `bench`: it profiles the real event loop,
# startup, history search, and pager rendering rather than benchmark machinery.
pgo-profile: pgo-instrument
	RUSTFLAGS="" CARGO_TARGET_$(TARGET_ENV)_RUSTFLAGS="" \
	ISH_TEST_BINARY="$(PGO_BINARY)" \
	cargo test --release --test pty pgo_profile_startup_history_tui -- --ignored
	@test -n "$$(find "$(PGO_DIR)" -type f -name '*.profraw' -print -quit)"
	$(LLVM_BIN)/llvm-profdata merge -o "$(PGO_MERGED)" "$(PGO_DIR)"/*.profraw

pgo-profile-linux: pgo-instrument-linux
	RUSTFLAGS="" CARGO_TARGET_$(TARGET_ENV)_RUSTFLAGS="" \
	ISH_TEST_BINARY="$(PGO_BINARY)" \
	cargo test --release --test pty pgo_profile_startup_history_tui -- --ignored
	@test -n "$$(find "$(PGO_DIR)" -type f -name '*.profraw' -print -quit)"
	$(LLVM_BIN)/llvm-profdata merge -o "$(PGO_MERGED)" "$(PGO_DIR)"/*.profraw

# PGO-optimized release: build dependencies and build-std without profile
# runtime support, then apply the merged profile to the final application
# compilation. The profile is never passed to the benchmark or PTY harness.
release-pgo: ensure-musl-target pgo-profile
	CARGO_TARGET_$(TARGET_ENV)_LINKER="$(MUSL_LINKER)" \
	CARGO_TARGET_$(TARGET_ENV)_RUSTFLAGS="$(RELEASE_RUSTFLAGS)" \
	cargo build --release \
	  -Z build-std=std \
	  -Z build-std-features= \
	  --target $(TARGET)
	CARGO_TARGET_$(TARGET_ENV)_LINKER="$(MUSL_LINKER)" \
	CARGO_TARGET_$(TARGET_ENV)_RUSTFLAGS="$(RELEASE_RUSTFLAGS)" \
	cargo rustc --release \
	  -Z build-std=std \
	  -Z build-std-features= \
	  --target $(TARGET) --bin $(NAME) -- \
	  -Cprofile-use=$(PGO_MERGED) \
	  -Cllvm-args=-pgo-warn-missing-function

release-pgo-linux: pgo-profile-linux
	CARGO_TARGET_$(TARGET_ENV)_LINKER=clang \
	CARGO_TARGET_$(TARGET_ENV)_RUSTFLAGS="$(RELEASE_LINUX_RUSTFLAGS)" \
	cargo build --release \
	  -Z build-std=std \
	  -Z build-std-features= \
	  --target $(TARGET)
	CARGO_TARGET_$(TARGET_ENV)_LINKER=clang \
	CARGO_TARGET_$(TARGET_ENV)_RUSTFLAGS="$(RELEASE_LINUX_RUSTFLAGS)" \
	cargo rustc --release \
	  -Z build-std=std \
	  -Z build-std-features= \
	  --target $(TARGET) --bin $(NAME) -- \
	  -Cprofile-use=$(PGO_MERGED) \
	  -Cllvm-args=-pgo-warn-missing-function

release-pgo-linux-static: ensure-musl-target pgo-profile
	CARGO_TARGET_$(TARGET_ENV)_LINKER="$(MUSL_LINKER)" \
	CARGO_TARGET_$(TARGET_ENV)_RUSTFLAGS="$(RELEASE_RUSTFLAGS)" \
	cargo build --release \
	  -Z build-std=std \
	  -Z build-std-features= \
	  --target $(TARGET)
	CARGO_TARGET_$(TARGET_ENV)_LINKER="$(MUSL_LINKER)" \
	CARGO_TARGET_$(TARGET_ENV)_RUSTFLAGS="$(RELEASE_RUSTFLAGS)" \
	cargo rustc --release \
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

ensure-musl-target:
	@if ! echo "$(TARGET)" | grep -q -- '-linux-musl$$'; then exit 0; fi; \
	if test -f "$(MUSL_TARGET_LIBDIR)/self-contained/libunwind.a"; then \
		exit 0; \
	fi; \
	if command -v rustup >/dev/null 2>&1; then \
		echo "Installing Rust target $(TARGET)"; \
		rustup target add "$(TARGET)"; \
	else \
		echo "Rust target $(TARGET) is missing and rustup is unavailable" >&2; \
		exit 1; \
	fi
