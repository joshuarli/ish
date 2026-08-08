FROM alpine:3.24.1

ARG TARGETARCH
ARG LLVM_VERSION=23.1.0-rc2
ARG LLVM_RELEASE_SHA=6eb5fb9

RUN apk add --no-cache \
    binutils \
    bash \
    file \
    make \
    strace \
    musl-dev \
    libgcc \
    git \
    tzdata

# rustix requests libutil for Unix PTY support; musl provides those symbols in
# libc, so an empty archive satisfies the legacy library name without adding a
# glibc runtime dependency.
RUN ar rcs /usr/lib/libutil.a

# Use the same prebuilt LLVM family as the Linux CI image. The archive is
# musl-linked so clang, lld, and the LLVM tools run inside Alpine.
ADD https://github.com/laputa-systems/llvm-prebuilt-musl/releases/download/llvm-musl-${LLVM_VERSION}-${LLVM_RELEASE_SHA}/clang+llvm-${LLVM_VERSION}-x86_64-linux-musl.tar.xz /tmp/llvm-x86_64.tar.xz
ADD https://github.com/laputa-systems/llvm-prebuilt-musl/releases/download/llvm-musl-${LLVM_VERSION}-${LLVM_RELEASE_SHA}/clang+llvm-${LLVM_VERSION}-aarch64-linux-musl.tar.xz /tmp/llvm-aarch64.tar.xz
RUN case "$TARGETARCH" in \
        amd64) archive=/tmp/llvm-x86_64.tar.xz ;; \
        arm64) archive=/tmp/llvm-aarch64.tar.xz ;; \
        *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac \
    && echo '36647cca0bf57d206a6ce757d07a9d8489ef6ccf283a2cc7f740d1cba99a088b  /tmp/llvm-x86_64.tar.xz' | sha256sum -c - \
    && echo '0c9bd6f0fefa26dbdb7d6ed568f3799b558428b1ce1264656aa328fc6fd9e32d  /tmp/llvm-aarch64.tar.xz' | sha256sum -c - \
    && mkdir -p /opt/llvm-musl \
    && tar xf "$archive" -C /opt/llvm-musl --strip-components=1 \
    && test -f /opt/llvm-musl/lib/libclang.so \
    && rm /tmp/llvm-x86_64.tar.xz /tmp/llvm-aarch64.tar.xz

# Rust's static musl profiler link asks lld for -lgcc. Keep the linker
# interface while resolving it to the LLVM compiler-rt builtins shipped in
# the prebuilt toolchain; this image must not depend on GCC.
RUN case "$TARGETARCH" in \
        amd64) llvm_arch=x86_64 ;; \
        arm64) llvm_arch=aarch64 ;; \
        *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac \
    && clang_major="${LLVM_VERSION%%.*}" \
    && builtins="/opt/llvm-musl/lib/clang/$clang_major/lib/linux/libclang_rt.builtins-$llvm_arch.a" \
    && test -f "$builtins" \
    && ln -sf "$builtins" /usr/lib/libcompiler-rt-builtins.a

RUN for target in x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do \
        stub_dir="/usr/lib/e-crt/$target"; \
        mkdir -p "$stub_dir"; \
        for obj in crtbegin.o crtbeginS.o crtbeginT.o crtend.o crtendS.o; do \
            /opt/llvm-musl/bin/clang --target="$target" -x c -c /dev/null -o "$stub_dir/$obj"; \
        done; \
    done

ADD https://static.rust-lang.org/rustup/dist/x86_64-unknown-linux-musl/rustup-init /rustup-init-x86_64
ADD https://static.rust-lang.org/rustup/dist/aarch64-unknown-linux-musl/rustup-init /rustup-init-aarch64
RUN case "$TARGETARCH" in \
        amd64) init=/rustup-init-x86_64 ;; \
        arm64) init=/rustup-init-aarch64 ;; \
        *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac \
    && cp "$init" /rustup-init \
    && chmod +x /rustup-init \
    && /rustup-init -y --default-toolchain none \
    && rm /rustup-init /rustup-init-x86_64 /rustup-init-aarch64

ENV PATH="/opt/llvm-musl/bin:/root/.cargo/bin:$PATH" \
    CC="/opt/llvm-musl/bin/clang" \
    AR="/opt/llvm-musl/bin/llvm-ar" \
    RANLIB="/opt/llvm-musl/bin/llvm-ranlib" \
    LIBRARY_PATH="/opt/llvm-musl/lib" \
    LIBCLANG_PATH="/opt/llvm-musl/lib" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER="/opt/llvm-musl/bin/clang" \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER="/opt/llvm-musl/bin/clang"

RUN rustup toolchain install nightly-2026-07-24 \
    --target x86_64-unknown-linux-musl \
    --target aarch64-unknown-linux-musl \
    --component rust-src \
    --component llvm-tools-preview

# Alpine keeps these libraries outside the Rust musl target directory.
RUN host_libdir="$(rustc --print target-libdir)" \
    && ln -sf /usr/lib/libgcc_s.so.1 "$host_libdir/libgcc_s.so" \
    && ln -sf /usr/lib/libgcc_s.so.1 "$host_libdir/libgcc_s.so.1" \
    && ln -sf /usr/lib/libc.so "$host_libdir/libc.so"
