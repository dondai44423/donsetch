#!/usr/bin/env node
'use strict';

// First-run bootstrap: download the prebuilt binary for this platform
// from GitHub Releases, verify SHA256, and extract it in-process.
//
// The installer is a library first: `install()` runs in the CALLING
// process. resolve-binary.js imports it instead of spawning a script,
// because the host that needs it can be a compiled executable — Pi-Bolt
// is a Bun-compiled pi, so `process.execPath` is that host, not a JS
// runtime, and `<host> install.js --bin-dir DIR` is rejected as an
// unknown option (issue #450). Nothing here calls process.exit or
// writes to stdout unless invoked directly as a CLI (`require.main === module`):
// an MCP stdio session owns stdout outright.
//
// Extraction uses Node's built-in `zlib` + an in-process POSIX ustar/PAX/GNU
// tar reader so installs never depend on external `tar` or `gzip` binaries
// (fixing minimal Linux containers and Windows GNU-tar `C:` drive-colon bugs).

const http = require('http');
const https = require('https');
const net = require('net');
const tls = require('tls');
const zlib = require('zlib');
const crypto = require('crypto');
const fs = require('fs');
const path = require('path');
const { execFileSync } = require('child_process');
const { findExistingBinary } = require('./resolve-binary.js');

const REPO = 'dondai44423/donsetch';
const VERSION = require('./package.json').version;
const REQUEST_TIMEOUT_MS = 30_000;
const MAX_REDIRECTS = 5;
const RETRIES = 3;
const RETRY_BACKOFF_MS = [1_000, 3_000];

// Parallel range booster for throttled GitHub CDN connections on >= 2 MiB assets.
const PARALLEL_FLOOR_BYTES = 2 * 1024 * 1024;
const PARALLEL_WARMUP_MS = 4_000;
const PARALLEL_MIN_BPS = 48 * 1024;
const PARALLEL_LANES = 6;

const PLATFORMS = {
  'linux-x64':    { label: 'linux-x64      Linux x86_64 (glibc)',           asset: 'donsetch-linux-x64.tar.gz',    binary: 'donsetch' },
  'linux-arm64':  { label: 'linux-arm64    Linux ARM64 (glibc)',            asset: 'donsetch-linux-arm64.tar.gz',  binary: 'donsetch' },
  'darwin-arm64': { label: 'darwin-arm64   macOS Apple Silicon',            asset: 'donsetch-darwin-arm64.tar.gz', binary: 'donsetch' },
  'darwin-x64':   { label: 'darwin-x64     macOS Intel',                    asset: 'donsetch-darwin-x64.tar.gz',   binary: 'donsetch' },
  'win32-x64':    { label: 'win32-x64      Windows x86_64',                 asset: 'donsetch-win32-x64.tar.gz',    binary: 'donsetch.exe' },
  'win32-arm64':  { label: 'win32-arm64    Windows ARM64 (x64 emulation)',  asset: 'donsetch-win32-x64.tar.gz',    binary: 'donsetch.exe', emulated: true },
};

// A failure the CLI reports in its own words; `code` names the class so
// the message can stay one line for a caller that shows only that.
class InstallError extends Error {
  constructor(message, code) {
    super(message);
    this.name = 'InstallError';
    this.code = code;
  }
}

function platformFor(platform, arch) {
  return PLATFORMS[`${platform}-${arch}`] || null;
}

