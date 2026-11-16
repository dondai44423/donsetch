'use strict';

// Exercise the shipped extension's session_start with a real subprocess,
// without adding Pi's runtime to the package's dependency tree. Node 22.13+
// strips TS; only the two host-provided UI/schema imports are stubbed.
const { test } = require('node:test');
const assert = require('node:assert/strict');
const { stripTypeScriptTypes } = require('node:module');
const fs = require('node:fs');
const { join } = require('node:path');
const h = require('./helpers.js');

function mcpBinary(dir, name) {
  const file = join(dir, 'donsetch');
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(file, `#!${process.execPath}
const readline = require('node:readline');
readline.createInterface({input:process.stdin}).on('line',line=>{
  const q=JSON.parse(line);
  if(q.id==null)return;
  const result=q.method==='tools/list'?{tools:[{name:${JSON.stringify(name)},inputSchema:{type:'object',properties:{}}}]}:{};
  console.log(JSON.stringify({jsonrpc:'2.0',id:q.id,result}));
});
`);
  fs.chmodSync(file, 0o755);
  return file;
}

function extension(pkg) {
  let source = fs.readFileSync(join(__dirname, '..', 'pi-extension.ts'), 'utf8');
  source = source.replace(/^import \{ Type \} from "typebox";$/m, 'const Type = { Unsafe: x => x };');
  source = source.replace(/^import \{ Text \} from "@earendil-works\/pi-tui";$/m, 'class Text {}');
  source = source.replace('export default function (pi: ExtensionAPI)', 'globalThis.__factory = function (pi: ExtensionAPI)');
  source = stripTypeScriptTypes(source);
  const file = join(pkg, 'extension.mjs');
  fs.writeFileSync(file, `const __dirname = ${JSON.stringify(pkg)};\n${source}\n
const handlers = new Map(); const tools = [];
const api = { on:(name,fn)=>handlers.set(name,fn), registerTool:t=>tools.push(t.name) };
await globalThis.__factory(api);
await handlers.get('session_start')();
console.log(JSON.stringify({tools}));
handlers.get('session_shutdown')();
`);
  return file;
}

for (const scenario of ['override', 'permissions', 'local', 'invalid-override']) {
  test(`Pi session_start: ${scenario}`, async (t) => {
    if (!h.IS_POSIX || !stripTypeScriptTypes) return t.skip('Node 22.13+ and POSIX fixture');
    const root = h.scratch(`pi-${scenario}`);
    const pkg = h.stagePkg(join(root, 'pkg'));
    const local = mcpBinary(join(pkg, 'binaries'), 'local');
    const over = mcpBinary(join(root, 'override'), 'override');
    if (scenario === 'permissions') fs.chmodSync(local, 0o644);
    const env = h.cleanEnv({ home: join(root, 'home'), extra: { DONSETCH_SKIP_DOWNLOAD: '1' } });
    if (scenario === 'override') env.DONSETCH_BINARY = over;
    if (scenario === 'invalid-override') env.DONSETCH_BINARY = join(root, 'missing');
    const result = await h.spawnNode([extension(pkg)], { env });
    assert.equal(result.code, 0, result.stderr);
    const tools = JSON.parse(result.stdout.trim()).tools;
    if (scenario === 'invalid-override') {
      assert.deepEqual(tools, []);
      assert.match(result.stderr, /DONSETCH_BINARY.*not an executable file/);
    } else {
      assert.deepEqual(tools, [scenario === 'override' ? 'override' : 'local'], result.stderr);
      assert.doesNotMatch(result.stderr, /failed to start/);
    }
  });
}
