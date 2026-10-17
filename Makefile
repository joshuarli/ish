NAME       := ish
TEST_BINARY_ENV := ISH_TEST_BINARY
TEST_TARGETS := --lib --tests
HOST       := $(shell rustc -vV | awk '/^host:/ {print $$2}')
TARGET     ?= $(subst -unknown-linux-gnu,-unknown-linux-musl,$(HOST))
TARGET_ENV := $(shell printf '%s' "$(TARGET)" | tr '[:lower:]-' '[:upper:]_')
CARGO_CMD  := $(if $(findstring -linux-musl,$(TARGET)),$(shell command -v musl-cargo 2>/dev/null || printf cargo),cargo)
# Docker's musl-cargo wrapper owns linker, CRT, and loader flags. The Makefile
# selects only the build mode and keeps Cargo invocations readable on macOS as
# well as in the Linux release image.
cargo = $(if $(findstring -linux-musl,$(TARGET)),,$(if $(filter release-static release-dynamic test,$(1)),CARGO_TARGET_$(TARGET_ENV)_LINKER=clang ,)$(if $(filter release-static release-dynamic,$(1)),RUSTFLAGS="-Zlocation-detail=none -Zunstable-options -Cpanic=immediate-abort")) MUSL_TARGET="$(TARGET)" MUSL_BUILD_MODE="$(1)" $(CARGO_CMD)

RUSTYBENCH ?= cargo run --quiet --manifest-path ../rustybench/Cargo.toml --

.PHONY: build test test-epsh conformance verify check test-ci release release-dynamic verify-release verify-release-dynamic bench bench-syscalls install install-login-shell lint fix

build:
	$(call cargo,dev) build

test:
	$(call cargo,test) test --quiet

test-epsh:
	@mkdir -p .work/tmp
	TMPDIR="$(CURDIR)/.work/tmp" cargo test -p epsh --lib --tests

conformance:
	@mkdir -p .work/tmp
	TMPDIR="$(CURDIR)/.work/tmp" cargo test -p epsh --test conformance -- --nocapture

verify:
	@mkdir -p .work/tmp
	cd formal && TMPDIR="$(CURDIR)/.work/tmp" lake build semanticBridge protocolTests IshSemantics.Audit
	cd formal && TMPDIR="$(CURDIR)/.work/tmp" lake exe protocolTests
	cd formal && TMPDIR="$(CURDIR)/.work/tmp" lake env lean IshSemantics/Audit.lean
	TMPDIR="$(CURDIR)/.work/tmp" cargo run --quiet -p ish --example semantic_bridge -- --cases 1000

check:
	@mkdir -p .work/tmp
	TMPDIR="$(CURDIR)/.work/tmp" cargo clippy --workspace --all-targets -- -D warnings
	TMPDIR="$(CURDIR)/.work/tmp" cargo test --workspace --lib --tests
	$(MAKE) verify

test-ci:
	@test -x "target/$(TARGET)/release/$(NAME)"
	$(TEST_BINARY_ENV)="$(CURDIR)/target/$(TARGET)/release/$(NAME)" \
	$(call cargo,test) test --quiet --release $(TEST_TARGETS)

release:
	$(call cargo,release-static) clean -p $(NAME) --release --target $(TARGET)
	$(call cargo,release-static) build --release -p $(NAME) \
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
	cargo clippy --workspace --fix --allow-dirty --all-targets -- -D warnings

bench:
	@$(RUSTYBENCH) baseline --root "$(CURDIR)" --baseline "$(CURDIR)/benches/baseline.json" -- cargo bench --bench bench

bench-syscalls:
	@$(RUSTYBENCH) syscalls --root "$(CURDIR)"

release-dynamic:
	$(call cargo,release-dynamic) clean -p $(NAME) --release --target $(TARGET)
	$(call cargo,release-dynamic) build --release -p $(NAME) \
	  -Z build-std=std \
	  -Z build-std-features= \
	  --target $(TARGET)

install: release
	@set -eu; \
	  install_dir="$(HOME)/usr/bin"; \
	  mkdir -p "$$install_dir"; \
	  install -m 755 "target/$(TARGET)/release/$(NAME)" "$$install_dir/$(NAME)"; \
	  if test "$$(uname -s)" = Darwin; then codesign -fs - "$$install_dir/$(NAME)"; fi

# Registers the installed binary in /etc/shells and makes it the login shell.
# Separate from install because it needs privilege and changes account state.
install-login-shell: install
	@set -eu; \
	  install_dir="$(HOME)/usr/bin"; \
	  shell_path="$$(cd "$$install_dir" && pwd -P)/$(NAME)"; \
	  login_user="$$(id -un)"; \
	  test -f /etc/shells || { echo '/etc/shells is required to register ish as a login shell' >&2; exit 1; }; \
	  command -v chsh >/dev/null 2>&1 || { echo 'chsh is required to set the login shell' >&2; exit 1; }; \
	  if test "$$(uname -s)" = Darwin; then \
	    current_shell="$$(id -P "$$login_user" | awk -F: 'NF >= 8 { print $$8; exit }')"; \
	  elif command -v getent >/dev/null 2>&1; then \
	    current_shell="$$(getent passwd "$$login_user" | awk -F: -v user="$$login_user" '$$1 == user { print $$7; exit }')"; \
	  else \
	    current_shell="$$(awk -F: -v user="$$login_user" '$$1 == user { print $$7; exit }' /etc/passwd)"; \
	  fi; \
	  test -n "$$current_shell" || { echo "could not determine the current shell for $$login_user" >&2; exit 1; }; \
	  needs_setup=0; \
	  if ! grep -Fqx -- "$$shell_path" /etc/shells || test "$$current_shell" != "$$shell_path"; then needs_setup=1; fi; \
	  privilege=; \
	  if test "$$needs_setup" -eq 1 && test "$$(id -u)" -ne 0; then \
	    if command -v sudo >/dev/null 2>&1; then privilege=sudo; \
	    elif command -v doas >/dev/null 2>&1; then privilege=doas; \
	    else echo 'sudo or doas is required to register ish as a login shell' >&2; exit 1; fi; \
	  fi; \
	  if test "$$needs_setup" -eq 1; then \
	    run_privileged() { if test -n "$$privilege"; then "$$privilege" "$$@"; else "$$@"; fi; }; \
	    run_privileged sh -c 'set -eu; \
	      shell_path=$$1; login_user=$$2; current_shell=$$3; \
	      if ! grep -Fqx -- "$$shell_path" /etc/shells; then printf "%s\n" "$$shell_path" >> /etc/shells; fi; \
	      if test "$$current_shell" != "$$shell_path"; then chsh -s "$$shell_path" "$$login_user"; fi' \
	      ish "$$shell_path" "$$login_user" "$$current_shell"; \
	  fi