function supportedPlatforms() {
  return Object.values(PLATFORMS).map((p) => p.label);
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

// ── Cross-platform terminal palette & kinetic progress renderer ──

function detectColorTier(stream) {
  if (!stream || !stream.isTTY) return 0;
  if (process.env.NO_COLOR !== undefined) return 0;
  const term = (process.env.TERM || '').toLowerCase();
  if (term === 'dumb') return 0;
  const colorterm = (process.env.COLORTERM || '').toLowerCase();
  const prog = (process.env.TERM_PROGRAM || '').toLowerCase();
  if (
    colorterm === 'truecolor' ||
    colorterm === '24bit' ||
    process.env.WT_SESSION ||
    process.env.ITERM_SESSION_ID ||
    ['wezterm', 'vscode', 'ghostty', 'hyper', 'alacritty', 'zed'].includes(prog)
  ) {
    return 3; // 24-bit truecolor
  }
  if (term.includes('256color') || process.platform === 'darwin' || process.platform === 'win32') {
    return 2; // 256-color safe (e.g. macOS Terminal.app)
  }
  return 1; // 16-color ANSI
}

function makePalette(tier) {
  const wrap = (open, s) => (tier > 0 ? `${open}${s}\x1b[0m` : String(s));
  if (tier === 3) {
    return {
      crimson: (s) => wrap('\x1b[38;2;229;43;49m', s),
      ember:   (s) => wrap('\x1b[38;2;255;107;74m', s),
      trail:   (s) => wrap('\x1b[38;2;135;34;40m', s),
      gold:    (s) => wrap('\x1b[38;2;245;166;35m', s),
      emerald: (s) => wrap('\x1b[38;2;46;204;113m', s),
      cyan:    (s) => wrap('\x1b[38;2;86;182;194m', s),
      muted:   (s) => wrap('\x1b[38;2;124;130;145m', s),
      track:   (s) => wrap('\x1b[38;2;62;66;78m', s),
      bold:    (s) => wrap('\x1b[1m', s),
    };
  }
  if (tier === 2) {
    return {
      crimson: (s) => wrap('\x1b[38;5;196m', s),
      ember:   (s) => wrap('\x1b[38;5;203m', s),
      trail:   (s) => wrap('\x1b[38;5;88m', s),
      gold:    (s) => wrap('\x1b[38;5;214m', s),
      emerald: (s) => wrap('\x1b[38;5;42m', s),
      cyan:    (s) => wrap('\x1b[38;5;73m', s),
      muted:   (s) => wrap('\x1b[38;5;245m', s),
      track:   (s) => wrap('\x1b[38;5;239m', s),
      bold:    (s) => wrap('\x1b[1m', s),
    };
  }
  return {
    crimson: (s) => wrap('\x1b[91m', s),
    ember:   (s) => wrap('\x1b[31m', s),
    trail:   (s) => wrap('\x1b[2m', s),
    gold:    (s) => wrap('\x1b[33m', s),
    emerald: (s) => wrap('\x1b[32m', s),
    cyan:    (s) => wrap('\x1b[36m', s),
    muted:   (s) => wrap('\x1b[2m', s),
    track:   (s) => wrap('\x1b[2m', s),
    bold:    (s) => wrap('\x1b[1m', s),
  };
}

function formatBytes(bytes) {
  if (!Number.isFinite(bytes) || bytes < 0) return '0 B';
  if (bytes >= 1_000_000) return `${(bytes / 1_000_000).toFixed(1)} MB`;
  if (bytes >= 1_000) return `${(bytes / 1_000).toFixed(0)} KB`;
  return `${bytes} B`;
}

function formatDuration(ms) {
  if (ms < 1000) return `${Math.max(1, Math.round(ms))}ms`;
  return `${(ms / 1000).toFixed(1)}s`;
}

const SUB_BLOCKS = [' ', '\u258F', '\u258E', '\u258D', '\u258C', '\u258B', '\u258A', '\u2589', '\u2588'];

function renderSubBar(ratio, width, tick, pal) {
  const clamped = Math.max(0, Math.min(1, ratio));
  const eighths = Math.round(clamped * width * 8);
  const full = Math.floor(eighths / 8);
  const rem = eighths % 8;
  const gleamPos = full > 2 ? tick % (full + 4) : -1;
  let out = '';
  for (let i = 0; i < width; i++) {
    if (i < full) {
      if (i === gleamPos || i === gleamPos - 1) {
        out += pal.gold('\u2588');
      } else if (i >= full - 2) {
        out += pal.ember('\u2588');
      } else {
        out += pal.crimson('\u2588');
      }
    } else if (i === full && rem > 0) {
      out += pal.ember(SUB_BLOCKS[rem]);
    } else {
      out += pal.track('\u2591');
    }
  }
  return out;
}

function renderScanner(tick, pal) {
  const span = 8;
  const cycle = (span - 1) * 2;
  const phase = tick % cycle;
  const pos = phase < span ? phase : cycle - phase;
  let cells = '';
  for (let i = 0; i < span; i++) {
    const dist = Math.abs(i - pos);
    if (dist === 0) cells += pal.crimson('\u25B0');
    else if (dist === 1) cells += pal.ember('\u25B1');
    else if (dist === 2) cells += pal.trail('\u2022');
    else cells += pal.track('\u00B7');
  }
  return `${pal.muted('\u27E6')}${cells}${pal.muted('\u27E7')}`;
}

function createReporter({ stream, log, warn, version, platKey, cacheNote }) {
  const isTty = Boolean(stream && stream.isTTY && process.env.TERM !== 'dumb');
  if (!isTty) {
    return {
      isTty: false,
      banner() {},
      stage() {},
      progress() {},
      stepOk() {},
      note(line) { log(line); },
      warn(line) { warn(line); },
      done() {},
      fail() {},
    };
  }

  const tier = detectColorTier(stream);
  const pal = makePalette(tier);
  const cols = () => Math.max(60, Math.min(120, stream.columns || 88));
  let timer = null;
  let tick = 0;
  let startedAt = Date.now();
  let stageStarted = Date.now();
  let activeLabel = '';
  let progressState = null;
  let cursorHidden = false;

  const restoreCursor = () => {
    if (cursorHidden) {
      cursorHidden = false;
      try { stream.write('\r\x1b[K\x1b[?25h'); } catch (_) {}
    }
  };
  const onExit = () => restoreCursor();
  process.once('exit', onExit);

  const paintActive = () => {
    if (!activeLabel) return;
    const width = cols();
    const scanner = renderScanner(tick, pal);
    let line;
    if (progressState && progressState.total > 0) {
      const ratio = progressState.received / progressState.total;
      const pct = `${Math.min(100, Math.floor(ratio * 100))}%`.padStart(4);
      const barWidth = width >= 92 ? 20 : 12;
      const bar = renderSubBar(ratio, barWidth, tick, pal);
      const bytes = `${formatBytes(progressState.received)}/${formatBytes(progressState.total)}`;
      const speed = progressState.rateBps > 0 ? ` \u00B7 ${formatBytes(progressState.rateBps)}/s` : '';
      const remSec = progressState.rateBps > 1024
        ? Math.max(0, (progressState.total - progressState.received) / progressState.rateBps)
        : null;
      const eta = remSec !== null && remSec < 3600 ? ` \u00B7 eta ${remSec.toFixed(1)}s` : '';
      const lanes = progressState.lanes > 1 ? ` \u00B7 ${pal.cyan(`${progressState.lanes}\u00D7`)}` : '';
      line = `  ${scanner} ${bar} ${pal.bold(pct)}  ${pal.muted(bytes + speed + eta)}${lanes}`;
    } else if (progressState && progressState.received > 0) {
      const speed = progressState.rateBps > 0 ? ` \u00B7 ${formatBytes(progressState.rateBps)}/s` : '';
      line = `  ${scanner}  ${activeLabel}  ${pal.muted(formatBytes(progressState.received) + speed)}`;
    } else {
      const elapsed = ((Date.now() - stageStarted) / 1000).toFixed(1);
      line = `  ${scanner}  ${activeLabel}  ${pal.muted(`${elapsed}s`)}`;
    }
    try { stream.write(`\r\x1b[K${line}`); } catch (_) {}
  };

  const ensureTimer = () => {
    if (!cursorHidden) {
      cursorHidden = true;
      try { stream.write('\x1b[?25l'); } catch (_) {}
    }
    if (!timer) {
      timer = setInterval(() => {
        tick += 1;
        paintActive();
      }, 65);
      if (typeof timer.unref === 'function') timer.unref();
    }
  };

  const clearActive = () => {
    if (timer) {
      clearInterval(timer);
      timer = null;
    }
    activeLabel = '';
    progressState = null;
    try { stream.write('\r\x1b[K'); } catch (_) {}
  };

  return {
    isTty: true,
    banner() {
      startedAt = Date.now();
      const sub = cacheNote ? `${platKey} \u00B7 user cache` : platKey;
      const rule = pal.muted('\u2500'.repeat(Math.min(56, cols() - 4)));
      try {
        stream.write(
          `\n  ${pal.crimson('\u25C6')} ${pal.bold(`DonSeTch v${version}`)} ${pal.muted(`\u00B7 Bootstrap (${sub})`)}\n` +
          `  ${rule}\n`
        );
      } catch (_) {}
    },
    stage(label) {
      stageStarted = Date.now();
      activeLabel = label;
      progressState = null;
      ensureTimer();
      paintActive();
    },
    progress(state) {
      progressState = state;
      ensureTimer();
      paintActive();
    },
    stepOk(name, detail) {
      const elapsed = formatDuration(Date.now() - stageStarted);
      clearActive();
      const pad = String(name).padEnd(12);
      try {
        stream.write(
          `  ${pal.emerald('\u2713')}  ${pal.bold(pad)} ${pal.muted(detail)}  ${pal.muted(elapsed)}\n`
        );
      } catch (_) {}
    },
    note(line) {
      clearActive();
      try { stream.write(`  ${pal.muted('\u25B8')}  ${pal.muted(line)}\n`); } catch (_) {}
    },
    warn(line) {
      clearActive();
      try { stream.write(`  ${pal.gold('\u26A0')}  ${pal.gold(line)}\n`); } catch (_) {}
      paintActive();
    },
    done(binaryPath) {
      clearActive();
      restoreCursor();
      process.removeListener('exit', onExit);
      const total = formatDuration(Date.now() - startedAt);
      try {
        stream.write(
          `  ${pal.emerald('\u2713')}  ${pal.bold('Installed   ')} ${pal.cyan(binaryPath)} ${pal.muted(`(${total})`)}\n\n`
        );
      } catch (_) {}
    },
    fail() {
      clearActive();
      restoreCursor();
      process.removeListener('exit', onExit);
    },
  };
}

// ── Proxy & HTTPS transport ──────────────────────────────────────

function noProxy(host) {
  const target = host.replace(/^\[|\]$/g, '').toLowerCase();
  const value = process.env.NO_PROXY || process.env.no_proxy || '';
  return value.split(',').map((part) => part.trim().toLowerCase()).filter(Boolean).some((entry) => {
    if (entry === '*') return true;
    const withoutPort = entry.startsWith('[')
      ? entry.replace(/^\[([^\]]+)\](?::\d+)?$/, '$1')
      : entry.replace(/:\d+$/, '');
    const normalized = withoutPort.replace(/^\*\./, '').replace(/^\./, '');
    return target === normalized || target.endsWith(`.${normalized}`);
  });
}

