# Security Policy

## Reporting a vulnerability

If you discover a security vulnerability in DonSeTch, **use GitHub's private vulnerability reporting** — do not open a public issue.

**[Report a vulnerability →](https://github.com/dondai44423/donsetch/security/advisories/new)**

This creates a private security advisory that only the maintainer can see. You can include details, reproduction steps, and severity assessment directly in the form. GitHub notifies the maintainer immediately.

Include in your report:

- A description of the vulnerability and its impact.
- Steps to reproduce, or a proof of concept.
- The DonSeTch version (`donsetch --version`).
- Your assessment of severity (low / medium / high / critical).

## Response timeline

| Stage | Target |
|---|---|
| Acknowledgment | Within 72 hours of report |
| Triage and severity assessment | Within 7 days |
| Fix development | Within 30 days (severity-dependent) |
| Patch release | Coordinated with reporter |
| Public disclosure | After patch is available |

If the vulnerability is confirmed, a fix will be released and you will be credited in the advisory (unless you prefer to remain anonymous).

## Scope

DonSeTch is a local MCP server that fetches web pages, searches the web, and crawls sites. Security-relevant areas:

- **TLS handling**: the custom BoringSSL stack, certificate validation, and connection pooling.
- **Challenge engine**: the embedded V8 that runs a walled page's own scripts against a bounded host, inside the DonSeTch process.
- **Browser** (`web_screenshot` and `donsetch doctor`'s launch check and `--stealth --parity` capture): CDP communication and the browser process lifecycle.
- **PDF parsing**: the PDFium FFI layer and untrusted-file handling.
- **Search**: the egress pool, proxy handling, and rate-limit backoff.
- **MCP daemon**: JSON-RPC input parsing and stdio handling.

## Out of scope

- Bypassing anti-bot protections on specific sites (that's the tool's purpose, not a vulnerability).
- Rate-limiting or blocking by search engines (expected behavior, not a vulnerability).
- Captcha-solving (intentionally not implemented — not a vulnerability).

## Security posture

DonSeTch defaults to a local stdio MCP process. Optional HTTP transport opens
an explicitly configured listener and supports bearer-token authentication.
Do not expose an unauthenticated listener to untrusted networks.

The default research tools need no API keys or accounts. Optional BYOK
providers store user-supplied credentials in their own config. The fetch state
stores clearance cookies in the local state directory. Credential-bearing state uses
owner-only file permissions on Unix; Windows relies on the state directory's
OS access controls. Cached pages and downloaded model files also live locally.

Outbound requests include the requested research, version/update checks, and
optional model downloads. DonSeTch has no telemetry collection.
PDFium libraries are downloaded at build time; OCR/reranker models are
retrieved when needed. Page reads launch no browser: when a wall needs the
page's own program, its scripts run in an embedded V8 inside the DonSeTch
process. `web_screenshot` and `donsetch doctor --stealth --parity`
(which loads tls.peet.ws) run page
JavaScript inside the installed Chromium-family browser, so that browser and
the operating system should be kept current.

HTTPS over TCP and the opt-in HTTP/3 path verify peer certificates against
the configured trust store and refuse the known compromised CA keys listed
in the transport. This incident blocklist is not a general OS revocation or
Certificate Transparency implementation. HTTP/3 is disabled by default.
