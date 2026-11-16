'use strict';

// The first run under a host whose `process.execPath` is not a JS
// interpreter — issue #450: Pi-Bolt is a Bun-compiled pi, so spawning
// `process.execPath install.js --bin-dir DIR` runs the host itself and
// the installer's arguments are rejected as its own. The resolver must
// install in-process, whatever the host binary is.
//
// Two proofs:
//   1. plain Node with process.execPath overridden to a host that
//      rejects script arguments (runs everywhere, no dependencies);
//   2. a real standalone Bun executable that dynamically imports the
//      resolver, the way a Pi host loads an extension (skips when bun
//      is not installed; the CI runner has no bun).

const { test } = require('node:test');
const assert = require('node:assert');
const { chmodSync, existsSync, mkdirSync, readFileSync, writeFileSync } = require('fs');
const { join } = require('path');
const h = require('./helpers.js');

test('a first run never spawns process.execPath as a script interpreter', async (t) => {
  if (!h.IS_POSIX) return t.skip('posix-only fixture');
  const root = h.scratch('compiled-host');
  const pkg = h.stagePkg(join(root, 'pkg'));
  const m = await h.firstRunMirror(root);
  if (!m) return t.skip('unsupported test platform');
  t.after(() => m.server.close());

  // The compiled-host condition, in plain Node: process.execPath names
  // an executable that runs its own program and rejects a script path.
  // The old resolver ran it as `host install.js --bin-dir DIR` and the
  // install failed with "Unknown option".
  const host = join(root, 'compiled-host');
  writeFileSync(host, '#!/bin/sh\necho "Error: Unknown option: ${1#--}" >&2\nexit 1\n');
  chmodSync(host, 0o755);
  const preload = join(root, 'fake-execpath.js');
  writeFileSync(
    preload,
    `Object.defineProperty(process, 'execPath', { value: ${JSON.stringify(host)} });\n`
  );

  const r = await h.spawnNode(
    ['--require', preload, join(pkg, 'resolve-binary.js'), '--print', '--pkg-dir', pkg],
    { env: m.env }
  );
  assert.equal(r.code, 0, r.stderr);
  assert.equal(r.stdout.trim(), join(pkg, 'binaries', 'donsetch'));
  assert.equal(readFileSync(join(pkg, 'binaries', 'donsetch'), 'utf8'), h.TARBALL_BINARY);
  assert.doesNotMatch(r.stderr, /Unknown option/, 'the host was used as an interpreter');
});

test('a compiled Bun host installs the pinned binary in-process', async (t) => {
  if (!h.IS_POSIX) return t.skip('posix-only fixture');
  if (!h.bunPath()) return t.skip('bun not installed: the compiled-host fixture needs it');
  const root = h.scratch('pi-bolt');
  const pkg = h.stagePkg(join(root, 'pkg'));
  const m = await h.firstRunMirror(root);
  if (!m) return t.skip('unsupported test platform');
  t.after(() => m.server.close());

  // A Pi-like host: a standalone executable that loads the package's
  // resolver from disk at runtime and awaits it.
  const host = h.buildBunHost(
    root,
    `const pkg = process.env.DONSETCH_PKG!;
const { resolveBinary } = await import(\`\${pkg}/resolve-binary.js\`);
const r = await resolveBinary({
  pkgDir: pkg,
  onOutput: (chunk: string) => process.stderr.write(chunk),
});
console.log(JSON.stringify({ execPath: process.execPath, path: r.path, source: r.source, notice: r.notice ?? null }));
`
  );

  const json = await runHost(host, { ...m.env, DONSETCH_PKG: pkg });
  const parsed = JSON.parse(json.stdout.trim());
  assert.equal(parsed.path, join(pkg, 'binaries', 'donsetch'), json.stderr);
  assert.equal(parsed.source, 'package');
  assert.equal(parsed.execPath, host, 'the host must be the compiled executable');
  assert.equal(readFileSync(join(pkg, 'binaries', 'donsetch'), 'utf8'), h.TARBALL_BINARY);
  const version = JSON.parse(readFileSync(join(pkg, 'package.json'), 'utf8')).version;
  assert.equal(
    readFileSync(join(pkg, 'binaries', 'donsetch.version'), 'utf8').trim(),
    version,
    'the version stamp is written'
  );

  // The old call shape, against a fresh package: the compiled host runs
  // its own program and nothing is installed. This is the bug the fix
  // removes, kept as evidence that the fixture reproduces it.
  const fresh = h.stagePkg(join(root, 'fresh'), {});
  const target = join(root, 'old-shape-binaries');
  const old = await runHost(host, { PATH: process.env.PATH, HOME: join(root, 'home') }, [
    join(fresh, 'install.js'),
    '--bin-dir',
    target,
  ]);
  assert.notEqual(old.code, 0, 'the compiled host cannot run install.js as a script');
  assert.ok(!existsSync(join(target, 'donsetch')), 'the old call shape must not install');
});

// Run the compiled host as a subprocess and collect its output.
function runHost(host, env, args = []) {
  const { spawn } = require('child_process');
  return new Promise((resolve) => {
    const child = spawn(host, args, { env, stdio: ['ignore', 'pipe', 'pipe'] });
    let stdout = '';
    let stderr = '';
    const timer = setTimeout(() => child.kill('SIGKILL'), 120_000);
    child.stdout.on('data', (d) => (stdout += d));
    child.stderr.on('data', (d) => (stderr += d));
    child.on('close', (code) => {
      clearTimeout(timer);
      resolve({ code, stdout, stderr });
    });
  });
}

test('installer progress stays off stdout: the wrapper owns an MCP channel', async (t) => {
  if (!h.IS_POSIX) return t.skip('posix-only fixture');
  const root = h.scratch('stdout-purity');
  const pkg = h.stagePkg(join(root, 'pkg'));
  const m = await h.firstRunMirror(root);
  if (!m) return t.skip('unsupported test platform');
  t.after(() => m.server.close());
  mkdirSync(join(root, 'home'), { recursive: true });

  const r = await h.spawnNode([join(pkg, 'bin', 'donsetch.js'), '--version'], { env: m.env });
  assert.equal(r.code, 0, r.stderr);
  assert.equal(r.stdout, 'TARBALL-BINARY --version\n', 'stdout must carry the child exactly');
  assert.match(r.stderr, /downloading/, 'progress belongs on stderr');
  assert.match(r.stderr, /installed/, 'progress belongs on stderr');
});