function proxyFor(target) {
  if (noProxy(target.hostname)) return null;
  const value = target.protocol === 'https:'
    ? (process.env.HTTPS_PROXY || process.env.https_proxy || process.env.HTTP_PROXY || process.env.http_proxy || process.env.ALL_PROXY || process.env.all_proxy)
    : (process.env.HTTP_PROXY || process.env.http_proxy || process.env.ALL_PROXY || process.env.all_proxy);
  if (!value) return null;
  let parsed;
  try {
    parsed = new URL(value);
  } catch (_) {
    const error = new InstallError(`invalid proxy URL "${value}"`, 'INVALID_PROXY');
    error.retryable = false;
    throw error;
  }
  if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') {
    const error = new InstallError(
      `unsupported proxy protocol "${parsed.protocol}" (expected http:// or https://)`,
      'UNSUPPORTED_PROXY'
    );
    error.retryable = false;
    throw error;
  }
  return parsed;
}

function isPermanentTlsError(err) {
  const code = String((err && err.code) || '');
  const msg = String((err && err.message) || '');
  return (
    code.includes('SELF_SIGNED') ||
    code.includes('UNABLE_TO_VERIFY') ||
    code.startsWith('CERT_') ||
    code.startsWith('ERR_TLS_CERT_') ||
    /self[- ]signed certificate|certificate has expired|unable to verify/i.test(msg)
  );
}

function tagTransportError(err) {
  if (isPermanentTlsError(err)) {
    err.retryable = false;
  }
  return err;
}

function requestDirect(target, extraHeaders = {}) {
  return new Promise((resolve, reject) => {
    const transport = target.protocol === 'https:' ? https : http;
    const req = transport.get(target, {
      headers: {
        Accept: 'application/octet-stream',
        'User-Agent': 'donsetch-npm-installer',
        ...extraHeaders,
      },
    }, resolve);
    req.setTimeout(REQUEST_TIMEOUT_MS, () => req.destroy(new Error('request timeout')));
    req.on('error', (err) => reject(tagTransportError(err)));
  });
}

function proxyAuthorization(proxy) {
  if (!proxy.username && !proxy.password) return null;
  const decode = (part) => {
    try { return decodeURIComponent(part); } catch (_) { return part; }
  };
  const pair = `${decode(proxy.username)}:${decode(proxy.password)}`;
  return `Basic ${Buffer.from(pair).toString('base64')}`;
}

function requestThroughProxy(target, proxy, extraHeaders = {}) {
  return new Promise((resolve, reject) => {
    const authority = `${target.hostname}:${target.port || 443}`;
    const connectHeaders = { Host: authority };
    const authorization = proxyAuthorization(proxy);
    if (authorization) connectHeaders['Proxy-Authorization'] = authorization;
    const connectTransport = proxy.protocol === 'https:' ? https : http;
    const proxyReq = connectTransport.request({
      hostname: proxy.hostname,
      port: proxy.port || (proxy.protocol === 'https:' ? 443 : 80),
      method: 'CONNECT',
      path: authority,
      headers: connectHeaders,
    });
    proxyReq.once('connect', (response, socket) => {
      if (response.statusCode !== 200) {
        socket.destroy();
        const err = new Error(`proxy CONNECT returned HTTP ${response.statusCode}`);
        err.retryable = response.statusCode === 429 || response.statusCode >= 500;
        reject(err);
        return;
      }
      const req = https.request({
        hostname: target.hostname,
        port: target.port || 443,
        path: `${target.pathname}${target.search}`,
        method: 'GET',
        headers: {
          Accept: 'application/octet-stream',
          'User-Agent': 'donsetch-npm-installer',
          ...extraHeaders,
        },
        // No `agent` key: Node uses createConnection only when no agent
        // is in use, and closes the tunnel socket once the response ends.
        createConnection: () => tls.connect({
          socket,
          host: target.hostname,
          servername: net.isIP(target.hostname) ? undefined : target.hostname,
        }),
      }, resolve);
      req.setTimeout(REQUEST_TIMEOUT_MS, () => req.destroy(new Error('request timeout')));
      req.on('error', (err) => reject(tagTransportError(err)));
      req.end();
    });
    proxyReq.setTimeout(REQUEST_TIMEOUT_MS, () => {
      proxyReq.destroy(new Error('proxy CONNECT timeout'));
    });
    proxyReq.on('error', (err) => reject(tagTransportError(err)));
    proxyReq.end();
  });
}

