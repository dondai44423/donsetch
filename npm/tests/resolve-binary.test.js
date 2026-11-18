'use strict';

// resolve-binary.js: the resolution order that #361 established.
//   DONSETCH_BINARY > package-local > installer > existing binary.

const { test } = require('node:test');
const assert = require('node:assert');
const { chmodSync, mkdirSync, readFileSync, symlinkSync, writeFileSync } = require('fs');
const { join } = require('path');
const h = require('./helpers.js');

function resolve(pkgDir, { path = [], home, extra = {} } = {}) {
  mkdirSync(home, { recursive: true });
  const env = h.cleanEnv({ path, home, extra });
  const r = h.runNode(
    [join(pkgDir, 'resolve-binary.js'), '--print', '--pkg-dir', pkgDir],
    { env }
  );
  return { ...r, out: r.stdout.trim() };
}

test('package-local binary wins over anything on PATH', (t) => {
  if (!h.IS_POSIX) return t.skip('posix-only fixture');
  const root = h.scratch('local');
  const pkg = h.stagePkg(join(root, 'pkg'), { binaries: { donsetch: '#!/bin/sh\necho LOCAL\n' } });
  h.stubBinary(join(root, 'userbin'), 'USERBIN');
  const r = resolve(pkg, { path: [join(root, 'userbin')], home: join(root, 'home') });
  assert.equal(r.code, 0, r.stderr);
  assert.equal(r.out, join(pkg, 'binaries', 'donsetch'));
  assert.doesNotMatch(r.stderr, /using existing binary/);
});

test('a non-executable package-local binary is repaired, not re-downloaded', (t) => {
  if (!h.IS_POSIX) return t.skip('posix-only fixture');
  const root = h.scratch('chmod');
  const pkg = h.stagePkg(join(root, 'pkg'), { binaries: { donsetch: '#!/bin/sh\necho LOCAL\n' } });
  chmodSync(join(pkg, 'binaries', 'donsetch'), 0o644);
  const r = resolve(pkg, { path: [], home: join(root, 'home') });
  assert.equal(r.code, 0, r.stderr);
  assert.equal(r.out, join(pkg, 'binaries', 'donsetch'));
});

test('DONSETCH_BINARY overrides even the package-local binary', (t) => {
  if (!h.IS_POSIX) return t.skip('posix-only fixture');
  const root = h.scratch('env');
  const pkg = h.stagePkg(join(root, 'pkg'), { binaries: { donsetch: '#!/bin/sh\necho LOCAL\n' } });
  const over = h.stubBinary(join(root, 'overbin'), 'OVERBIN');
  const r = resolve(pkg, {
    path: [],
    home: join(root, 'home'),
    extra: { DONSETCH_BINARY: over },
  });
  assert.equal(r.code, 0, r.stderr);
  assert.equal(r.out, over);
});

test('an invalid DONSETCH_BINARY fails loudly instead of falling through', (t) => {
  if (!h.IS_POSIX) return t.skip('posix-only fixture');
  const root = h.scratch('env-bad');
  const pkg = h.stagePkg(join(root, 'pkg'));
  h.stubBinary(join(root, 'userbin'), 'USERBIN');
  const r = resolve(pkg, {
    path: [join(root, 'userbin')],
    home: join(root, 'home'),
    extra: { DONSETCH_BINARY: join(root, 'missing-donsetch') },
  });
  assert.equal(r.code, 1);
  assert.match(r.stderr, /DONSETCH_BINARY is set to/);
  assert.doesNotMatch(r.out, /userbin/);
});

test('falls back to PATH when the package binary cannot be installed', (t) => {
  if (!h.IS_POSIX) return t.skip('posix-only fixture');
  const root = h.scratch('path');
  const pkg = h.stagePkg(join(root, 'pkg'));
  const userbin = join(root, 'userbin');
  h.stubBinary(userbin, 'USERBIN');
  const r = resolve(pkg, {
    path: [userbin],
    home: join(root, 'home'),
    extra: { DONSETCH_SKIP_DOWNLOAD: '1' },
  });
  assert.equal(r.code, 0, r.stderr);
  assert.equal(r.out, join(userbin, 'donsetch'));
  assert.match(r.stderr, /download skipped \(DONSETCH_SKIP_DOWNLOAD=1\)/);
  assert.match(r.stderr, /using existing binary: .*userbin/);
});

