#!/usr/bin/env node
'use strict';

// Binary resolution for the npm package, shared by every consumer:
// bin/donsetch.js (the CLI / MCP wrapper), install.js (failure-path
// acknowledgement), and pi-extension.ts.
//
// Order:
//   1. DONSETCH_BINARY            explicit override, wins over everything
//   2. <pkg>/binaries/<name>      the pinned copy this package installs
//   3. versioned user cache       read-only global packages need no sudo
//   4. install.js                 fetch the pinned copy on first use
//   5. an existing binary         PATH, $CARGO_HOME/bin, ~/.cargo/bin
//   6. hard failure               curated, names every way out
//
// The installer runs IN THIS PROCESS (`install()` from install.js),
// never as `process.execPath install.js`: under a compiled host —
// Pi-Bolt is a Bun-compiled pi — process.execPath is that host, not a
// JS runtime, and it rejects install.js's arguments as its own
// (issue #450). A host that can run this file can run install.js.
//
// The fallback exists because the pinned copy is not always obtainable
// (issue #361): a system-wide npm package under a root-owned prefix
// cannot be written by the user running it, and an install that cannot
// succeed must not take a working donsetch down with it. The wrapper
// itself is never a candidate: on a global npm install the PATH entry
// named `donsetch` is a symlink back into this package, so candidates
// are compared by their real path and anything resolving into the
// package is skipped. A fallback names its version, and says so when
// it is not the version this package pins (#450).

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
// directory is judged by its parent. Advisory only: the install itself
// still handles a late failure, but a known-unwritable target skips a
// doomed 19MB download first (#361).
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

// Run install.js in this process. Lazy so a resolved binary never pays
// for loading the download machinery, and so the CLI's plain
// resolution paths work even where install.js cannot load.
async function runInstaller(pkgDir, onOutput, binDir) {
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
    const result = await install({ binDir, log, warn: log });
    return { ok: true, result };
  } catch (err) {
    const lines = String(err.message || err).trim().split('\n');
    return { ok: false, error: (lines[0] || 'installer failed').trim() };
  }
}

// Resolve the binary for a run. `onOutput` receives installer output
// chunks; callers point it at stderr (never stdout: an MCP stdio
// session owns that channel).
// What a candidate binary says it is. Best effort: a binary for another
// platform, or one that will not start, has no version to report.
function binaryVersion(p) {
  try {
    const out = execFileSync(p, ['--version'], {
      encoding: 'utf8',
      timeout: 5_000,
      stdio: ['ignore', 'pipe', 'ignore'],
      windowsHide: true,
    });
    const m = /\b(\d+\.\d+\.\d+)\b/.exec(String(out));
    return m ? m[1] : null;
  } catch (_) {
    return null;
  }
}

async function resolveBinary(opts) {
  const pkgDir = opts.pkgDir;
  const onOutput = opts.onOutput;

  const over = overrideBinary();
  if (over) return over;

  const local = packageBinary(pkgDir);
  if (usableLocal(local)) return { path: local, source: 'package' };

  const cached = cachedBinary(pkgDir);
  if (usableLocal(cached)) return { path: cached, source: 'cache' };

  let why;
  if (process.env.DONSETCH_SKIP_DOWNLOAD === '1') {
    why = 'download skipped (DONSETCH_SKIP_DOWNLOAD=1)';
  } else if (!targetWritable(pkgDir)) {
    why = `${path.join(pkgDir, 'binaries')} is not writable`;
    if (onOutput) onOutput('donsetch: first run; installing the verified binary in your user cache...\n');
    const result = await runInstaller(pkgDir, onOutput, path.dirname(cached));
    if (usableLocal(cached)) return { path: cached, source: 'cache' };
    why = result.ok ? 'the installer did not produce the binary' : result.error;
  } else {
    if (onOutput) onOutput('donsetch: first run; downloading the verified binary...\n');
    const result = await runInstaller(pkgDir, onOutput, path.dirname(local));
    if (usableLocal(local)) return { path: local, source: 'package' };
    // The installer can return 'present'/'installed' without the file
    // landing where this resolver looks (an interrupted writer): name
    // that, never "undefined".
    why = result.ok ? 'the installer did not produce the binary' : result.error;
  }

  const existing = findExistingBinary({ pkgDir, self: opts.self });
  if (existing) return existingNotice(existing, why, pinnedVersion(pkgDir));
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

module.exports = { resolveBinary, findExistingBinary, targetWritable, packageBinary, cachedBinary };

// ── CLI mode ─────────────────────────────────────────────────────
// `--print` writes the resolved path to
// stdout; `--print-file <path>` writes it to a file and keeps stdout
// free for an inheriting caller. Progress and failures go to stderr.
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
