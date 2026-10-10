#!/usr/bin/env node
'use strict';

// donsetch postinstall: download the prebuilt binary for this platform
// from GitHub Releases, verify SHA256, and extract to ./binaries/.

const http = require('http');
const https = require('https');
const tls = require('tls');
const crypto = require('crypto');
const fs = require('fs');
const path = require('path');
const { execFileSync } = require('child_process');
const { findExistingBinary, targetWritable } = require('./resolve-binary.js');
const { pipeline } = require('stream/promises');

const REPO = 'dondai44423/donsetch';
const VERSION = require('./package.json').version;
const TAG = process.env.DONSETCH_INSTALL_TAG || `v${VERSION}`;
const REQUEST_TIMEOUT_MS = 30_000;
const MAX_REDIRECTS = 5;
const RETRIES = 3;
const RETRY_BACKOFF_MS = [1_000, 3_000];

const PLATFORMS = {
  'linux-x64':    { asset: 'donsetch-linux-x64.tar.gz',    binary: 'donsetch' },
  'linux-arm64':  { asset: 'donsetch-linux-arm64.tar.gz',  binary: 'donsetch' },
  'darwin-arm64': { asset: 'donsetch-darwin-arm64.tar.gz', binary: 'donsetch' },
  'darwin-x64':   { asset: 'donsetch-darwin-x64.tar.gz',   binary: 'donsetch' },
  'win32-x64':    { asset: 'donsetch-win32-x64.tar.gz',    binary: 'donsetch.exe' },
  'win32-arm64':  { asset: 'donsetch-win32-x64.tar.gz',    binary: 'donsetch.exe', emulated: true },
};

const platKey = `${process.platform}-${process.arch}`;
const plat = PLATFORMS[platKey];

if (!plat) {
  console.error(`donsetch: unsupported platform ${platKey}`);
  console.error('');
  console.error('Supported platforms:');
  console.error('  linux-x64      Linux x86_64 (glibc)');
  console.error('  linux-arm64    Linux ARM64 (glibc)');
  console.error('  darwin-arm64   macOS Apple Silicon');
  console.error('  darwin-x64     macOS Intel');
  console.error('  win32-x64      Windows x86_64');
  console.error('  win32-arm64    Windows ARM64 (x64 emulation)');
  console.error('');
  console.error('Build from source: https://github.com/' + REPO);
  process.exit(1);
}

if (process.env.DONSETCH_SKIP_DOWNLOAD === '1') {
  console.log('donsetch: download skipped (DONSETCH_SKIP_DOWNLOAD=1)');
  process.exit(0);
}

if (plat.emulated) {
  console.log('donsetch: Windows arm64 uses the win32-x64 build under emulation.');
}

function detectMusl() {
  let interpreter;
  try {
    const fd = fs.openSync('/proc/self/exe', 'r');
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
    fs.closeSync(fd);
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

if (process.platform === 'linux'
    && process.env.DONSETCH_FORCE_GLIBC !== '1'
    && detectMusl()) {
  console.error('donsetch: musl libc detected (Alpine?).');
  console.error('The prebuilt Linux binaries are glibc-linked and will not run.');
  console.error('');
  console.error('Build from source:');
  console.error(`  git clone https://github.com/${REPO} && cd donsetch && cargo build --release`);
  console.error('Or use a glibc-based image (Debian, Ubuntu, Fedora).');
  console.error('Set DONSETCH_FORCE_GLIBC=1 only if a glibc compatibility layer is installed.');
  process.exit(1);
}

const binDir = path.join(__dirname, 'binaries');
const binaryPath = path.join(binDir, plat.binary);
const stampPath = `${binaryPath}.version`;

if (fs.existsSync(binaryPath) && fs.existsSync(stampPath)) {
  let stamp;
  try { stamp = fs.readFileSync(stampPath, 'utf8').trim(); } catch (_) {}
  if (stamp === VERSION) {
    console.log(`donsetch: binary already present (${plat.binary} ${VERSION})`);
    process.exit(0);
  }
  console.log(`donsetch: binary version stamp mismatch; re-downloading ${VERSION}`);
}

const releasesBase = (process.env.DONSETCH_RELEASES_BASE
  || `https://github.com/${REPO}/releases/download`).replace(/\/+$/, '');
const assetUrl = `${releasesBase}/${TAG}/${plat.asset}`;
const checksumUrl = `${assetUrl}.sha256`;

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function noProxy(host) {
  const value = process.env.NO_PROXY || process.env.no_proxy || '';
  return value.split(',').map((part) => part.trim().toLowerCase()).filter(Boolean).some((entry) => {
    if (entry === '*') return true;
    const normalized = entry.replace(/^\*\./, '').replace(/^\./, '');
    return host === normalized || host.endsWith(`.${normalized}`);
  });
}

function proxyFor(target) {
  if (noProxy(target.hostname)) return null;
  const value = target.protocol === 'https:'
    ? (process.env.HTTPS_PROXY || process.env.https_proxy || process.env.HTTP_PROXY || process.env.http_proxy)
    : (process.env.HTTP_PROXY || process.env.http_proxy);
  return value ? new URL(value) : null;
}

function requestDirect(target) {
  return new Promise((resolve, reject) => {
    const transport = target.protocol === 'https:' ? https : http;
    const req = transport.get(target, {
      headers: {
        Accept: 'application/octet-stream',
        'User-Agent': 'donsetch-npm-installer',
      },
    }, resolve);
    req.setTimeout(REQUEST_TIMEOUT_MS, () => req.destroy(new Error('request timeout')));
    req.on('error', reject);
  });
}

function requestThroughProxy(target, proxy) {
  return new Promise((resolve, reject) => {
    const connectTransport = proxy.protocol === 'https:' ? https : http;
    const proxyReq = connectTransport.request({
      hostname: proxy.hostname,
      port: proxy.port || (proxy.protocol === 'https:' ? 443 : 80),
      method: 'CONNECT',
      path: `${target.hostname}:${target.port || 443}`,
      headers: { Host: `${target.hostname}:${target.port || 443}` },
    });
    proxyReq.once('connect', (response, socket) => {
      if (response.statusCode !== 200) {
        socket.destroy();
        reject(new Error(`proxy CONNECT returned HTTP ${response.statusCode}`));
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
        },
        agent: false,
        createConnection: () => tls.connect({
          socket,
          servername: target.hostname,
        }),
      }, resolve);
      req.setTimeout(REQUEST_TIMEOUT_MS, () => req.destroy(new Error('request timeout')));
      req.on('error', reject);
      req.end();
    });
    proxyReq.setTimeout(REQUEST_TIMEOUT_MS, () => {
      proxyReq.destroy(new Error('proxy CONNECT timeout'));
    });
    proxyReq.on('error', reject);
    proxyReq.end();
  });
}

