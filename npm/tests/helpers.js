'use strict';

// Shared scaffolding for the npm installer tests. Every test runs
// against a scratch copy of the package files, with a fully controlled
// environment (PATH, HOME, CARGO_HOME) so resolution can never pick up
// the machine's own donsetch install by accident.

const crypto = require('crypto');
const https = require('https');
const { execFileSync, spawn, spawnSync } = require('child_process');
const { chmodSync, copyFileSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } = require('fs');
const { tmpdir } = require('os');
const { join } = require('path');

const PKG_SRC = join(__dirname, '..');
const NODE = process.execPath;

const IS_POSIX = process.platform !== 'win32';
const IS_ROOT = typeof process.getuid === 'function' && process.getuid() === 0;

// chmod 0555 does not stop root, so the unwritable-target tests need a
// non-root uid to simulate issue #361 faithfully.
const SKIP_IF_ROOT = IS_ROOT ? 'runs as root: a 0555 directory stays writable' : false;

function scratch(tag) {
  return mkdtempSync(join(tmpdir(), `donsetch-npm-${tag}-`));
}

// The files npm ships; `binaries` seeds the package-local copy.
function stagePkg(dst, { binaries = null } = {}) {
  mkdirSync(join(dst, 'bin'), { recursive: true });
  for (const f of ['install.js', 'resolve-binary.js', 'package.json']) {
    copyFileSync(join(PKG_SRC, f), join(dst, f));
  }
  copyFileSync(join(PKG_SRC, 'bin', 'donsetch.js'), join(dst, 'bin', 'donsetch.js'));
  if (binaries) {
    mkdirSync(join(dst, 'binaries'), { recursive: true });
    for (const [name, body] of Object.entries(binaries)) {
      const p = join(dst, 'binaries', name);
      writeFileSync(p, body);
      chmodSync(p, 0o755);
    }
  }
  return dst;
}

// An executable stub that identifies itself in stdout, standing in for
// the native binary.
function stubBinary(dir, marker, { exit = 0 } = {}) {
  mkdirSync(dir, { recursive: true });
  const p = join(dir, 'donsetch');
  writeFileSync(p, `#!/bin/sh\necho ${marker} "$@"\nexit ${exit}\n`);
  chmodSync(p, 0o755);
  return p;
}

// Fully controlled env: no leaking of the machine's PATH donsetch,
// ~/.cargo, DONSETCH_* or proxy settings into resolution.
function cleanEnv({ path = [], home, extra = {} } = {}) {
  return {
    PATH: path.join(process.platform === 'win32' ? ';' : ':'),
    HOME: home,
    ...extra,
  };
}

function runNode(args, { env, cwd, timeout = 120_000 } = {}) {
  const r = spawnSync(NODE, args, { env, cwd, timeout, encoding: 'utf8' });
  return { code: r.status, stdout: r.stdout || '', stderr: r.stderr || '' };
}

function spawnNode(args, { env, cwd, timeout = 60_000 } = {}) {
  return new Promise((resolve) => {
    const child = spawn(NODE, args, { env, cwd, stdio: ['ignore', 'pipe', 'pipe'] });
    let stdout = '';
    let stderr = '';
    let killed = false;
    const timer = setTimeout(() => {
      killed = true;
      child.kill('SIGKILL');
    }, timeout);
    child.stdout.on('data', (d) => (stdout += d));
    child.stderr.on('data', (d) => (stderr += d));
    child.on('close', (code) => {
      clearTimeout(timer);
      resolve({ code: killed ? null : code, stdout, stderr });
    });
  });
}

// A system tool resolved by name. The installer shells out to tar
// (which itself needs gzip), and the tests hand children a PATH that
// contains exactly those and nothing that could resolve as a donsetch.
function toolPath(name) {
  return execFileSync('/bin/sh', ['-c', `command -v ${name}`], { encoding: 'utf8' }).trim();
}

function assetName() {
  const names = {
    'linux-x64': 'donsetch-linux-x64.tar.gz',
    'linux-arm64': 'donsetch-linux-arm64.tar.gz',
    'darwin-arm64': 'donsetch-darwin-arm64.tar.gz',
    'darwin-x64': 'donsetch-darwin-x64.tar.gz',
    'win32-x64': 'donsetch-win32-x64.tar.gz',
    'win32-arm64': 'donsetch-win32-x64.tar.gz',
  };
  return names[`${process.platform}-${process.arch}`] || null;
}

function makeTlsFixture(root) {
  const keyPath = join(root, 'key.pem');
  const certPath = join(root, 'cert.pem');
  execFileSync(
    'openssl',
    [
      'req', '-x509', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:prime256v1',
      '-keyout', keyPath, '-out', certPath, '-days', '2', '-nodes',
      '-subj', '/CN=127.0.0.1', '-addext', 'subjectAltName=IP:127.0.0.1',
    ],
    { stdio: 'ignore' }
  );
  return { keyPath, certPath };
}

function makeTarball(root, files) {
  const stage = join(root, 'stage');
  mkdirSync(stage, { recursive: true });
  for (const [name, body] of Object.entries(files)) {
    writeFileSync(join(stage, name), body);
  }
  const tarPath = join(root, 'asset.tar.gz');
  execFileSync('tar', ['czf', tarPath, '-C', stage, ...Object.keys(files)]);
  return tarPath;
}

function sha256File(p) {
  return crypto.createHash('sha256').update(readFileSync(p)).digest('hex');
}

// A local release mirror speaking HTTPS (the installer refuses
// non-HTTPS download URLs, so the fixture must be the real thing).
function startReleaseServer(tls, { tag, asset, tarballPath, sha }) {
  const state = { hits: [] };
  const server = https.createServer(
    { key: readFileSync(tls.keyPath), cert: readFileSync(tls.certPath) },
    (req, res) => {
      state.hits.push(req.url);
      if (req.url === `/${tag}/${asset}`) {
        res.writeHead(200);
        res.end(readFileSync(tarballPath));
      } else if (req.url === `/${tag}/${asset}.sha256`) {
        res.writeHead(200);
        res.end(`${sha}\n`);
      } else {
        res.writeHead(404);
        res.end('not found');
      }
    }
  );
  return new Promise((resolve) => {
    server.listen(0, '127.0.0.1', () => {
      state.port = server.address().port;
      state.close = () =>
        new Promise((r) => {
          // Node keeps idle keep-alive sockets open; close() alone
          // would wait on them forever and hang the suite.
          server.closeAllConnections();
          server.close(r);
        });
      resolve(state);
    });
  });
}

module.exports = {
  IS_POSIX,
  SKIP_IF_ROOT,
  assetName,
  cleanEnv,
  makeTarball,
  makeTlsFixture,
  runNode,
  sha256File,
  spawnNode,
  scratch,
  stagePkg,
  startReleaseServer,
  stubBinary,
  toolPath,
};
