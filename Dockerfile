# syntax=docker/dockerfile:1
# ─────────────────────────────────────────────────────────────────────────────
# DonSeTch MCP server — production image
#
# Builds from the local source tree (this repo).
#
# Build:    docker build -t donsetch-mcp .
# Run:      docker compose run --rm donsetch     (recommended — see docker-compose.yml)
#   or:     docker run -i --rm --init donsetch-mcp donsetch mcp --supervised
#
# Optional Chrome for web_screenshot (see the sandbox note in the runtime
# stage before relying on it):
#   docker build --build-arg INSTALL_CHROME=true -t donsetch-mcp .
#
# The container runs the MCP server on stdio. Connect from your MCP
# client:
#   Claude Code: mcp server config using "docker run -i --rm donsetch-mcp donsetch mcp --supervised"
#   Cursor/OpenCode: stdio MCP configuration
#
# Note: build.rs downloads and sha256-verifies the PDFium static archive
# and the ONNX Runtime archive at build time, exactly as the upstream CI
# does (curl + tar are needed).
# ─────────────────────────────────────────────────────────────────────────────

# Optional: install Chrome for web_screenshot (page reads never use a browser).
# Adds ~350MB. Enable with:  docker build --build-arg INSTALL_CHROME=true .
ARG INSTALL_CHROME=false

# ── Builder stage ─────────────────────────────────────────────────────────────
# Use Rust slim image (Debian-based, edition2024 support in Rust 1.82+)
#
# Both stages must share one modern Debian generation. The ort-sys
# prebuilt ONNX Runtime archives reference glibc 2.38+ symbols
# (__isoc23_strtoll/strtoull), so they will not link against an older
# glibc — and a binary built against a newer glibc will not run on one.
# rust:slim and debian:trixie-slim track Debian trixie together.
FROM rust:slim AS builder