async function requestUrl(url, extraHeaders = {}) {
  const target = new URL(url);
  if (target.protocol !== 'https:') {
    const err = new Error(`refusing non-HTTPS download URL ${url}`);
    err.retryable = false;
    throw err;
  }
  const proxy = proxyFor(target);
  return proxy
    ? requestThroughProxy(target, proxy, extraHeaders)
    : requestDirect(target, extraHeaders);
}

async function getWithRedirects(url, extraHeaders = {}) {
  let current = url;
  for (let hops = 0; hops <= MAX_REDIRECTS; hops++) {
    const response = await requestUrl(current, extraHeaders);
    if (![301, 302, 303, 307, 308].includes(response.statusCode)) {
      response.finalUrl = current;
      return response;
    }
    response.resume();
    const location = response.headers.location;
    if (!location) throw new Error(`redirect without Location from ${current}`);
    const next = new URL(location, current).toString();
    if (!next.startsWith('https://')) {
      const err = new Error(`refusing HTTP downgrade redirect to ${next}`);
      err.retryable = false;
      throw err;
    }
    current = next;
  }
  throw new Error(`too many redirects for ${url}`);
}

function drain(response) {
  return new Promise((resolve) => {
    response.resume();
    response.once('end', resolve);
    response.once('close', resolve);
  });
}

function statusError(url, status) {
  const error = new Error(`HTTP ${status} for ${url}`);
  error.retryable = status === 429 || status >= 500;
  return error;
}

function totalFromContentRange(header) {
  if (!header) return 0;
  const slash = String(header).lastIndexOf('/');
  if (slash < 0) return 0;
  const n = Number(String(header).slice(slash + 1).trim());
  return Number.isFinite(n) && n > 0 ? n : 0;
}

async function fetchRangeSegment(url, from, toExclusive, onChunk) {
  const want = toExclusive - from;
  const chunks = [];
  let got = 0;
  for (let tries = 0; tries < 4 && got < want; tries++) {
    const start = from + got;
    const res = await getWithRedirects(url, { Range: `bytes=${start}-${toExclusive - 1}` });
    if (res.statusCode !== 206) {
      await drain(res);
      throw new Error(`range ${start}-${toExclusive - 1}: HTTP ${res.statusCode}`);
    }
    await new Promise((resolve, reject) => {
      res.on('data', (chunk) => {
        const take = Math.min(chunk.length, want - got);
        if (take > 0) {
          chunks.push(take === chunk.length ? chunk : chunk.subarray(0, take));
          got += take;
          onChunk(take);
        }
        if (got >= want) {
          res.destroy();
          resolve();
        }
      });
      res.once('end', resolve);
      res.once('error', (err) => (got >= want ? resolve() : reject(err)));
    });
  }
  if (got < want) {
    throw new Error(`range ${from}-${toExclusive - 1} incomplete (${got}/${want})`);
  }
  return Buffer.concat(chunks, want);
}

async function downloadParallelTail(url, dest, startOffset, total, onProgress) {
  const remaining = total - startOffset;
  const segSize = Math.max(256 * 1024, Math.ceil(remaining / PARALLEL_LANES));
  const segments = [];
  for (let at = startOffset; at < total; at += segSize) {
    segments.push([at, Math.min(total, at + segSize)]);
  }
  let delivered = startOffset;
  const started = Date.now();
  let smoothBps = 0;
  let lastBytes = delivered;
  let lastTick = started;

  const parts = await Promise.all(
    segments.map(([from, to]) =>
      fetchRangeSegment(url, from, to, (delta) => {
        delivered += delta;
        const now = Date.now();
        const dt = (now - lastTick) / 1000;
        if (dt >= 0.08 && onProgress) {
          const inst = (delivered - lastBytes) / dt;
          smoothBps = smoothBps > 0 ? smoothBps * 0.7 + inst * 0.3 : inst;
          lastBytes = delivered;
          lastTick = now;
          onProgress({ received: delivered, total, rateBps: smoothBps, lanes: segments.length });
        }
      })
    )
  );
  const fd = fs.openSync(dest, 'a');
  try {
    for (const buf of parts) fs.writeSync(fd, buf);
  } finally {
    fs.closeSync(fd);
  }
  if (onProgress) {
    const elapsed = Math.max(0.001, (Date.now() - started) / 1000);
    onProgress({ received: total, total, rateBps: (total - startOffset) / elapsed, lanes: segments.length });
  }
}

async function streamToFile(response, dest, appendFrom, total, allowParallelBoost, onProgress) {
  return new Promise((resolve, reject) => {
    const flags = appendFrom > 0 ? 'a' : 'w';
    const fd = fs.openSync(dest, flags);
    let closed = false;
    const closeFd = () => {
      if (!closed) {
        closed = true;
        try { fs.closeSync(fd); } catch (_) {}
      }
    };
    let received = appendFrom;
    const started = Date.now();
    let windowStart = started + PARALLEL_WARMUP_MS;
    let windowBase = appendFrom;
    let lastBytes = appendFrom;
    let lastTick = started;
    let smoothBps = 0;
    let settled = false;

    const finish = (fn, val) => {
      if (settled) return;
      settled = true;
      closeFd();
      fn(val);
    };

    response.on('data', (chunk) => {
      if (settled) return;
      try {
        fs.writeSync(fd, chunk);
      } catch (err) {
        response.destroy();
        finish(reject, err);
        return;
      }
      received += chunk.length;
      const now = Date.now();
      const dt = (now - lastTick) / 1000;
      if (dt >= 0.06 && onProgress) {
        const inst = (received - lastBytes) / dt;
        smoothBps = smoothBps > 0 ? smoothBps * 0.72 + inst * 0.28 : inst;
        lastBytes = received;
        lastTick = now;
        onProgress({ received, total, rateBps: smoothBps, lanes: 1 });
      }
      if (allowParallelBoost && now >= windowStart + 3_000 && total - received > 512 * 1024) {
        const winSecs = (now - windowStart) / 1000;
        const rate = (received - windowBase) / Math.max(0.001, winSecs);
        if (rate < PARALLEL_MIN_BPS) {
          settled = true;
          closeFd();
          response.destroy();
          resolve({ boostedFrom: received });
          return;
        }
        windowStart = now;
        windowBase = received;
      }
    });

    response.once('error', (err) => {
      finish(reject, err);
    });
    response.once('end', () => {
      if (settled) return;
      if (total > 0 && received !== total) {
        finish(reject, new Error(`truncated download (${received} of ${total} bytes)`));
        return;
      }
      if (onProgress) {
        const elapsed = Math.max(0.001, (Date.now() - started) / 1000);
        onProgress({ received, total: total || received, rateBps: (received - appendFrom) / elapsed, lanes: 1 });
      }
      finish(resolve, { boostedFrom: null });
    });
    response.once('close', () => {
      if (settled) return;
      if (total > 0 && received !== total) {
        finish(reject, new Error(`connection closed early (${received} of ${total} bytes)`));
      } else {
        finish(resolve, { boostedFrom: null });
      }
    });
  });
}

