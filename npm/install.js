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

fs.mkdirSync(binDir, { recursive: true });

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
    let connected = false;
    proxyReq.once('connect', (response, socket) => {
      connected = true;
      if (response.statusCode !== 200) {
        socket.destroy();
        reject(new Error(`proxy CONNECT returned HTTP ${response.statusCode}`));
        return;
      }
      const secureSocket = tls.connect({
        socket,
        servername: target.hostname,
      });
      const tlsTimer = setTimeout(
        () => secureSocket.destroy(new Error('TLS timeout')),
        REQUEST_TIMEOUT_MS,
      );
      secureSocket.once('error', reject);
      secureSocket.once('secureConnect', () => {
        clearTimeout(tlsTimer);
        const requestTimer = setTimeout(
          () => secureSocket.destroy(new Error('request timeout')),
          REQUEST_TIMEOUT_MS,
        );
        const req = https.request({
          hostname: target.hostname,
          port: target.port || 443,
          path: `${target.pathname}${target.search}`,
          method: 'GET',
          headers: {
            Host: target.hostname,
            Accept: 'application/octet-stream',
            'User-Agent': 'donsetch-npm-installer',
          },
          agent: false,
          createConnection: () => secureSocket,
        }, (response) => {
          clearTimeout(requestTimer);
          let idleTimer = setTimeout(
            () => response.destroy(new Error('response timeout')),
            REQUEST_TIMEOUT_MS,
          );
          const refreshIdleTimer = () => {
            clearTimeout(idleTimer);
            idleTimer = setTimeout(
              () => response.destroy(new Error('response timeout')),
              REQUEST_TIMEOUT_MS,
            );
          };
          response.on('data', refreshIdleTimer);
          response.once('end', () => clearTimeout(idleTimer));
          response.once('close', () => clearTimeout(idleTimer));
          resolve(response);
        });
        req.on('error', (error) => {
          clearTimeout(requestTimer);
          reject(error);
        });
      });
    });
    proxyReq.on('error', (error) => {
      if (!connected) reject(error);
    });
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
      await new Promise((resolve, reject) => {
        const file = fs.createWriteStream(dest);
        response.pipe(file);
        file.on('finish', () => {
          file.close(resolve);
        });
        file.on('error', reject);
        response.on('error', reject);
      });
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
  const tarball = path.join(binDir, plat.asset);
  const checksumFile = path.join(binDir, 'checksum.sha256');

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
  await download(assetUrl, tarball);
  console.log('donsetch: verifying checksum...');
  await download(checksumUrl, checksumFile);

  const expectedHash = fs.readFileSync(checksumFile, 'utf8').trim().split(/\s+/)[0];
  const actualHash = crypto.createHash('sha256').update(fs.readFileSync(tarball)).digest('hex');
  if (!/^[a-f0-9]{64}$/.test(expectedHash) || actualHash !== expectedHash) {
    try { fs.unlinkSync(tarball); } catch (_) {}
    try { fs.unlinkSync(checksumFile); } catch (_) {}
    throw new Error(`SHA256 mismatch (expected ${expectedHash}, actual ${actualHash})`);
  }

  console.log('donsetch: extracting...');
  execFileSync('tar', ['xzf', tarball, '-C', binDir], { stdio: 'inherit' });
  try { fs.unlinkSync(tarball); } catch (_) {}
  try { fs.unlinkSync(checksumFile); } catch (_) {}

  if (!fs.existsSync(binaryPath)) {
    throw new Error(`expected ${plat.binary} not found after extraction in ${binDir}`);
  }
  if (process.platform !== 'win32') fs.chmodSync(binaryPath, 0o755);
  fs.writeFileSync(stampPath, `${VERSION}\n`);
  console.log(`donsetch: installed ${plat.binary} to ${binaryPath}`);
}

main().catch((error) => {
  console.error(`donsetch: install failed: ${error.message}`);
  console.error('');
  console.error('You can build from source:');
  console.error(`  git clone https://github.com/${REPO} && cd donsetch && cargo build --release`);
  process.exit(1);
});
