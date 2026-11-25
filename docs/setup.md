# Setup

Install DonSeTch, connect an agent, and configure the local process. [Back to the README](../README.md).

## Install a release

```bash
npm install -g donsetch
donsetch doctor
```

The npm package has no install lifecycle scripts, so npm, pnpm and bun need no script approval. The first command (`donsetch doctor` above) downloads the matching release asset and verifies its SHA256. The tools run in the Rust binary. Pi prepares the same binary at session start.

The binary normally lives inside the package. If a system-wide package is read-only, it uses a versioned directory under your user cache instead (`donsetch/npm/<version>/<platform>`). `DONSETCH_CACHE_DIR` relocates that cache; `DONSETCH_BINARY` selects an existing executable for both CLI and Pi. No sudo is needed for the first-run download.

Windows release archives include the required Visual C++ runtime DLLs beside `donsetch.exe`; installing a machine-wide redistributable is not required.

| Platform | Asset | OCR / HTTP transport |
|---|---|---|
| Linux x86_64 · glibc ≥ 2.35 | `donsetch-linux-x64.tar.gz` | Both |
| Linux ARM64 · glibc ≥ 2.35 | `donsetch-linux-arm64.tar.gz` | Neither; fetch, search, crawl, PDF, and screenshots remain available |
| macOS Apple Silicon · ≥ 13.4 for OCR | `donsetch-darwin-arm64.tar.gz` | Both |
| macOS Intel · ≥ 13.4 for OCR | `donsetch-darwin-x64.tar.gz` | Both |
| Windows x86_64 | `donsetch-win32-x64.tar.gz` | Both |
| Windows ARM64 | The x64 asset under emulation | Both |

The Linux ARM64 package omits ONNX because its native runtime deadlocks during loading. Search uses no ONNX on any platform. Published Linux assets use glibc; musl/Alpine needs a source build.

### Other installers

```bash
# Homebrew: macOS or Linux.
brew tap dondai44423/donsetch
brew install donsetch

# Pi: native tools, managed with pi update --extensions.
pi install npm:donsetch

# DeepSeek Harness: native donsetch_* tools.
dsh plugin --profile web add github:dondai44423/donsetch-dsh
```