async function downloadFile(url, dest, warn, onProgress) {
  try { fs.unlinkSync(dest); } catch (_) {}
  for (let attempt = 0; attempt < RETRIES; attempt++) {
    let have = 0;
    try {
      if (fs.existsSync(dest)) have = fs.statSync(dest).size;
    } catch (_) {
      have = 0;
    }
    const headers = have > 0 ? { Range: `bytes=${have}-` } : {};
    try {
      const response = await getWithRedirects(url, headers);
      const code = response.statusCode;
      if (code !== 200 && code !== 206) {
        const error = statusError(url, code);
        await drain(response);
        throw error;
      }
      const appendFrom = code === 206 ? have : 0;
      const contentLen = Number(response.headers['content-length'] || 0);
      const total = code === 206
        ? (totalFromContentRange(response.headers['content-range']) || (contentLen > 0 ? appendFrom + contentLen : 0))
        : (contentLen > 0 ? contentLen : 0);
      const acceptsRanges = String(response.headers['accept-ranges'] || '').toLowerCase().includes('bytes');
      const allowBoost = acceptsRanges && total >= PARALLEL_FLOOR_BYTES;

      const outcome = await streamToFile(
        response,
        dest,
        appendFrom,
        total,
        allowBoost,
        onProgress
      );
      if (outcome && outcome.boostedFrom !== null) {
        const targetUrl = response.finalUrl || url;
        await downloadParallelTail(targetUrl, dest, outcome.boostedFrom, total, onProgress);
      }
      return;
    } catch (error) {
      if (attempt + 1 >= RETRIES || error.retryable === false) {
        try { fs.unlinkSync(dest); } catch (_) {}
        throw error;
      }
      const delay = RETRY_BACKOFF_MS[attempt] || RETRY_BACKOFF_MS[RETRY_BACKOFF_MS.length - 1];
      let resumed = 0;
      try { if (fs.existsSync(dest)) resumed = fs.statSync(dest).size; } catch (_) {}
      const resumeNote = resumed > 0 ? `, keeping ${formatBytes(resumed)}` : '';
      warn(`donsetch: download attempt ${attempt + 1} failed (${error.message}${resumeNote}); retrying in ${delay / 1000}s`);
      await sleep(delay);
    }
  }
}

async function fetchText(url, warn, extraHeaders = {}) {
  for (let attempt = 0; attempt < RETRIES; attempt++) {
    try {
      const response = await getWithRedirects(url, extraHeaders);
      if (response.statusCode !== 200) {
        const error = statusError(url, response.statusCode);
        await drain(response);
        throw error;
      }
      const chunks = [];
      let bytes = 0;
      await new Promise((resolve, reject) => {
        response.on('data', (chunk) => {
          bytes += chunk.length;
          if (bytes > 2 * 1024 * 1024) {
            response.destroy(new Error('metadata response exceeded 2 MiB'));
            return;
          }
          chunks.push(chunk);
        });
        response.once('end', resolve);
        response.once('error', reject);
      });
      return Buffer.concat(chunks).toString('utf8');
    } catch (error) {
      if (attempt + 1 >= RETRIES || error.retryable === false) throw error;
      const delay = RETRY_BACKOFF_MS[attempt] || RETRY_BACKOFF_MS[RETRY_BACKOFF_MS.length - 1];
      warn(`donsetch: download attempt ${attempt + 1} failed (${error.message}); retrying in ${delay / 1000}s`);
      await sleep(delay);
    }
  }
  throw new Error(`failed to fetch ${url}`);
}

// Resolve alternate asset download URLs through the GitHub Releases API
// when default `github.com/.../releases/download` is blocked or failing.
async function lookupGithubReleaseAssets(tag) {
  const apiUrl = `https://api.github.com/repos/${REPO}/releases/tags/${encodeURIComponent(tag)}`;
  const raw = await fetchText(apiUrl, () => {}, { Accept: 'application/vnd.github+json' });
  const parsed = JSON.parse(raw);
  const map = new Map();
  for (const item of parsed.assets || []) {
    if (item && item.name && item.url) {
      map.set(item.name, item.url);
    }
  }
  return map;
}

// ── Pure-Node .tar.gz extraction (ustar, PAX extended, GNU LongLink) ──

function parseTarNumber(buf, offset, length) {
  if (buf[offset] & 0x80) {
    let val = 0n;
    for (let i = 1; i < length; i++) {
      val = (val << 8n) | BigInt(buf[offset + i]);
    }
    return Number(val);
  }
  const raw = buf.toString('ascii', offset, offset + length).replace(/\0.*$/, '').trim();
  return raw ? parseInt(raw, 8) : 0;
}

function parsePaxRecords(buf) {
  const out = {};
  let pos = 0;
  const str = buf.toString('utf8');
  while (pos < str.length) {
    const sp = str.indexOf(' ', pos);
    if (sp < 0) break;
    const len = parseInt(str.slice(pos, sp), 10);
    if (!Number.isFinite(len) || len <= 0) break;
    // PAX record length is in UTF-8 bytes; slice from Buffer for accuracy.
    const restBytes = Buffer.from(str.slice(pos), 'utf8');
    const record = restBytes.subarray(0, len).toString('utf8');
    const eq = record.indexOf('=', record.indexOf(' ') + 1);
    if (eq > 0) {
      const key = record.slice(record.indexOf(' ') + 1, eq);
      const val = record.slice(eq + 1).replace(/\n$/, '');
      out[key] = val;
    }
    pos += record.length;
  }
  return out;
}

