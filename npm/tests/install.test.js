'use strict';

// install.js: the unwritable-target contract (#361), atomic landing,
// and the ordinary download/verify/extract path against a local HTTPS
// release mirror.

const { test } = require('node:test');
const assert = require('node:assert');
const { chmodSync, existsSync, mkdirSync, readFileSync, readdirSync, statSync, symlinkSync, writeFileSync } = require('fs');
const { join } = require('path');
const h = require('./helpers.js');

const TAG = 'v0.0.0-test';
const BIN_BODY = '#!/bin/sh\necho TARBALL-BINARY "$@"\n';
const LIB_BODY = 'fake-onnx-runtime-bytes';

function scaffold(tag) {
  const root = h.scratch(tag);
  const pkg = h.stagePkg(join(root, 'pkg'));
  const home = join(root, 'home');
  const empty = join(root, 'empty');
  mkdirSync(home, { recursive: true });
  mkdirSync(empty, { recursive: true });
  return { root, pkg, home, empty };
}

async function mirror(root, pkg, extra) {
  const asset = h.assetName();
  if (!asset) return null;
  const tls = h.makeTlsFixture(root);
  const tarball = h.makeTarball(root, { donsetch: BIN_BODY, 'libonnxruntime.so': LIB_BODY });
  const server = await h.startReleaseServer(tls, {
    tag: TAG,
    asset,
    tarballPath: tarball,
    sha: h.sha256File(tarball),
  });
  // The child PATH holds exactly the tools the installer shells out to
  // (tar and its gzip) and nothing that could resolve as a donsetch.
  const tools = join(root, 'tools');
  mkdirSync(tools, { recursive: true });
  for (const tool of ['tar', 'gzip']) {
    symlinkSync(h.toolPath(tool), join(tools, tool));
  }
  const env = h.cleanEnv({
    path: [tools],
    home: join(root, 'home'),
    extra: {
      DONSETCH_RELEASES_BASE: `https://127.0.0.1:${server.port}`,
      DONSETCH_INSTALL_TAG: TAG,
      NODE_EXTRA_CA_CERTS: tls.certPath,
      ...extra,
    },
  });
  return { asset, server, env };
}

test('unwritable target + existing binary: acknowledged, exit 0, no download', (t) => {
  if (!h.IS_POSIX) return t.skip('posix-only fixture');
  if (h.SKIP_IF_ROOT) return t.skip(h.SKIP_IF_ROOT);
  const { root, pkg, home } = scaffold('ack');
  const userbin = join(root, 'userbin');
  h.stubBinary(userbin, 'USERBIN');
  chmodSync(pkg, 0o555);
  // A non-HTTPS base: any download attempt would surface its refusal,
  // so its absence proves the unwritable path never touches the network.
  const env = h.cleanEnv({
    path: [userbin],
    home,
    extra: { DONSETCH_RELEASES_BASE: 'http://127.0.0.1:1/x' },
  });
  const r = h.runNode([join(pkg, 'install.js')], { env });
  assert.equal(r.code, 0, r.stderr);
  assert.match(r.stdout, /an existing binary was found and will be used/);
  assert.doesNotMatch(r.stdout + r.stderr, /refusing non-HTTPS/, 'a download was attempted');
  assert.ok(!existsSync(join(pkg, 'binaries')), 'nothing may be written into the read-only package');
});

test('unwritable target + no fallback: exit 1 naming every way out', (t) => {
  if (!h.IS_POSIX) return t.skip('posix-only fixture');
  if (h.SKIP_IF_ROOT) return t.skip(h.SKIP_IF_ROOT);
  const { pkg, home, empty } = scaffold('noack');
  chmodSync(pkg, 0o555);
  const env = h.cleanEnv({
    path: [empty],
    home,
    extra: { DONSETCH_RELEASES_BASE: 'http://127.0.0.1:1/x' },
  });
  const r = h.runNode([join(pkg, 'install.js')], { env });
  assert.equal(r.code, 1);
  assert.match(r.stderr, /cannot install into/);
  assert.match(r.stderr, /no existing donsetch binary was found/);
  assert.match(r.stderr, /DONSETCH_BINARY=\/path\/to\/donsetch/);
  assert.match(r.stderr, /npm config set prefix/);
  assert.doesNotMatch(r.stdout + r.stderr, /refusing non-HTTPS/, 'a download was attempted');
});