# Install build dependencies for boring-sys (BoringSSL) and PDFium.
# Go is required by BoringSSL's build system; NASM builds the x86_64
# crypto assembly; libclang drives bindgen. No system linker is needed:
# build.rs links PDFium with the Rust toolchain's own rust-lld.
RUN apt-get update && apt-get install -y --no-install-recommends \
        cmake \
        g++ \
        gcc \
        gnupg \
        libclang-dev \
        libssl-dev \
        make \
        nasm \
        curl \
        tar \
        git \
        pkg-config \
        && rm -rf /var/lib/apt/lists/*

# Install Go (required for BoringSSL's build system). TARGETARCH
# (amd64/arm64, injected by BuildKit; falls back to the host arch for
# classic builds) selects the tarball so arm64 builds work too.
ARG TARGETARCH
RUN arch="${TARGETARCH:-$(dpkg --print-architecture)}" \
    && curl -fsSL "https://go.dev/dl/go1.23.4.linux-${arch}.tar.gz" -o /tmp/go.tar.gz \
    && tar -C /usr/local -xzf /tmp/go.tar.gz \
    && rm /tmp/go.tar.gz

ENV PATH="/usr/local/go/bin:${PATH}"

WORKDIR /build

# Copy source code (build.rs must ship: it acquires + links PDFium
# and emits the linux_like cfg)
COPY Cargo.toml Cargo.lock README.md LICENSE ./
COPY build.rs ./
COPY src/ ./src/
COPY crates/ ./crates/
COPY tests/ ./tests/

# Pre-fetch PDFium before cargo build. build.rs vendors the library
# under vendor/pdfium and skips its own download once lib/libpdfium.a
# exists — but its curl path gives up on a single mid-transfer reset
# (curl 35 is not retried without --retry-all-errors). Filtered or
# flaky networks that stall then reset long transfers die there, so
# fetch here first with aggressive retries and stall detection, and
# verify the same sha256 pins build.rs enforces (KNOWN_HASHES) to
# keep the download fail-closed.
ARG TARGETARCH
RUN set -eu; \
    arch="${TARGETARCH:-$(dpkg --print-architecture)}"; \
    pair="linux-x64"; \
    hash="13908bb2d40a6e017c4c5a6a7baecc6efd7b1c30392c8a79e80072d2b48b18eb"; \
    if [ "$arch" = "arm64" ] || [ "$arch" = "aarch64" ]; then \
        pair="linux-arm64"; \
        hash="abe1c3d5b168ec2baaafc7a8fcddfda1a09417f39199c7993fd28d34d3a7f70e"; \
    fi; \
    url="https://github.com/kognitos/pdfium-static/releases/download/chromium/7809/pdfium-${pair}-static.tgz"; \
    mkdir -p vendor/pdfium; \
    curl -fSL --proto '=https' --proto-redir '=https' \
         --retry 8 --retry-all-errors --retry-delay 3 \
         --connect-timeout 20 --speed-time 30 --speed-limit 1024 \
         -o /tmp/pdfium.tgz "$url"; \
    echo "${hash}  /tmp/pdfium.tgz" | sha256sum -c -; \
    tar -xzf /tmp/pdfium.tgz -C vendor/pdfium; \
    rm /tmp/pdfium.tgz

# Build the shipped feature set, locked to Cargo.lock.
ENV CARGO_TERM_COLOR=always
# ocr (ONNX) + http (axum/tower-http streamable-HTTP transport), the same
# set as the release binaries. The rerank feature no longer exists.
RUN cargo build --release --features ocr,http --locked

# Stage the ONNX Runtime shared library for the runtime image. Since
# 3.2.5, ort loads ONNX Runtime dynamically: when ocr/rerank are
# enabled, build.rs builds vendor/onnx/libonnxruntime.so from the pyke
# CDN archive and the binary dlopens it at runtime (searched next to
# the executable first, then the cache dir). Stage only the .so — the
# vendor/onnx dir also holds the large static archive, which the
# runtime image does not need. arm64 has no ONNX prebuilt: stage an
# empty dir there so the runtime COPY is a no-op and ocr/rerank
# self-disable at runtime with a notice instead of failing the build.
RUN mkdir -p /stage-onnx \
    && if [ -f vendor/onnx/libonnxruntime.so ]; then \
           cp vendor/onnx/libonnxruntime.so /stage-onnx/; \
       fi

# ── Runtime stage ─────────────────────────────────────────────────────────────
# Minimal Debian image for runtime — trixie, matching the builder stage's
# glibc generation (see the glibc note on the builder stage above)
FROM debian:trixie-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        libgcc-s1 \
    && rm -rf /var/lib/apt/lists/*

# Optional: install Chrome for web_screenshot
ARG INSTALL_CHROME=false
RUN if [ "$INSTALL_CHROME" = "true" ]; then \
        apt-get update && \
        apt-get install -y --no-install-recommends chromium && \
        rm -rf /var/lib/apt/lists/*; \
    fi

# Copy binary from builder
COPY --from=builder /build/target/release/donsetch /usr/local/bin/donsetch

# ONNX Runtime shared library (ocr + rerank features, dynamic since
# 3.2.5). The binary looks for it next to the executable first, then
# $cache_dir/onnx/ — /usr/local/bin satisfies the first even when an
# older cache volume shadows the second. Empty dir on arm64: nothing
# is copied and the features degrade at runtime.
COPY --from=builder /stage-onnx/ /usr/local/bin/

# Verify binary works
RUN donsetch --version

# ── Non-root runtime user ────────────────────────────────────────────────────
# The cache dir must exist and be donsetch-owned before the VOLUME
# declaration below: when it is absent from the image, the daemon
# creates a root-owned mountpoint and fresh volumes stay root-owned,
# so the non-root user can neither write the fetch cache nor download
# the rerank model on first use.
RUN useradd -m -u 1000 donsetch \
    && mkdir -p /home/donsetch/.cache/donsetch \
    && chown -R donsetch:donsetch /home/donsetch/.cache
USER donsetch
WORKDIR /home/donsetch

# Cache directory for fetch/search results and stored login sessions.
# The server resolves it as $HOME/.cache/donsetch (XDG_CACHE_HOME is
# honoured too); no env var is needed. VOLUME makes plain `docker run`
# persist it in an anonymous volume.

# Chromium launches with its sandbox on. 5.0.0-beta.1 removed
# `browser.no_sandbox` / `DONGHOST_NO_SANDBOX`, the switch this image used
# to set, and nothing else turns the sandbox off. The sandbox needs
# unprivileged user namespaces, which a default Docker container does not
# provide, so with INSTALL_CHROME=true expect web_screenshot and doctor's
# browser check to fail at launch unless the container allows them. Not yet
# tested in this image.
VOLUME /home/donsetch/.cache/donsetch

# Report the installed Chrome. Browser discovery finds /usr/bin/chromium
# without DONGHOST_CHROME.
RUN if [ "$INSTALL_CHROME" = "true" ]; then \
        echo "chromium found at $(which chromium)" && \
        echo "web_screenshot finds it without DONGHOST_CHROME"; \
    fi

# Healthcheck: verify binary responds to version check
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD donsetch --version || exit 1

# Port for the HTTP transport (compose activates it via
# DONSETCH_TRANSPORT=http in the http profile).
EXPOSE 8765

# Run the MCP server on stdio with crash-only supervisor.
# HTTP mode: docker run -d -p 127.0.0.1:8765:8765 \
#   -e DONSETCH_TRANSPORT=http -e DONSETCH_HTTP_HOST=0.0.0.0 donsetch-mcp
CMD ["donsetch", "mcp", "--supervised"]