function resolveSafeArchivePath(destRoot, rawEntryName) {
  const normalized = String(rawEntryName || '')
    .replace(/\\/g, '/')
    .replace(/^(?:\.\/)+/, '')
    .replace(/\/+$/, '');
  if (!normalized || normalized === '.') return null;
  if (normalized.startsWith('/') || /^[A-Za-z]:/.test(normalized)) {
    throw new Error(`unsafe archive entry path "${rawEntryName}"`);
  }
  const segments = normalized.split('/');
  if (segments.some((seg) => seg === '..')) {
    throw new Error(`unsafe path traversal in archive entry "${rawEntryName}"`);
  }
  const resolvedRoot = path.resolve(destRoot);
  const target = path.resolve(resolvedRoot, ...segments);
  const rootPrefix = resolvedRoot.endsWith(path.sep) ? resolvedRoot : resolvedRoot + path.sep;
  if (target !== resolvedRoot && !target.startsWith(rootPrefix)) {
    throw new Error(`archive entry escapes destination "${rawEntryName}"`);
  }
  return target;
}

function extractTarGz(tarballPath, extractDir) {
  const compressed = fs.readFileSync(tarballPath);
  const tar = zlib.gunzipSync(compressed);
  fs.mkdirSync(extractDir, { recursive: true });

  let offset = 0;
  let nextPax = null;
  let nextLongName = null;

  while (offset + 512 <= tar.length) {
    const header = tar.subarray(offset, offset + 512);
    let allZero = true;
    for (let i = 0; i < 512; i++) {
      if (header[i] !== 0) {
        allZero = false;
        break;
      }
    }
    if (allZero) {
      offset += 512;
      continue;
    }

    const typeByte = header[156];
    const typeFlag = typeByte === 0 ? '0' : String.fromCharCode(typeByte);
    const mode = parseTarNumber(header, 100, 8);
    let size = parseTarNumber(header, 124, 12);

    const nameRaw = header.toString('utf8', 0, 100).replace(/\0.*$/, '');
    const magic = header.toString('ascii', 257, 262);
    const prefix = magic === 'ustar'
      ? header.toString('utf8', 345, 500).replace(/\0.*$/, '')
      : '';
    let entryName = prefix ? `${prefix}/${nameRaw}` : nameRaw;

    const dataStart = offset + 512;
    if (nextPax && nextPax.size !== undefined) {
      const paxSize = Number(nextPax.size);
      if (Number.isFinite(paxSize) && paxSize >= 0) size = paxSize;
    }
    const dataEnd = dataStart + size;
    if (dataEnd > tar.length) {
      throw new Error(`corrupt tar archive: truncated entry "${entryName}"`);
    }
    const payload = tar.subarray(dataStart, dataEnd);
    const blocks = Math.ceil(size / 512);
    offset = dataStart + blocks * 512;

    if (typeFlag === 'x') {
      nextPax = parsePaxRecords(payload);
      continue;
    }
    if (typeFlag === 'g') {
      continue;
    }
    if (typeFlag === 'L') {
      nextLongName = payload.toString('utf8').replace(/\0.*$/, '');
      continue;
    }
    if (typeFlag === 'K') {
      continue;
    }

    if (nextPax && nextPax.path) entryName = nextPax.path;
    if (nextLongName) entryName = nextLongName;
    nextPax = null;
    nextLongName = null;

    const target = resolveSafeArchivePath(extractDir, entryName);
    if (!target) continue;

    if (typeFlag === '5') {
      fs.mkdirSync(target, { recursive: true });
    } else if (typeFlag === '0') {
      fs.mkdirSync(path.dirname(target), { recursive: true });
      fs.writeFileSync(target, payload);
      if (process.platform !== 'win32' && (mode & 0o111) !== 0) {
        try { fs.chmodSync(target, 0o755); } catch (_) {}
      }
    }
  }
}

// ── Host environment & atomic file landing ───────────────────────

function detectMusl() {
  let interpreter;
  try {
    const fd = fs.openSync('/proc/self/exe', 'r');
    try {
      const hdr = Buffer.alloc(64);
      fs.readSync(fd, hdr, 0, 64, 0);
      const ePhOff = hdr.readBigUInt64LE(0x20);
      const ePhEntSize = hdr.readUInt16LE(0x36);
      const ePhNum = hdr.readUInt16LE(0x38);
      for (let i = 0; i < ePhNum; i++) {
        const ph = Buffer.alloc(ePhEntSize);
        fs.readSync(fd, ph, 0, ePhEntSize, Number(ePhOff) + i * ePhEntSize);
        if (ph.readUInt32LE(0) === 3) {
          const offset = Number(ph.readBigUInt64LE(0x08));
          const size = Number(ph.readBigUInt64LE(0x20));
          const value = Buffer.alloc(size);
          fs.readSync(fd, value, 0, size, offset);
          interpreter = value.toString('ascii').replace(/\0/g, '').trim();
          break;
        }
      }
    } finally {
      try { fs.closeSync(fd); } catch (_) {}
    }
  } catch (_) {
    interpreter = undefined;
  }
  if (interpreter) return interpreter.includes('musl');

  const muslLoader = fs.existsSync('/lib/ld-musl-x86_64.so.1')
    || fs.existsSync('/lib/ld-musl-aarch64.so.1');
  const glibcLoader = fs.existsSync('/lib64/ld-linux-x86-64.so.2')
    || fs.existsSync('/lib/ld-linux-aarch64.so.1');
  return muslLoader && !glibcLoader;
}

// Temp leftovers from crashed runs (`.tmp-<pid>` workspaces and Windows
// `.old-<pid>-*` locked-binary move-asides). A pid that is still alive
// keeps its files untouched.
function cleanupStaleTemps(binDir) {
  let names;
  try {
    names = fs.readdirSync(binDir);
  } catch (_) {
    return;
  }
  for (const name of names) {
    const m = /\.(?:tmp|old)-(\d+)(?:-\d+)?$/.exec(name);
    if (!m || Number(m[1]) === process.pid) continue;
    let alive = false;
    try {
      process.kill(Number(m[1]), 0);
      alive = true;
    } catch (_) {/* dead process: safe to reap */}
    if (!alive) {
      try { fs.rmSync(path.join(binDir, name), { recursive: true, force: true }); } catch (_) {}
    }
  }
}