async function requestUrl(url) {
  const target = new URL(url);
  if (target.protocol !== 'https:') {
    throw new Error(`refusing non-HTTPS download URL ${url}`);
  }
  const proxy = proxyFor(target);
  return proxy ? requestThroughProxy(target, proxy) : requestDirect(target);
}

async function getWithRedirects(url) {
  let current = url;
  for (let hops = 0; hops <= MAX_REDIRECTS; hops++) {
    const response = await requestUrl(current);
    if (![301, 302, 303, 307, 308].includes(response.statusCode)) {
      return response;
    }
    response.resume();
    const location = response.headers.location;
    if (!location) throw new Error(`redirect without Location from ${current}`);
    const next = new URL(location, current).toString();
    if (!next.startsWith('https://')) {
      throw new Error(`refusing HTTP downgrade redirect to ${next}`);
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

async function download(url, dest) {
  for (let attempt = 0; attempt < RETRIES; attempt++) {
    try {
      const response = await getWithRedirects(url);
      if (response.statusCode !== 200) {
        const error = statusError(url, response.statusCode);
        await drain(response);
        throw error;
      }
      // pipeline destroys BOTH ends on error. A bare
      // `response.pipe(file)` leaves the write stream open when the
      // source errors (documented Node behavior), so the unlink below
      // fails with EBUSY on Windows and the partial download survives.
      await pipeline(response, fs.createWriteStream(dest));
      return;
    } catch (error) {
      try { fs.unlinkSync(dest); } catch (_) {}
      if (attempt + 1 >= RETRIES || error.retryable === false) throw error;
      const delay = RETRY_BACKOFF_MS[attempt] || RETRY_BACKOFF_MS[RETRY_BACKOFF_MS.length - 1];
      console.error(`donsetch: download attempt ${attempt + 1} failed (${error.message}); retrying in ${delay / 1000}s`);
      await sleep(delay);
    }
  }
}

async function main() {
  // Per-pid temp names: two first runs racing (two MCP clients
  // starting together) must never share a write target. The renames
  // that land the files are atomic; the temps are invisible until then.
  const tarball = path.join(binDir, `${plat.asset}.tmp-${process.pid}`);
  const checksumFile = path.join(binDir, `checksum.sha256.tmp-${process.pid}`);
  const extractDir = path.join(binDir, `extract.tmp-${process.pid}`);

  // The unwritable install dir (#361: a system-wide npm package under
  // a root-owned prefix, run by an unprivileged user) is decided
  // BEFORE any download, so the failure names the directory and the
  // ways out instead of dying after 19MB of network.
  try {
    fs.mkdirSync(binDir, { recursive: true });
  } catch (err) {
    settleUnwritable(err);
  }
  if (!targetWritable(__dirname)) {
    settleUnwritable(new Error(`${binDir} is not writable`));
  }
  cleanupStaleTemps();

  if (process.platform === 'win32') {
    try {
      execFileSync('tar', ['--version'], { stdio: 'ignore' });
    } catch (_) {
      console.error('donsetch: `tar` not found on this Windows system.');
      console.error('tar ships with Windows 10 1803+. Update Windows, or extract manually');
      console.error(`after downloading ${assetUrl}`);
      process.exit(1);
    }
  }

  console.log(`donsetch: downloading ${plat.asset} from ${TAG}...`);
  try {
    await download(assetUrl, tarball);
    console.log('donsetch: verifying checksum...');
    await download(checksumUrl, checksumFile);

    const expectedHash = fs.readFileSync(checksumFile, 'utf8').trim().split(/\s+/)[0];
    const actualHash = crypto.createHash('sha256').update(fs.readFileSync(tarball)).digest('hex');
    if (!/^[a-f0-9]{64}$/.test(expectedHash) || actualHash !== expectedHash) {
      throw new Error(`SHA256 mismatch (expected ${expectedHash}, actual ${actualHash})`);
    }

    console.log('donsetch: extracting...');
    fs.rmSync(extractDir, { recursive: true, force: true });
    fs.mkdirSync(extractDir, { recursive: true });
    execFileSync('tar', ['xzf', tarball, '-C', extractDir], { stdio: 'inherit' });

    const entries = fs.readdirSync(extractDir);
    if (!entries.includes(plat.binary)) {
      throw new Error(`expected ${plat.binary} not found after extraction in ${binDir}`);
    }
    // Land each entry with a rename inside binDir: a rename is atomic,
    // so a concurrent first run can only ever replace a complete file,
    // never expose a half-written one. The stamp goes last: a crash
    // before it leaves no stamp, and the next run re-downloads.
    for (const name of entries) {
      fs.renameSync(path.join(extractDir, name), path.join(binDir, name));
    }
    fs.rmSync(extractDir, { recursive: true, force: true });
    if (process.platform !== 'win32') fs.chmodSync(binaryPath, 0o755);
    fs.writeFileSync(stampPath, `${VERSION}\n`);
    console.log(`donsetch: installed ${plat.binary} to ${binaryPath}`);
  } finally {
    // Always drop the download artifacts: a failed checksum fetch, a
    // failed extract, or a missing binary must not leave the tarball
    // and checksum sitting in ./binaries/ forever.
    try { fs.unlinkSync(tarball); } catch (_) {}
    try { fs.unlinkSync(checksumFile); } catch (_) {}
    try { fs.rmSync(extractDir, { recursive: true, force: true }); } catch (_) {}
  }
}

// One existing-binary lookup for the failure paths: an install that
// cannot succeed must not take a working donsetch down with it (#361).
// Returns false when there is nothing to fall back on.
function ackExisting(detail) {
  let existing = null;
  try {
    existing = findExistingBinary({ pkgDir: __dirname });
  } catch (_) {
    return false;
  }
  if (!existing) return false;
  console.log(`donsetch: install failed: ${detail}`);
  console.log(`donsetch: an existing binary was found and will be used: ${existing.path}`);
  process.exit(0);
}

// The unwritable install dir: acknowledge an existing binary when one
// is findable (npm install / postinstall stays green), otherwise fail
// with every way out named.
function settleUnwritable(cause) {
  const detail = cause && cause.message ? cause.message : 'directory is not writable';
  if (ackExisting(detail)) return;
  console.error(
    `donsetch: cannot install into ${binDir} (${detail}) and no existing donsetch binary was found.`
  );
  console.error('');
  console.error('Options:');
  console.error('  - install under a user-writable prefix:');
  console.error('      npm config set prefix ~/.npm-global && npm install -g donsetch');
  console.error('  - point at a donsetch binary you already have:');
  console.error('      DONSETCH_BINARY=/path/to/donsetch');
  console.error('  - build from source:');
  console.error(`      git clone https://github.com/${REPO} && cd donsetch && cargo build --release`);
  process.exit(1);
}

// Temp leftovers from crashed runs (this installer is the only writer
// of `.tmp-<pid>` / `extract.tmp-<pid>` names in binDir). A pid that
// is still alive keeps its workspace.
function cleanupStaleTemps() {
  let names;
  try {
    names = fs.readdirSync(binDir);
  } catch (_) {
    return;
  }
  for (const name of names) {
    const m = /\.tmp-(\d+)$/.exec(name);
    if (!m || Number(m[1]) === process.pid) continue;
    let alive = false;
    try {
      process.kill(Number(m[1]), 0);
      alive = true;
    } catch (_) {/* not running (or not ours): its temps are stale */}
    if (!alive) {
      try { fs.rmSync(path.join(binDir, name), { recursive: true, force: true }); } catch (_) {}
    }
  }
}

main().catch((error) => {
  if (ackExisting(error.message)) return;
  console.error(`donsetch: install failed: ${error.message}`);
  console.error('');
  console.error('Options:');
  console.error('  - install under a user-writable prefix:');
  console.error('      npm config set prefix ~/.npm-global && npm install -g donsetch');
  console.error('  - point at a donsetch binary you already have:');
  console.error('      DONSETCH_BINARY=/path/to/donsetch');
  console.error('  - build from source:');
  console.error(`  git clone https://github.com/${REPO} && cd donsetch && cargo build --release`);
  process.exit(1);
});