Homebrew and the [dsh plugin](https://github.com/dondai44423/donsetch-dsh) track published releases.

## MCP over stdio

Add a server entry to your client's MCP configuration:

```json
{
  "mcpServers": {
    "donsetch": {
      "command": "donsetch",
      "args": ["mcp", "--supervised"]
    }
  }
}
```

Restart the client after editing its configuration. The supervised daemon restarts after a crash. Clients differ in their config format and location; run:

```bash
donsetch doctor --mcp
```

This prints absolute-path registration examples. Use an absolute binary path if a desktop client cannot find the command on its own `PATH`.

Without a global install, an npm launcher entry can use `"command": "npx"` and `"args": ["-y", "donsetch", "mcp", "--supervised"]`.

## HTTP transport

Requires a build with the `http` feature; see the release matrix above.

```bash
donsetch mcp --http --port 8765
```

| Endpoint | Purpose |
|---|---|
| `http://localhost:8765/mcp` | Streamable HTTP MCP |
| `http://localhost:8765/health` | Health check |

Set `DONSETCH_HTTP_TOKEN` to require bearer authentication. Set `DONSETCH_HTTP_HOST` when choosing a bind address. Keep the default loopback binding unless remote access is deliberate; configure a token before exposing the server to a network.

Browser-origin requests are restricted. `DONSETCH_HTTP_CORS` enables CORS mode and requires a token. A loopback listener also validates the request's `Host`. For session, cancellation, timeout, and transport options, use `donsetch mcp --help`.

## Browser for screenshots

Fetch, search, and crawl need no browser. Screenshots need an installed Chromium-family browser: Chromium, Chrome, or Edge on Windows.

DonSeTch checks normal install locations, Playwright browser caches, and `PATH`. It does not download a browser. To choose one:

```toml
[browser]
chromium_path = "/path/to/chrome"
```

The environment equivalent is `DONSETCH_BROWSER__CHROMIUM_PATH`. Legacy `DONGHOST_CHROME` is still supported. A custom Playwright cache can be named with `browser.playwright_path` or `PLAYWRIGHT_BROWSERS_PATH`.

On hosts that cannot install Chrome locally (such as a NAS or slim container), point screenshots at an external Chromium DevTools Protocol endpoint instead:

```toml
[browser]
cdp_url = "http://192.168.1.50:9222" # or ws://.../devtools/browser/...
```

The environment equivalent is `DONSETCH_BROWSER__CDP_URL`. Each capture creates an isolated incognito browser context, enforces the same SSRF and URL rules on requests, and disposes only its own target and context when finished.

Screenshots use a throwaway profile with no signed-in cookies. `donsetch doctor` includes a browser launch check.

## Configuration

Create `donsetch.toml` under your platform's config directory:

| OS | Default file |
|---|---|
| Linux | `$XDG_CONFIG_HOME/donsetch/donsetch.toml`, or `~/.config/donsetch/donsetch.toml` |
| macOS | `~/Library/Application Support/donsetch/donsetch.toml` |
| Windows | `%APPDATA%\donsetch\donsetch.toml` |

Set `DONSETCH_CONFIG=/absolute/path/donsetch.toml` to use another file. A bare install needs none.

```toml
[fetch]
pdf_max_mb = 100
dns_cache_ttl_secs = 30

[bypass]
max_daily = 20
```

Later layers win:

1. Compiled defaults.
2. Supported legacy environment names.
3. The config file.
4. `DONSETCH_<SECTION>__<KEY>` environment names.

For example, `DONSETCH_FETCH__PDF_MAX_MB=25` overrides `fetch.pdf_max_mb`. Unknown file keys and invalid values are errors, with their source named.

| Command | Purpose |
|---|---|
| `donsetch config show` | Current values and their origin |
| `donsetch config show --markdown` | Full settings reference |
| `donsetch config show --legacy` | Old environment names and their replacements |
| `donsetch status --json` | Machine-readable version, config, key, proxy, and cache state |

`DONSETCH_NO_CONFIG_FILE=1` skips the file layer; do not combine it with `DONSETCH_CONFIG`. `DONSETCH_CACHE_DIR` relocates the cache. `DONSEEK_NO_DISK_STATE=1` keeps learned state in memory.

For host policies, use [local rules](rules.md). For egress configuration, use the [proxy guide](proxies.md).

## Docker

Prebuilt multi-arch images (`linux/amd64`, `linux/arm64`) are published to GitHub Container Registry on each release:

```bash
# Stdio process for an MCP client.
docker run -i --rm --init ghcr.io/dondai44423/donsetch:latest

# Or start the streamable HTTP service.
docker run -d -p 127.0.0.1:8765:8765 \
  -e DONSETCH_TRANSPORT=http -e DONSETCH_HTTP_HOST=0.0.0.0 \
  ghcr.io/dondai44423/donsetch:latest
```

To build locally from a checkout instead:

```bash
docker build -t donsetch-mcp .

# Stdio process for an MCP client.
docker run -i --rm donsetch-mcp donsetch mcp --supervised

# Or start the opt-in HTTP service.
docker compose --profile http up -d donsetch-http
```

The [compose file](../docker-compose.yml) binds HTTP to `127.0.0.1:8765`, persists the cache in a volume, and gives in-flight work a 45-second shutdown grace period. The image runs as a non-root user, includes a transport-aware healthcheck (`DONSETCH_TRANSPORT=http` probes the local HTTP port), and accepts `DONSETCH_SEARCH__SEARXNG_URL` (for an existing SearXNG/Degoog container) and `DONSETCH_BROWSER__CDP_URL` (for an external Chromium CDP container). Set the `INSTALL_CHROME=true` build argument if screenshots need Chrome inside the container.

## Build from source

Use Rust 1.98+ (the repo pins its toolchain), Go 1.22+, NASM, CMake, and Clang/libclang.

| Tool | Debian/Ubuntu | macOS | Windows |
|---|---|---|---|
| Rust | `rustup` | `rustup` | `rustup` |
| Go | `golang-go` | `brew install go` | `winget install GoLang.Go` |
| NASM | `nasm` | `brew install nasm` | `choco install nasm` |
| CMake | `cmake` | `brew install cmake` | `winget install cmake` |
| Clang | `clang libclang-dev` | Xcode command-line tools | `choco install llvm` |

```bash
git clone https://github.com/dondai44423/donsetch.git
cd donsetch
cargo build --release --features ocr,http
```

Default Cargo features are empty: fetch, search, crawl, PDF, and the screenshot command. `ocr` adds ONNX and `http` adds HTTP MCP transport. OCR downloads its model, about 37 MB, on first use. Search downloads nothing.

The first build compiles BoringSSL and downloads a prebuilt V8 archive. `RUSTY_V8_ARCHIVE` can point to a local archive for an offline build. Linux PDFium archives require CREL-capable linking; `build.rs` uses the Rust toolchain's `rust-lld` or a system LLD ≥ 20. Do not force an older linker.

For development, use the [contributor build ladder](../CONTRIBUTING.md#development-workflow), not repeated release builds.

## Troubleshooting

| Symptom | Try |
|---|---|
| First command is downloading | Allow the one-time verified binary download to finish. Later starts use the installed copy. |
| Installed with `--ignore-scripts` or strict script policy | No extra step: run `donsetch doctor` or start Pi. DonSeTch has no install scripts. |
| `npm install -g` fails with `EACCES` | Use `npx -y donsetch doctor`, or create `~/.local/lib` with `mkdir -p ~/.local/lib`, run `npm install -g --prefix "$HOME/.local" donsetch`, then `~/.local/bin/donsetch doctor`. Add `~/.local/bin` to your shell's `PATH` for later use. |
| Package directory is read-only at runtime | The launcher downloads into your user cache automatically. `DONSETCH_BINARY` can select an existing binary; `PATH` and `~/.cargo/bin` are fallback locations. |
| Download needs a proxy | Set `HTTPS_PROXY`, `HTTP_PROXY`, or `ALL_PROXY` to an HTTP CONNECT proxy. |
| GitHub Releases is inaccessible | Set `DONSETCH_RELEASES_BASE` to a mirror with `<tag>/<asset>` paths; default installs also fall back automatically to the GitHub Releases API mirror. |
| Interrupted or throttled first-run download | Re-running resumes automatically via HTTP `Range` requests, boosts throttled multi-MB streams across parallel connections, and extracts in-process without external `tar` or `gzip`. |
| Screenshot cannot launch | Run `doctor`, install Chromium/Chrome/Edge, or set `browser.chromium_path`. |
| Search or egress is degraded | Run `doctor --deep`; inspect source status and [proxy checks](proxies.md#troubleshooting). |
| A client still runs the old binary | Restart its MCP process after updating. |

`donsetch doctor --fix` repairs mechanical problems it can identify. `--json` adds machine-readable checks. `donsetch update` (`--check`, `--force`) installs or refreshes a release; `donsetch rollback` (`--check`) restores the previous binary. A major-version update asks for explicit confirmation; `--yes` is the unattended opt-in.