function landEntryAtomically(src, dst, platform) {
  try {
    if (fs.lstatSync(dst).isDirectory()) {
      fs.rmSync(dst, { recursive: true, force: true });
    }
  } catch (_) {}

  try {
    fs.renameSync(src, dst);
  } catch (err) {
    // On Windows a running `donsetch.exe` or loaded `.dll` (e.g. an active
    // MCP server in an IDE) cannot be overwritten in place, but CAN be
    // renamed aside first so the new binary lands cleanly.
    if (
      platform === 'win32' &&
      fs.existsSync(dst) &&
      (err.code === 'EPERM' || err.code === 'EBUSY' || err.code === 'EACCES')
    ) {
      const aside = `${dst}.old-${process.pid}-${Date.now()}`;
      fs.renameSync(dst, aside);
      try {
        fs.renameSync(src, dst);
      } catch (inner) {
        try { fs.renameSync(aside, dst); } catch (_) {}
        throw inner;
      }
      try { fs.unlinkSync(aside); } catch (_) {/* freed on next run once process exits */}
      return;
    }
    throw err;
  }
}

function unwritable(cause, binDir) {
  const detail = cause && cause.message ? cause.message : 'directory is not writable';
  const error = new InstallError(`cannot install into ${binDir} (${detail})`, 'UNWRITABLE');
  error.binDir = binDir;
  return error;
}

// Download, verify and land the pinned binary in `binDir`.
//
// Returns { status, binaryPath } where status is 'skipped', 'present',
// or 'installed'. Throws InstallError on failure.
async function install(opts = {}) {
  const log = typeof opts.log === 'function' ? opts.log : () => {};
  const warn = typeof opts.warn === 'function' ? opts.warn : log;
  const platform = opts.platform || process.platform;
  const arch = opts.arch || process.arch;
  const version = opts.version || VERSION;
  const binDir = opts.binDir ? path.resolve(opts.binDir) : path.join(__dirname, 'binaries');
  const tag = process.env.DONSETCH_INSTALL_TAG || `v${version}`;
  const customBase = opts.releasesBase || process.env.DONSETCH_RELEASES_BASE || null;
  const releasesBase = (customBase || `https://github.com/${REPO}/releases/download`).replace(/\/+$/, '');

  const platKey = `${platform}-${arch}`;
  const plat = platformFor(platform, arch);
  if (!plat) {
    throw new InstallError(`unsupported platform ${platKey}`, 'UNSUPPORTED_PLATFORM');
  }

  if (process.env.DONSETCH_SKIP_DOWNLOAD === '1') {
    return { status: 'skipped' };
  }

  if (platform === 'linux'
      && process.env.DONSETCH_FORCE_GLIBC !== '1'
      && detectMusl()) {
    throw new InstallError(
      [
        'donsetch: musl libc detected (Alpine?).',
        'The prebuilt Linux binaries are glibc-linked and will not run.',
        '',
        'Build from source:',
        `  git clone https://github.com/${REPO} && cd donsetch && cargo build --release`,
        'Or use a glibc-based image (Debian, Ubuntu, Fedora).',
        'Set DONSETCH_FORCE_GLIBC=1 only if a glibc compatibility layer is installed.',
      ].join('\n'),
      'UNSUPPORTED_LIBC'
    );
  }

  const binaryPath = path.join(binDir, plat.binary);
  const stampPath = `${binaryPath}.version`;
  const assetUrl = `${releasesBase}/${tag}/${plat.asset}`;
  const checksumUrl = `${assetUrl}.sha256`;

  if (fs.existsSync(binaryPath) && fs.existsSync(stampPath)) {
    let stamp;
    try { stamp = fs.readFileSync(stampPath, 'utf8').trim(); } catch (_) {}
    if (stamp === version) {
      log(`donsetch: binary already present (${plat.binary} ${version})`);
      return { status: 'present', binaryPath };
    }
    log(`donsetch: binary version stamp mismatch; re-downloading ${version}`);
  }

  // Decide directory writability BEFORE touching the network (#361).
  try {
    fs.mkdirSync(binDir, { recursive: true });
  } catch (err) {
    throw unwritable(err, binDir);
  }
  try { fs.accessSync(binDir, fs.constants.W_OK); }
  catch (err) { throw unwritable(err, binDir); }
  cleanupStaleTemps(binDir);

  const ui = createReporter({
    stream: opts.stream || null,
    log,
    warn,
    version,
    platKey,
    cacheNote: Boolean(opts.cacheInstall),
  });

  if (plat.emulated) {
    if (ui.isTty) ui.note('Windows arm64 uses the win32-x64 build under emulation.');
    else log('donsetch: Windows arm64 uses the win32-x64 build under emulation.');
  }

  const tarball = path.join(binDir, `${plat.asset}.tmp-${process.pid}`);
  const stampTmp = path.join(binDir, `${plat.binary}.version.tmp-${process.pid}`);
  const extractDir = path.join(binDir, `extract.tmp-${process.pid}`);

  ui.banner();
  if (!ui.isTty) {
    log(`donsetch: downloading ${plat.asset} from ${tag}...`);
  }

  try {
    let apiAssets = null;
    const getApiAssets = async () => {
      if (customBase) return null;
      if (!apiAssets) {
        try {
          apiAssets = await lookupGithubReleaseAssets(tag);
        } catch (_) {
          apiAssets = new Map();
        }
      }
      return apiAssets;
    };

    // 1. Fetch checksum first (64 bytes): refuses missing releases, bad
    //    proxies, or TLS failures before pulling a 20-40 MB archive.
    ui.stage('Checking SHA256 manifest\u2026');
    let checksumRaw;
    try {
      checksumRaw = await fetchText(checksumUrl, (m) => ui.warn(m));
    } catch (err) {
      const assets = err.retryable === false ? null : await getApiAssets();
      const alt = assets && assets.get(`${plat.asset}.sha256`);
      if (!alt) throw err;
      checksumRaw = await fetchText(alt, (m) => ui.warn(m));
    }
    const expectedHash = checksumRaw.trim().split(/\s+/)[0] || '';
    if (!/^[a-f0-9]{64}$/.test(expectedHash)) {
      throw new Error(`invalid SHA256 manifest for ${plat.asset}`);
    }
    ui.stepOk('Checksum', `sha256 \u00B7 ${expectedHash.slice(0, 12)}\u2026`);

    // 2. Download platform archive (resumable + parallel boost on slow CDN).
    const dlStarted = Date.now();
    ui.stage(`Downloading ${plat.asset}\u2026`);
    try {
      await downloadFile(
        assetUrl,
        tarball,
        (m) => ui.warn(m),
        (p) => ui.progress(p)
      );
    } catch (err) {
      const assets = err.retryable === false ? null : await getApiAssets();
      const alt = assets && assets.get(plat.asset);
      if (!alt) throw err;
      ui.warn('donsetch: primary release CDN failed; switching to GitHub API mirror');
      await downloadFile(
        alt,
        tarball,
        (m) => ui.warn(m),
        (p) => ui.progress(p)
      );
    }
    const tarBytes = fs.statSync(tarball).size;
    const dlSecs = Math.max(0.001, (Date.now() - dlStarted) / 1000);
    ui.stepOk(
      'Downloaded',
      `${plat.asset} \u00B7 ${formatBytes(tarBytes)} (${formatBytes(tarBytes / dlSecs)}/s)`
    );

    // 3. Verify SHA-256 digest before unpacking.
    if (!ui.isTty) log('donsetch: verifying checksum...');
    ui.stage('Verifying SHA256 digest\u2026');
    const actualHash = crypto.createHash('sha256').update(fs.readFileSync(tarball)).digest('hex');
    if (actualHash !== expectedHash) {
      throw new Error(`SHA256 mismatch (expected ${expectedHash}, actual ${actualHash})`);
    }
    ui.stepOk('Verified', `${actualHash.slice(0, 8)}\u2026${actualHash.slice(-8)}`);

    // 4. Extract in-process via built-in zlib + POSIX tar parser.
    if (!ui.isTty) log('donsetch: extracting...');
    ui.stage('Extracting runtime payload\u2026');
    fs.rmSync(extractDir, { recursive: true, force: true });
    extractTarGz(tarball, extractDir);

    const entries = fs.readdirSync(extractDir);
    if (!entries.includes(plat.binary)) {
      throw new Error(`expected ${plat.binary} not found after extraction in ${binDir}`);
    }

    const stagedBinary = path.join(extractDir, plat.binary);
    if (platform !== 'win32') {
      fs.chmodSync(stagedBinary, 0o755);
    }

    // Land sibling libraries FIRST and the main executable LAST so a
    // concurrent resolver never observes `donsetch(.exe)` before its
    // companion `.so` / `.dylib` / `.dll` runtimes are in place.
    const ordered = entries
      .filter((name) => name !== plat.binary)
      .concat([plat.binary]);
    for (const name of ordered) {
      landEntryAtomically(
        path.join(extractDir, name),
        path.join(binDir, name),
        platform
      );
    }
    fs.rmSync(extractDir, { recursive: true, force: true });

    if (platform === 'darwin') {
      try {
        execFileSync('xattr', ['-dr', 'com.apple.quarantine', binDir], { stdio: 'ignore' });
      } catch (_) {}
    }

    fs.writeFileSync(stampTmp, `${version}\n`);
    fs.renameSync(stampTmp, stampPath);

    ui.stepOk('Extracted', `${ordered.length} file${ordered.length === 1 ? '' : 's'} staged`);
    if (ui.isTty) {
      ui.done(binaryPath);
    } else {
      log(`donsetch: installed ${plat.binary} to ${binaryPath}`);
    }
    return { status: 'installed', binaryPath };
  } catch (err) {
    ui.fail();
    throw err;
  } finally {
    try { fs.unlinkSync(tarball); } catch (_) {}
    try { fs.unlinkSync(stampTmp); } catch (_) {}
    try { fs.rmSync(extractDir, { recursive: true, force: true }); } catch (_) {}
  }
}