test('writable target: downloads, verifies, lands, stamps; rerun is offline', async (t) => {
  if (!h.IS_POSIX) return t.skip('posix-only fixture');
  const { root, pkg } = scaffold('happy');
  const m = await mirror(root, pkg);
  if (!m) return t.skip('unsupported test platform');
  t.after(() => m.server.close());

  // Async spawn, not spawnSync: the fixture server lives in THIS
  // process, and a blocked event loop would never answer the child.
  const r = await h.spawnNode([join(pkg, 'install.js')], { env: m.env });
  assert.equal(r.code, 0, r.stderr);

  const bin = join(pkg, 'binaries', 'donsetch');
  assert.ok(existsSync(bin), 'binary landed');
  assert.equal(readFileSync(bin, 'utf8'), BIN_BODY, 'binary bytes complete');
  assert.ok(statSync(bin).mode & 0o111, 'binary executable');
  assert.ok(existsSync(join(pkg, 'binaries', 'libonnxruntime.so')), 'sibling lib landed');
  const version = JSON.parse(readFileSync(join(pkg, 'package.json'), 'utf8')).version;
  assert.equal(
    readFileSync(join(pkg, 'binaries', 'donsetch.version'), 'utf8').trim(),
    version,
    'version stamp written'
  );
  assert.deepEqual(
    readdirSync(join(pkg, 'binaries')).filter((n) => n.includes('.tmp-')),
    [],
    'temp artifacts must not survive'
  );

  // Second run: the stamp short-circuits before any request.
  const hits = m.server.hits.length;
  const r2 = await h.spawnNode([join(pkg, 'install.js')], { env: m.env });
  assert.equal(r2.code, 0, r2.stderr);
  assert.match(r2.stdout, /already present/);
  assert.equal(m.server.hits.length, hits, 'already-installed run must not touch the network');

  // A stamp mismatch means a fresh release under the same package
  // version: re-download.
  writeFileSync(join(pkg, 'binaries', 'donsetch.version'), 'older\n');
  const r3 = await h.spawnNode([join(pkg, 'install.js')], { env: m.env });
  assert.equal(r3.code, 0, r3.stderr);
  assert.match(r3.stdout, /re-downloading/);
  assert.ok(m.server.hits.length > hits, 'mismatch re-fetches');
});

test('concurrent first runs all land one complete binary', async (t) => {
  if (!h.IS_POSIX) return t.skip('posix-only fixture');
  const { root, pkg } = scaffold('race');
  const m = await mirror(root, pkg);
  if (!m) return t.skip('unsupported test platform');
  t.after(() => m.server.close());

  const runs = await Promise.all(
    [0, 1, 2].map(() => h.spawnNode([join(pkg, 'install.js')], { env: m.env }))
  );
  for (const r of runs) assert.equal(r.code, 0, r.stderr);

  const bin = join(pkg, 'binaries', 'donsetch');
  assert.equal(readFileSync(bin, 'utf8'), BIN_BODY, 'a racing writer tore the binary');
  assert.ok(statSync(bin).mode & 0o111);
  assert.deepEqual(
    readdirSync(join(pkg, 'binaries')).filter((n) => n.includes('.tmp-')),
    [],
    'temp artifacts must not survive the race'
  );
});

test('DONSETCH_SKIP_DOWNLOAD=1 exits 0 without installing', (t) => {
  if (!h.IS_POSIX) return t.skip('posix-only fixture');
  const { pkg, home, empty } = scaffold('skip');
  const env = h.cleanEnv({
    path: [empty],
    home,
    extra: { DONSETCH_SKIP_DOWNLOAD: '1' },
  });
  const r = h.runNode([join(pkg, 'install.js')], { env });
  assert.equal(r.code, 0, r.stderr);
  assert.match(r.stdout, /download skipped/);
  assert.ok(!existsSync(join(pkg, 'binaries', 'donsetch')));
});