test('the npm wrapper symlink on PATH is never picked as a candidate', (t) => {
  if (!h.IS_POSIX) return t.skip('posix-only fixture');
  const root = h.scratch('symlink');
  const pkg = h.stagePkg(join(root, 'pkg'));
  const linkdir = join(root, 'npm-bin');
  mkdirSync(linkdir, { recursive: true });
  symlinkSync(join(pkg, 'bin', 'donsetch.js'), join(linkdir, 'donsetch'));
  const userbin = join(root, 'userbin');
  h.stubBinary(userbin, 'USERBIN');
  const r = resolve(pkg, {
    path: [linkdir, userbin],
    home: join(root, 'home'),
    extra: { DONSETCH_SKIP_DOWNLOAD: '1' },
  });
  assert.equal(r.code, 0, r.stderr);
  assert.equal(r.out, join(userbin, 'donsetch'), 'resolved to the wrapper itself');
});

test('the npm shim directory alone resolves to nothing, not to a loop', (t) => {
  if (!h.IS_POSIX) return t.skip('posix-only fixture');
  const root = h.scratch('symlink-only');
  const pkg = h.stagePkg(join(root, 'pkg'));
  const linkdir = join(root, 'npm-bin');
  mkdirSync(linkdir, { recursive: true });
  symlinkSync(join(pkg, 'bin', 'donsetch.js'), join(linkdir, 'donsetch'));
  const r = resolve(pkg, {
    path: [linkdir],
    home: join(root, 'home'),
    extra: { DONSETCH_SKIP_DOWNLOAD: '1' },
  });
  assert.equal(r.code, 1);
  assert.match(r.stderr, /none was found/);
});

test('$CARGO_HOME/bin is consulted even when absent from PATH', (t) => {
  if (!h.IS_POSIX) return t.skip('posix-only fixture');
  const root = h.scratch('cargo-home');
  const pkg = h.stagePkg(join(root, 'pkg'));
  const cargoHome = join(root, 'cargo');
  h.stubBinary(join(cargoHome, 'bin'), 'CARGOBIN');
  const r = resolve(pkg, {
    path: [],
    home: join(root, 'home'),
    extra: { CARGO_HOME: cargoHome, DONSETCH_SKIP_DOWNLOAD: '1' },
  });
  assert.equal(r.code, 0, r.stderr);
  assert.equal(r.out, join(cargoHome, 'bin', 'donsetch'));
});

test('~/.cargo/bin is consulted even when absent from PATH', (t) => {
  if (!h.IS_POSIX) return t.skip('posix-only fixture');
  const root = h.scratch('cargo-user');
  const pkg = h.stagePkg(join(root, 'pkg'));
  const home = join(root, 'home');
  h.stubBinary(join(home, '.cargo', 'bin'), 'CARGOUSER');
  const r = resolve(pkg, {
    path: [],
    home,
    extra: { DONSETCH_SKIP_DOWNLOAD: '1' },
  });
  assert.equal(r.code, 0, r.stderr);
  assert.equal(r.out, join(home, '.cargo', 'bin', 'donsetch'));
});

test('nothing anywhere: exit 1 with every way out named', (t) => {
  if (!h.IS_POSIX) return t.skip('posix-only fixture');
  const root = h.scratch('none');
  const pkg = h.stagePkg(join(root, 'pkg'));
  const empty = join(root, 'empty');
  mkdirSync(empty, { recursive: true });
  const r = resolve(pkg, {
    path: [empty],
    home: join(root, 'home'),
    extra: { DONSETCH_SKIP_DOWNLOAD: '1' },
  });
  assert.equal(r.code, 1);
  assert.match(r.stderr, /could not install the donsetch binary and none was found/);
  assert.match(r.stderr, /DONSETCH_BINARY=\/path\/to\/donsetch/);
  assert.match(r.stderr, /npm config set prefix/);
});

test('installer failure falls back to PATH with a named reason, never "undefined"', (t) => {
  if (!h.IS_POSIX) return t.skip('posix-only fixture');
  const root = h.scratch('installer-fail');
  // Writable target: the installer RUNS and fails (dead releases
  // host), instead of being skipped up front. Its own acknowledgement
  // exits 0, and the resolver must still name why the package copy is
  // missing. Spends the installer's 1s+3s retry backoff by design.
  const pkg = h.stagePkg(join(root, 'pkg'));
  const userbin = join(root, 'userbin');
  h.stubBinary(userbin, 'USERBIN');
  const r = resolve(pkg, {
    path: [userbin],
    home: join(root, 'home'),
    extra: { DONSETCH_RELEASES_BASE: 'https://127.0.0.1:1/x' },
  });
  assert.equal(r.code, 0, r.stderr);
  assert.equal(r.out, join(userbin, 'donsetch'));
  assert.match(r.stderr, /using existing binary/);
  assert.doesNotMatch(r.stderr, /undefined/);
});