function optionsText() {
  return [
    'Options:',
    '  - install under a user-writable prefix:',
    '      npm config set prefix ~/.npm-global && npm install -g donsetch',
    '  - point at a donsetch binary you already have:',
    '      DONSETCH_BINARY=/path/to/donsetch',
    '  - build from source:',
    `      git clone https://github.com/${REPO} && cd donsetch && cargo build --release`,
  ];
}

// One existing-binary lookup for the failure paths: an install that
// cannot succeed must not take a working donsetch down with it (#361).
function ackExisting(detail, log = console.log) {
  let existing = null;
  try {
    existing = findExistingBinary({ pkgDir: __dirname });
  } catch (_) {
    return false;
  }
  if (!existing) return false;
  log(`donsetch: install failed: ${detail}`);
  log(`donsetch: an existing binary was found and will be used: ${existing.path}`);
  return true;
}

// ── CLI mode ─────────────────────────────────────────────────────
async function main(argv = process.argv.slice(2)) {
  const binDirIndex = argv.indexOf('--bin-dir');
  if (binDirIndex >= 0 && !argv[binDirIndex + 1]) {
    console.error('donsetch: --bin-dir requires a directory');
    return 1;
  }
  const binDir = binDirIndex >= 0
    ? path.resolve(argv[binDirIndex + 1])
    : path.join(__dirname, 'binaries');

  try {
    const result = await install({
      binDir,
      stream: process.stdout.isTTY ? process.stdout : null,
      log: (line) => console.log(line),
      warn: (line) => console.error(line),
    });
    if (result.status === 'skipped') {
      console.log('donsetch: download skipped (DONSETCH_SKIP_DOWNLOAD=1)');
    }
    return 0;
  } catch (error) {
    if (error.code === 'UNSUPPORTED_PLATFORM') {
      console.error(`donsetch: ${error.message}`);
      console.error('');
      console.error('Supported platforms:');
      for (const line of supportedPlatforms()) console.error(`  ${line}`);
      console.error('');
      console.error(`Build from source: https://github.com/${REPO}`);
      return 1;
    }
    if (error.code === 'UNSUPPORTED_LIBC' || error.code === 'NO_TAR') {
      console.error(error.message);
      return 1;
    }
    if (ackExisting(error.message, console.log)) return 0;
    if (error.code === 'UNWRITABLE') {
      console.error(`donsetch: ${error.message} and no existing donsetch binary was found.`);
    } else {
      console.error(`donsetch: install failed: ${error.message}`);
    }
    console.error('');
    for (const line of optionsText()) console.error(line);
    return 1;
  }
}

if (require.main === module) {
  main().then((code) => process.exit(code));
}

module.exports = {
  install,
  InstallError,
  platformFor,
  supportedPlatforms,
  extractTarGz,
};
