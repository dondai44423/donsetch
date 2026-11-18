#!/usr/bin/env node
'use strict';

// Binary resolution for the npm package, shared by every consumer:
// bin/donsetch.js (the CLI / MCP wrapper), install.js (failure-path
// acknowledgement), and pi-extension.ts.
//
// Order:
//   1. DONSETCH_BINARY            explicit override, wins over everything
//   2. <pkg>/binaries/<name>      the pinned copy (upgraded if stamp < package.json)
//   3. versioned user cache       read-only global packages need no sudo
//   4. install.js                 fetch the pinned copy on first use / upgrade
//   5. an existing binary         stale local copy or PATH, $CARGO_HOME/bin, ~/.cargo/bin
//   6. hard failure               curated, names every way out
//
// The installer runs IN THIS PROCESS (`install()` from install.js),
// never as `process.execPath install.js`: under a compiled host —
// Pi-Bolt is a Bun-compiled pi — process.execPath is that host, not a
// JS runtime, and it rejects install.js's arguments as its own
// (issue #450). A host that can run this file can run install.js.

const fs = require('fs');
const path = require('path');
const os = require('os');
const { execFileSync } = require('child_process');

const BINARY_NAME = process.platform === 'win32' ? 'donsetch.exe' : 'donsetch';

function isExecutableFile(p) {
  try {
    if (!fs.statSync(p).isFile()) return false;
    if (process.platform !== 'win32') fs.accessSync(p, fs.constants.X_OK);
    return true;
  } catch (_) {
    return false;
  }
}

function real(p) {
  try {
    return fs.realpathSync(p);
  } catch (_) {
    return p;
  }
}

function packageBinary(pkgDir) {
  return path.join(pkgDir, 'binaries', BINARY_NAME);
}

// A system-wide npm package can be read-only to its caller. First-run
// installation still works without sudo, in a versioned per-user cache.
function cachedBinary(pkgDir) {
  const version = require(path.join(pkgDir, 'package.json')).version;
  const home = os.homedir();
  let root = process.env.DONSETCH_CACHE_DIR;
  if (!root) {
    const base = process.platform === 'win32'
      ? (process.env.LOCALAPPDATA || path.join(home, 'AppData', 'Local'))
      : process.platform === 'darwin'
        ? path.join(home, 'Library', 'Caches')
        : (process.env.XDG_CACHE_HOME || path.join(home, '.cache'));
    root = path.join(base, 'donsetch');
  }
  return path.join(root, 'npm', version, `${process.platform}-${process.arch}`, BINARY_NAME);
}

