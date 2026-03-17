NAME       := ish
TARGET     := $(shell rustc -vV | awk '/^host:/ {print $$2}')

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

pc:
	prek --quiet run --all-files
