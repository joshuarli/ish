NAME       := ish
TARGET     := $(shell rustc -vV | awk '/^host:/ {print $$2}')
LLVM_BIN   := $(shell rustc --print sysroot)/lib/rustlib/$(TARGET)/bin
PGO_DIR    := $(CURDIR)/target/pgo-profiles
PGO_MERGED := $(PGO_DIR)/merged.profdata

.PHONY: setup build release release-pgo pgo-profile bench-pgo install pc

setup:
	prek install --install-hooks

build:
	cargo build

release:
	cargo clean -p $(NAME) --release --target $(TARGET)
	RUSTFLAGS="-Zlocation-detail=none -Zunstable-options -Cpanic=immediate-abort" \
	cargo build --release \
	  -Z build-std=std \
	  -Z build-std-features= \
	  --target $(TARGET)

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

pc:
	prek --quiet run --all-files