// Parse basic semver `major.minor.patch[-prerelease]`.
function parseSemver(ver) {
  const m = /^v?(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?$/.exec(String(ver || '').trim());
  if (!m) return null;
  return {
    major: Number(m[1]),
    minor: Number(m[2]),
    patch: Number(m[3]),
    pre: m[4] || null,
  };
}

function compareSemver(a, b) {
  const va = parseSemver(a);
  const vb = parseSemver(b);
  if (!va || !vb) return 0;
  if (va.major !== vb.major) return va.major < vb.major ? -1 : 1;
  if (va.minor !== vb.minor) return va.minor < vb.minor ? -1 : 1;
  if (va.patch !== vb.patch) return va.patch < vb.patch ? -1 : 1;
  if (va.pre && !vb.pre) return -1;
  if (!va.pre && vb.pre) return 1;
  if (va.pre && vb.pre && va.pre !== vb.pre) return va.pre < vb.pre ? -1 : 1;
  return 0;
}

// True when `<binaryPath>.version` exists and records a version strictly
// older than `pinned`. Unstamped binaries (dev builds, fixtures) and
// newer binaries (`donsetch update`) are kept as-is.
function needsStampedUpgrade(binaryPath, pinned) {
  if (!pinned || process.env.DONSETCH_SKIP_DOWNLOAD === '1') return false;
  const stampPath = `${binaryPath}.version`;
  if (!fs.existsSync(stampPath)) return false;
  try {
    const stamp = fs.readFileSync(stampPath, 'utf8').trim();
    if (!stamp) return false;
    return compareSemver(stamp, pinned) < 0;
  } catch (_) {
    return false;
  }
}

// The package-local copy, with a chmod rescue: an executable bit lost
// in transit (copied tree, artifact download) is repaired in place
// rather than triggering a re-download.
function usableLocal(p) {
  if (!fs.existsSync(p)) return false;
  if (isExecutableFile(p)) return true;
  if (process.platform !== 'win32') {
    try {
      fs.chmodSync(p, 0o755);
    } catch (_) {/* fall through to the recheck */}
  }
  return isExecutableFile(p);
}

// True when install.js could write into <pkgDir>/binaries. A missing
// directory is judged by its parent.
function targetWritable(pkgDir) {
  const binDir = path.join(pkgDir, 'binaries');
  try {
    if (fs.existsSync(binDir)) {
      fs.accessSync(binDir, fs.constants.W_OK);
      return true;
    }
    fs.accessSync(pkgDir, fs.constants.W_OK);
    return true;
  } catch (_) {
    return false;
  }
}

// The one explicit override. Invalid values fail loudly: a DONSETCH_BINARY
// that names nothing is a config bug, not a fall-through case.
function overrideBinary() {
  const over = process.env.DONSETCH_BINARY;
  if (!over) return null;
  const p = path.resolve(over);
  if (!isExecutableFile(p)) {
    throw new Error(
      `DONSETCH_BINARY is set to ${p}, which is not an executable file. ` +
        `Unset it or point it at a donsetch binary.`
    );
  }
  return { path: p, source: 'env' };
}

// An already-installed donsetch outside this package: PATH entries
// (skipping empty ones, which mean the current directory), then the
// cargo bin directories even when they are not on PATH.
function findExistingBinary(opts) {
  const pkgDir = opts.pkgDir;

  const over = overrideBinary();
  if (over) return over;

  const banned = new Set([real(path.join(pkgDir, 'bin', 'donsetch.js'))]);
  if (opts.self) banned.add(real(opts.self));
  const pkgRoot = real(pkgDir);
  const pkgPrefix = pkgRoot.endsWith(path.sep) ? pkgRoot : pkgRoot + path.sep;

  const candidates = [];
  const seen = new Set();
  const add = (p) => {
    const r = real(p);
    if (seen.has(r)) return;
    seen.add(r);
    candidates.push({ p, r });
  };

  for (const dir of (process.env.PATH || '').split(path.delimiter)) {
    if (!dir) continue;
    add(path.join(dir, BINARY_NAME));
  }
  const home = process.env.HOME || process.env.USERPROFILE;
  if (process.env.CARGO_HOME) add(path.join(process.env.CARGO_HOME, 'bin', BINARY_NAME));
  if (home) add(path.join(home, '.cargo', 'bin', BINARY_NAME));

  for (const c of candidates) {
    if (banned.has(c.r)) continue;
    if (c.r === pkgRoot || c.r.startsWith(pkgPrefix)) continue;
    if (!isExecutableFile(c.p)) continue;
    return { path: c.p, source: 'path' };
  }
  return null;
}

function noBinaryMessage(why) {
  return [
    `could not install the donsetch binary and none was found (${why}).`,
    'Options:',
    '  - install under a user-writable prefix:',
    '      npm config set prefix ~/.npm-global && npm install -g donsetch',
    '  - point at a donsetch binary you already have:',
    '      DONSETCH_BINARY=/path/to/donsetch',
    '  - build from source:',
    '      git clone https://github.com/dondai44423/donsetch && cd donsetch && cargo build --release',
  ].join('\n');
}

async function runInstaller(pkgDir, onOutput, binDir, extraOpts = {}) {
  const installScript = path.join(pkgDir, 'install.js');
  if (!fs.existsSync(installScript)) {
    return { ok: false, error: `installer missing: ${installScript}` };
  }
  let install;
  try {
    ({ install } = require(installScript));
  } catch (err) {
    return { ok: false, error: `installer unavailable: ${String(err.message || err).trim()}` };
  }
  if (typeof install !== 'function') {
    return { ok: false, error: `installer exports no install() function: ${installScript}` };
  }
  const log = (line) => {
    if (onOutput) onOutput(String(line) + '\n');
  };
  try {
    const result = await install({ binDir, log, warn: log, ...extraOpts });
    return { ok: true, result };
  } catch (err) {
    const lines = String(err.message || err).trim().split('\n');
    return { ok: false, error: (lines[0] || 'installer failed').trim() };
  }
}

function binaryVersion(p) {
  try {
    const out = execFileSync(p, ['--version'], {
      encoding: 'utf8',
      timeout: 5_000,
      stdio: ['ignore', 'pipe', 'ignore'],
      windowsHide: true,
    });
    const m = /\b(\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?)\b/.exec(String(out));
    return m ? m[1] : null;
  } catch (_) {
    return null;
  }
}

async function resolveBinary(opts) {
  const pkgDir = opts.pkgDir;
  const onOutput = opts.onOutput;
  const stream = opts.stream || null;
  const isTty = Boolean(stream && stream.isTTY && process.env.TERM !== 'dumb');
  const pinned = pinnedVersion(pkgDir);

  const over = overrideBinary();
  if (over) return over;

  const local = packageBinary(pkgDir);
  let staleLocal = null;
  if (usableLocal(local)) {
    if (!needsStampedUpgrade(local, pinned)) {
      return { path: local, source: 'package' };
    }
    staleLocal = local;
  }

  const cached = cachedBinary(pkgDir);
  if (usableLocal(cached)) return { path: cached, source: 'cache' };

  let why;
  if (process.env.DONSETCH_SKIP_DOWNLOAD === '1') {
    why = 'download skipped (DONSETCH_SKIP_DOWNLOAD=1)';
  } else if (!targetWritable(pkgDir)) {
    why = `${path.join(pkgDir, 'binaries')} is not writable`;
    if (!isTty && onOutput) {
      onOutput('donsetch: first run; installing the verified binary in your user cache...\n');
    }
    const result = await runInstaller(pkgDir, onOutput, path.dirname(cached), {
      stream,
      cacheInstall: true,
    });
    if (usableLocal(cached)) return { path: cached, source: 'cache' };
    why = result.ok ? 'the installer did not produce the binary' : result.error;
  } else {
    if (!isTty && onOutput) {
      onOutput(
        staleLocal
          ? `donsetch: upgrading package binary to ${pinned}...\n`
          : 'donsetch: first run; downloading the verified binary...\n'
      );
    }
    const result = await runInstaller(pkgDir, onOutput, path.dirname(local), {
      stream,
      cacheInstall: false,
    });
    if (usableLocal(local) && !needsStampedUpgrade(local, pinned)) {
      return { path: local, source: 'package' };
    }
    why = result.ok ? 'the installer did not produce the binary' : result.error;
  }

  if (staleLocal && usableLocal(staleLocal)) {
    return existingNotice({ path: staleLocal, source: 'package' }, why, pinned);
  }

  const existing = findExistingBinary({ pkgDir, self: opts.self });
  if (existing) return existingNotice(existing, why, pinned);
  throw new Error(noBinaryMessage(why));
}

function pinnedVersion(pkgDir) {
  try {
    return require(path.join(pkgDir, 'package.json')).version || null;
  } catch (_) {
    return null;
  }
}

function existingNotice(existing, why, pinned) {
  const found = binaryVersion(existing.path);
  const version = found ? `version ${found}` : 'version unknown';
  const mismatch = found && pinned && found !== pinned
    ? `; this package pins ${pinned}`
    : '';
  return {
    ...existing,
    notice: `donsetch: package binary unavailable (${why}); using existing binary: ${existing.path} (${version}${mismatch})`,
  };
}

module.exports = {
  resolveBinary,
  findExistingBinary,
  targetWritable,
  packageBinary,
  cachedBinary,
  compareSemver,
  needsStampedUpgrade,
};

// ── CLI mode ─────────────────────────────────────────────────────
if (require.main === module) {
  const args = process.argv.slice(2);
  const pkgIdx = args.indexOf('--pkg-dir');
  const fileIdx = args.indexOf('--print-file');
  if (pkgIdx < 0) {
    process.stderr.write('usage: resolve-binary.js --pkg-dir <dir> [--print-file <path>]\n');
    process.exit(2);
  }
  const pkgDir = path.resolve(args[pkgIdx + 1] || '');
  const outFile = fileIdx >= 0 ? args[fileIdx + 1] : null;
  resolveBinary({
    pkgDir,
    self: process.argv[1],
    stream: process.stderr.isTTY ? process.stderr : null,
    onOutput: (chunk) => process.stderr.write(chunk),
  })
    .then((resolved) => {
      if (resolved.notice) process.stderr.write(resolved.notice + '\n');
      if (outFile) {
        fs.writeFileSync(outFile, resolved.path + '\n');
      } else {
        process.stdout.write(resolved.path + '\n');
      }
      process.exit(0);
    })
    .catch((err) => {
      process.stderr.write(String(err.message || err) + '\n');
      process.exit(1);
    });
}
