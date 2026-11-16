'use strict';

// A development runner's system-wide VC runtime must not hide an incomplete
// release. The payload gate should reject it before attempting to run it.
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const { join, resolve } = require('node:path');
const { spawnSync } = require('node:child_process');
const h = require('./helpers.js');

test('Windows payload gate requires app-local CRT and accepts the complete payload', (t) => {
  if (!h.IS_POSIX) return t.skip('POSIX shell fixture; real Windows payload is gated in CI');
  const root = h.scratch('windows-payload');
  const repo = resolve(__dirname, '../..');
  const version = require('../package.json').version;
  const bin = join(root, 'donsetch.exe');
  fs.writeFileSync(bin, `#!/bin/sh\necho 'DonSeTch ${version} ONNX Runtime shared library present'\nexit 0\n`);
  fs.truncateSync(bin, 12_000_000);
  fs.chmodSync(bin, 0o755);
  fs.writeFileSync(join(root, 'onnxruntime.dll'), 'fixture');
  fs.truncateSync(join(root, 'onnxruntime.dll'), 10_000_000);
  fs.writeFileSync(join(root, 'pdfium.dll'), 'fixture');
  const gate = () => spawnSync('sh', [join(repo, 'scripts/gates.sh'), 'win32-x64', root], {
    encoding: 'utf8', env: { ...process.env, CI: '' },
  });
  let result = gate();
  assert.equal(result.status, 1, result.stdout + result.stderr);
  assert.match(result.stdout, /msvcp140.dll missing beside the binary/);
  const runtimes = ['msvcp140.dll', 'msvcp140_1.dll', 'vcruntime140.dll', 'vcruntime140_1.dll'];
  const updater = fs.readFileSync(join(repo, 'src/cli/update.rs'), 'utf8');
  const workflow = fs.readFileSync(join(repo, '.github/workflows/ci.yml'), 'utf8');
  const staging = fs.readFileSync(join(repo, 'scripts/windows-runtime.ps1'), 'utf8');
  for (const name of runtimes) {
    assert.ok(updater.includes(`"${name}"`), `${name} must be updated and rolled back`);
    assert.ok(workflow.includes(name), `${name} must be packaged`);
    assert.ok(staging.includes(name), `${name} must be staged`);
    fs.writeFileSync(join(root, name), 'fixture');
  }
  result = gate();
  assert.equal(result.status, 0, result.stdout + result.stderr);
  assert.match(result.stdout, /gates ok: win32-x64/);
});
