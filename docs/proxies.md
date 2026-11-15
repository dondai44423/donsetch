# Proxies

Choose the route each tool uses, add a pool, and verify where requests leave. [Back to the README](../README.md).

## Start with one proxy

Replace the example host and credentials with your provider's values:

```bash
donsetch proxy add 'http://user:pass@proxy.example:8080'
# Or:
donsetch proxy add 'socks5://user:pass@proxy.example:1080'

donsetch proxy list
donsetch proxy check
```

`add` validates, tests, and saves the proxy. `check` opens a real tunnel to `api.ipify.org` and reports the exit IP and latency. An accepted TCP connection alone is not a pass.

**Restart the MCP server after editing the pool.** The pool is loaded at startup. `list` masks credentials.

## What uses the pool

| Tool | Default | Switch |
|---|---|---|
| Search | Distributes engine requests across pool lanes, sticky per host | Uses the configured pool automatically |
| Crawl | Starts each host on the direct lane; later requests can spread across pool lanes as pacing permits | `donsetch proxy crawl off` / `on` |
| Fetch | Direct, outside the pool | `donsetch proxy fetch on` / `off` |

Direct means “no pool lane.” An environment/configured proxy can still carry that route; see [environment proxies](#environment-proxies).

With fetch pool rotation enabled, a host stays on its selected lane and rotates on 429, 407, a dead connection, or a timeout. A failed configured proxy is reported instead of falling back silently to your own IP. The challenge/content engine uses the read's route too.

**A pool is not an IP-concealment guarantee for crawl.** The host's initial direct request has already happened before pool lanes can take over. To exclude a host entirely, set a [local deny rule](rules.md).

## Supported addresses

| Format | Meaning |
|---|---|
| `http://host:port` | HTTP CONNECT |
| `http://user:pass@host:port` | HTTP CONNECT with authentication |
| `socks5://host:port` | SOCKS5; the proxy resolves the target name |
| `socks5h://host:port` | Alias for the same SOCKS5 behavior |
| `host:port` | Shorthand for HTTP CONNECT |

Credentials must be URL-encoded: `p%40ss` means `p@ss`; a literal percent is `%25`. Quote a proxy URL in your shell, particularly when credentials contain punctuation.

Try a proxy before saving it:

```bash
donsetch proxy test 'http://user:pass@proxy.example:8080'
```

## Add or remove a pool

```bash
# Several at once.
donsetch proxy add 'http://first.example:8080' 'socks5://second.example:1080'

# One address per line; # starts a comment.
donsetch proxy import proxies.txt

# Print an importable file. This includes credentials.
donsetch proxy export > proxies-backup.txt

# Use the index from proxy list, or a host:port.
donsetch proxy remove 1
donsetch proxy clear
```

`--no-check` skips live validation when adding a large set. It does not prove the proxies work.

Three sources merge into the pool:

| Source | Example |
|---|---|
| Saved file | `<cache-dir>/proxies.txt`; created by `proxy add` / `import` |
| Environment | `DONSEEK_PROXIES="http://first.example:8080,socks5://second.example:1080"` |
| Config | `[proxy] pool = ["http://first.example:8080"]` |

Environment/config entries replace saved-file entries at the same `host:port`. The saved file contains credentials and uses mode 0600 on Unix; protect exported copies too.

## Environment proxies

To route the configured direct path through an existing proxy, use the curl-style environment:

```bash
export HTTPS_PROXY='http://user:pass@proxy.example:8080'
export HTTP_PROXY="$HTTPS_PROXY"
export NO_PROXY='localhost,127.0.0.1,.internal.example'
donsetch fetch https://example.com
```

`HTTPS_PROXY`, `HTTP_PROXY`, and `ALL_PROXY` are supported, including lowercase names. `NO_PROXY` supports host suffixes, `*`, `host:port`, and CIDR exclusions. `DONSETCH_NO_ENV_PROXY=1` disables environment-proxy handling.

The config-file equivalent:

```toml
[proxy]
https = "http://user:pass@proxy.example:8080"
http = "http://user:pass@proxy.example:8080"
# all = "socks5://user:pass@proxy.example:1080"
```

Keep credential-bearing config private. This is distinct from the rotating pool: choose the route you intend, then verify it with `doctor --deep`.

## TLS-inspecting networks

Install the organization's root through the OS trust store, or supply `SSL_CERT_FILE` / `SSL_CERT_DIR`. DonSeTch adds these to its bundled roots. Config also accepts `[tls] cert_file` and `cert_dir`. The client identity's handshake profile is preserved.

## Troubleshooting

| Problem | Check |
|---|---|
| Pool added, but fetch still uses your IP | Run `donsetch proxy fetch on`, then restart MCP. |
| New proxy is missing from an agent session | Restart its MCP server; the pool is read at startup. |
| Proxy accepts TCP but cannot fetch | Run `proxy check`; it verifies a tunnel and exit IP. |
| Authentication fails | Confirm the scheme, provider credentials, and URL encoding of `@`, `%`, and other reserved characters. |
| Requests unexpectedly use a proxy | Inspect `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`, and `donsetch config show`. |
| A configured lane dies | Inspect the named failure; DonSeTch does not replace it with a direct request to that host. |
| Crawl contacts your IP first | That is its default. A local deny rule can keep the host out entirely. |
| A proxy still receives a bot wall | Its exit network may be refused too. A working tunnel does not establish access to every target. |

Use `donsetch proxy list`, `donsetch proxy check`, and `donsetch doctor --deep` to inspect the configuration, tunnel, and wider network posture respectively.