test('a fallback names its version, and says when it is not the pinned one', (t) => {
  if (!h.IS_POSIX) return t.skip('posix-only fixture');
  const root = h.scratch('fallback-version');
  const pkg = h.stagePkg(join(root, 'pkg'));
  const userbin = join(root, 'userbin');
  mkdirSync(userbin, { recursive: true });
  const other = join(userbin, 'donsetch');
  writeFileSync(other, '#!/bin/sh\nif [ "$1" = "--version" ]; then echo "DonSeTch 9.9.9"; else echo STUB; fi\n');
  chmodSync(other, 0o755);
  const r = resolve(pkg, {
    path: [userbin],
    home: join(root, 'home'),
    extra: { DONSETCH_SKIP_DOWNLOAD: '1' },
  });
  assert.equal(r.code, 0, r.stderr);
  assert.match(r.stderr, /using existing binary: .*\(version 9\.9\.9; this package pins \d+\.\d+\.\d+\)/);
});

test('--print-file keeps stdout empty and puts the path in the file', (t) => {
  if (!h.IS_POSIX) return t.skip('posix-only fixture');
  const root = h.scratch('printfile');
  const pkg = h.stagePkg(join(root, 'pkg'), { binaries: { donsetch: '#!/bin/sh\necho LOCAL\n' } });
  const outFile = join(root, 'out.txt');
  mkdirSync(join(root, 'home'), { recursive: true });
  const env = h.cleanEnv({ path: [], home: join(root, 'home') });
  const r = h.runNode(
    [join(pkg, 'resolve-binary.js'), '--print-file', outFile, '--pkg-dir', pkg],
    { env }
  );
  assert.equal(r.code, 0, r.stderr);
  assert.equal(r.stdout, '', 'stdout must stay free for an inheriting caller');
  assert.equal(readFileSync(outFile, 'utf8').trim(), join(pkg, 'binaries', 'donsetch'));
});

test('a stale stamped package binary upgrades to package.json version, while a newer self-updated binary stays untouched', async (t) => {
  if (!h.IS_POSIX) return t.skip('posix-only fixture');
  const root = h.scratch('stamp-upgrade');
  const pkg = h.stagePkg(join(root, 'pkg'), {
    binaries: {
      donsetch: '#!/bin/sh\necho OLD-STAMPED\n',
      'donsetch.version': '0.0.1\n',
    },
  });
  const m = await h.firstRunMirror(root);
  if (!m) return t.skip('unsupported test platform');
  t.after(() => m.server.close());

  const r1 = await h.spawnNode(
    [join(pkg, 'resolve-binary.js'), '--print', '--pkg-dir', pkg],
    { env: m.env }
  );
  assert.equal(r1.code, 0, r1.stderr);
  assert.equal(readFileSync(join(pkg, 'binaries', 'donsetch'), 'utf8'), h.TARBALL_BINARY);

  // Now simulate `donsetch update` advancing the local binary ahead of package.json:
  // resolve-binary.js must keep the newer binary offline without downgrading it.
  writeFileSync(join(pkg, 'binaries', 'donsetch'), '#!/bin/sh\necho NEWER-SELF-UPDATED\n');
  chmodSync(join(pkg, 'binaries', 'donsetch'), 0o755);
  writeFileSync(join(pkg, 'binaries', 'donsetch.version'), '99.0.0\n');
  const hitsBefore = m.server.hits.length;
  const r2 = await h.spawnNode(
    [join(pkg, 'resolve-binary.js'), '--print', '--pkg-dir', pkg],
    { env: m.env }
  );
  assert.equal(r2.code, 0, r2.stderr);
  assert.equal(m.server.hits.length, hitsBefore, 'newer self-updated binary must not re-download');
  assert.equal(readFileSync(join(pkg, 'binaries', 'donsetch'), 'utf8'), '#!/bin/sh\necho NEWER-SELF-UPDATED\n');
});
