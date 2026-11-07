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
rustup run nightly cargo fuzz run extract -s none -- -max_total_time=300
```

The linked PDFium archives use CREL relocations, which a GNU ld before 2.44
and an lld before LLVM 20 skip without an error (the binary then dies before
`main`). x86_64 Linux toolchains link with their own `rust-lld` by default, so
nothing is needed there. On a host that defaults to GNU ld (aarch64, say), point
the link at the toolchain's lld instead of a system one:

```sh
T=$(rustup run nightly rustc -vV | sed -n 's/^host: //p')
D=$(rustup run nightly rustc --print sysroot)/lib/rustlib/$T/bin/gcc-ld
RUSTFLAGS="-C link-arg=-fuse-ld=lld -C link-arg=-B$D" rustup run nightly cargo fuzz run extract -s none -- -max_total_time=300
```

CI runs a 90s smoke of every target on each
master push; long runs are manual.

When a crash is found: the artifact lands in `fuzz/artifacts/`,
reproduce with `cargo fuzz run <target> -s none artifacts/<file>`,
minimize (`-minimize_crash=1`), fix the parser, and add the input
as a corpus seed (`corpus/<target>/`) so it can never regress.
