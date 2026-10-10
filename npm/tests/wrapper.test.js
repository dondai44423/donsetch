'use strict';

// bin/donsetch.js end to end: resolution, spawn, exit codes, and the
// stdout-purity contract (an MCP stdio session owns stdout; notices
// and installer output belong on stderr).

const { test } = require('node:test');
const assert = require('node:assert');
const { mkdirSync } = require('fs');
const { join } = require('path');
const h = require('./helpers.js');

function scaffold(tag) {
  const root = h.scratch(tag);
  const pkg = h.stagePkg(join(root, 'pkg'));
  const home = join(root, 'home');
  const empty = join(root, 'empty');
  mkdirSync(home, { recursive: true });
  mkdirSync(empty, { recursive: true });
  return { root, pkg, home, empty };
}

test('falls back to a PATH binary: exit 0, child output only on stdout', (t) => {
  if (!h.IS_POSIX) return t.skip('posix-only fixture');
  const { root, pkg, home } = scaffold('fallback');
  const userbin = join(root, 'userbin');
  h.stubBinary(userbin, 'STUB-OK');
  const env = h.cleanEnv({
    path: [userbin],
    home,
    extra: { DONSETCH_SKIP_DOWNLOAD: '1' },
  });
  const r = h.runNode([join(pkg, 'bin', 'donsetch.js'), '--version', '--json'], { env });
  assert.equal(r.code, 0, r.stderr);
  assert.equal(r.stdout, 'STUB-OK --version --json\n', 'stdout must carry the child exactly');
  assert.match(r.stderr, /using existing binary/);
});

test('spawns the package-local binary when present', (t) => {
  if (!h.IS_POSIX) return t.skip('posix-only fixture');
  const { root, pkg, home } = scaffold('local');
  h.stubBinary(join(root, 'userbin'), 'OTHER');
  const { writeFileSync, chmodSync, mkdirSync } = require('fs');
  mkdirSync(join(pkg, 'binaries'), { recursive: true });
  writeFileSync(join(pkg, 'binaries', 'donsetch'), '#!/bin/sh\necho LOCAL "$@"\n');
  chmodSync(join(pkg, 'binaries', 'donsetch'), 0o755);
  const env = h.cleanEnv({ path: [join(root, 'userbin')], home });
  const r = h.runNode([join(pkg, 'bin', 'donsetch.js'), 'mcp'], { env });
  assert.equal(r.code, 0, r.stderr);
  assert.equal(r.stdout, 'LOCAL mcp\n');
  assert.doesNotMatch(r.stderr, /using existing binary/);
});

test("the child's exit code passes through", (t) => {
  if (!h.IS_POSIX) return t.skip('posix-only fixture');
  const { root, pkg, home } = scaffold('exitcode');
  const userbin = join(root, 'userbin');
  h.stubBinary(userbin, 'FAILING', { exit: 42 });
  const env = h.cleanEnv({
    path: [userbin],
    home,
    extra: { DONSETCH_SKIP_DOWNLOAD: '1' },
  });
  const r = h.runNode([join(pkg, 'bin', 'donsetch.js'), 'fetch'], { env });
  assert.equal(r.code, 42);
  assert.equal(r.stdout, 'FAILING fetch\n');
});

test('nothing anywhere: exit 1 with the curated text, no stack trace', (t) => {
  if (!h.IS_POSIX) return t.skip('posix-only fixture');
  const { pkg, home, empty } = scaffold('none');
  const env = h.cleanEnv({
    path: [empty],
    home,
    extra: { DONSETCH_SKIP_DOWNLOAD: '1' },
  });
  const r = h.runNode([join(pkg, 'bin', 'donsetch.js'), 'doctor'], { env });
  assert.equal(r.code, 1);
  assert.match(r.stderr, /could not install the donsetch binary and none was found/);
  assert.doesNotMatch(r.stderr, /at Object\.|node:internal/, 'no raw Node stack trace');
});

test('an invalid DONSETCH_BINARY stops the wrapper with the reason', (t) => {
  if (!h.IS_POSIX) return t.skip('posix-only fixture');
  const { root, pkg, home } = scaffold('env-bad');
  const env = h.cleanEnv({
    path: [join(root, 'empty')],
    home,
    extra: { DONSETCH_BINARY: join(root, 'nope') },
  });
  const r = h.runNode([join(pkg, 'bin', 'donsetch.js'), 'doctor'], { env });
  assert.equal(r.code, 1);
  assert.match(r.stderr, /DONSETCH_BINARY is set to/);
});
