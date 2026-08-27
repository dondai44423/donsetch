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
# Optional Chrome for tier 2 browser escalation:
#   docker build --build-arg INSTALL_CHROME=true -t donsetch-mcp .
#
# The container runs the MCP server on stdio. Connect from your MCP
# client:
#   Claude Code: mcp server config using "docker run -i --rm donsetch-mcp donsetch mcp --supervised"
#   Cursor/OpenCode: stdio MCP configuration
#
# Note: build.rs downloads and sha256-verifies the PDFium static archive
# at build time, exactly as the upstream CI does (curl + tar are needed).
# ─────────────────────────────────────────────────────────────────────────────

# Optional: install Chrome for tier 2 browser escalation (bot-wall bypass).
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
# crypto assembly; libclang drives bindgen.
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

# Build all features (OCR + semantic reranking), locked to Cargo.lock
ENV CARGO_TERM_COLOR=always
RUN cargo build --release --features ocr,rerank --locked

# ── Runtime stage ─────────────────────────────────────────────────────────────
# Minimal Debian image for runtime — trixie, matching the builder stage's
# glibc generation (see the glibc note on the builder stage above)
FROM debian:trixie-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        libgcc-s1 \
    && rm -rf /var/lib/apt/lists/*

# Optional: install Chrome for tier 2 browser escalation
ARG INSTALL_CHROME=false
RUN if [ "$INSTALL_CHROME" = "true" ]; then \
        apt-get update && \
        apt-get install -y --no-install-recommends chromium xvfb && \
        rm -rf /var/lib/apt/lists/*; \
    fi

# Copy binary from builder
COPY --from=builder /build/target/release/donsetch /usr/local/bin/donsetch

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

# Cache directory for fetch/search results and ghost browser profile.
# The server resolves it as $HOME/.cache/donsetch (XDG_CACHE_HOME is
# honoured too); no env var is needed. VOLUME makes plain `docker run`
# persist it in an anonymous volume.

# Chromium's sandbox cannot initialize inside a default Docker container
# (no unprivileged user namespaces): the browser dies at launch before the
# DevTools handshake. Upstream's documented container escape hatch
# (`DONGHOST_NO_SANDBOX=1`, see ghost docs) is therefore the correct
# default for this image. The container boundary + non-root user provide
# the isolation the sandbox would on a host.
ENV DONGHOST_NO_SANDBOX=1
VOLUME /home/donsetch/.cache/donsetch

# Set Chrome path if Chrome was installed
RUN if [ "$INSTALL_CHROME" = "true" ]; then \
        echo "chromium found at $(which chromium)" && \
        echo "Set DONGHOST_CHROME=/usr/bin/chromium for browser escalation"; \
    fi

# Healthcheck: verify binary responds to version check
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD donsetch --version || exit 1

# Run the MCP server on stdio with crash-only supervisor.
CMD ["donsetch", "mcp", "--supervised"]
