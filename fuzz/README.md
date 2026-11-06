# DonSeTch fuzz targets

Daemon aborts are the worst failure mode for an MCP server — one
panic kills the session. These targets cover every parser that has
ever panicked or plausibly could (the v2.5 hardening classes:
charset multi-byte, js_unescape, paginate overflow, sitemap bomb).

| Target | Covers |
|---|---|
| `extract` | full DonSift pipeline + reddit/HN/feed/jsdata extractors + wall detection, with hostile offsets/max_chars |
| `charset` | charset decode across labels + malformed multi-byte bodies |
| `paginate` | char-boundary pagination invariants under hostile offsets |
| `sitemap` | gunzip cap + sitemap XML parsing |
| `feed` | RSS/Atom/JSON-feed detection + structured rendering |

## Local runs

```sh
rustup toolchain install nightly
rustup run nightly cargo install cargo-fuzz
cd fuzz
RUSTFLAGS="-C link-arg=-fuse-ld=lld" rustup run nightly cargo fuzz run extract -s none -- -max_total_time=300
```

The `RUSTFLAGS` prefix is only needed on machines where GNU ld
rejects LLVM CREL relocations (for example Void Linux); it is
harmless elsewhere. CI runs a 90s smoke of every target on each
master push; long runs are manual.

When a crash is found: the artifact lands in `fuzz/artifacts/`,
reproduce with `cargo fuzz run <target> -s none artifacts/<file>`,
minimize (`-minimize_crash=1`), fix the parser, and add the input
as a corpus seed (`corpus/<target>/`) so it can never regress.
