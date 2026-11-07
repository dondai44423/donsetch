'use strict';

// install.js, and the wrapper's self-healing install, through an HTTP
// CONNECT proxy named by HTTPS_PROXY: the downloads ride the tunnel,
// the credentials of a user:pass@ proxy URL reach the proxy, and the
// process exits as soon as the install is done.

const { test } = require('node:test');
const assert = require('node:assert');
const http = require('http');
const net = require('net');
const { existsSync, mkdirSync, readFileSync, symlinkSync } = require('fs');
const { join } = require('path');
const h = require('./helpers.js');

const TAG = 'v0.0.0-test';
const BIN_BODY = '#!/bin/sh\necho TARBALL-BINARY "$@"\n';
// Reserved TLD (RFC 6761): the release host has no address, so only
// the proxy can reach it, and its certificate is valid for that name
// alone, so the tunnelled TLS must verify against the release host.
const RELEASE_HOST = 'releases.donsetch.test';
// The password holds ':' and '@', which the proxy URL carries
// percent-encoded: the proxy accepts only the decoded pair.
const USER = 'alice';
const PASS = 's3:cr@t';

function basic(user, pass) {
  return `Basic ${Buffer.from(`${user}:${pass}`).toString('base64')}`;
}

function userinfo(user, pass) {
  return `${encodeURIComponent(user)}:${encodeURIComponent(pass)}`;
}

// An HTTP CONNECT proxy that records every tunnel: the
// Proxy-Authorization it arrived with, the bytes it carried each way,
// and the local port of its leg to the mirror. Every target name is
// dialled on 127.0.0.1. The client leg stays open after the mirror
// hangs up, so only the client closing its own sockets ends it.
function startConnectProxy({ want = null } = {}) {
  const tunnels = [];
  const sockets = new Set();
  const track = (s) => {
    sockets.add(s);
    s.on('close', () => sockets.delete(s));
  };
  const server = http.createServer((req, res) => {
    res.writeHead(405);
    res.end();
  });
  server.on('connect', (req, client, head) => {
    track(client);
    const tunnel = {
      target: req.url,
      auth: req.headers['proxy-authorization'] || null,
      up: 0,
      down: 0,
      upstreamPort: null,
    };
    tunnels.push(tunnel);
    if (want && tunnel.auth !== want) {
      client.end(
        'HTTP/1.1 407 Proxy Authentication Required\r\n'
        + 'Proxy-Authenticate: Basic realm="test"\r\nContent-Length: 0\r\n\r\n'
      );
      return;
    }
    const port = Number(req.url.slice(req.url.lastIndexOf(':') + 1));
    const upstream = net.connect(port, '127.0.0.1', () => {
      tunnel.upstreamPort = upstream.localPort;
      client.write('HTTP/1.1 200 Connection Established\r\n\r\n');
      if (head && head.length) {
        tunnel.up += head.length;
        upstream.write(head);
      }
      client.on('data', (d) => {
        tunnel.up += d.length;
        upstream.write(d);
      });
      upstream.on('data', (d) => {
        tunnel.down += d.length;
        if (!client.destroyed) client.write(d);
      });
      client.on('end', () => upstream.end());
    });
    track(upstream);
    upstream.on('error', () => client.destroy());
    client.on('error', () => upstream.destroy());
  });
  return new Promise((resolve) => {
    server.listen(0, '127.0.0.1', () => resolve({
      port: server.address().port,
      tunnels,
      close: () => new Promise((r) => {
        for (const s of sockets) s.destroy();
        server.closeAllConnections();
        server.close(r);
      }),
    }));
  });
}

// One install against a fresh package copy, a release mirror at
// `host`, and a CONNECT proxy in HTTPS_PROXY.
async function scenario(tag, {
  host = RELEASE_HOST,
  want = null,
  credentials = '',
  trustMirror = true,
  noProxy = null,
  wrapper = false,
} = {}) {
  const asset = h.assetName();
  const root = h.scratch(tag);
  const pkg = h.stagePkg(join(root, 'pkg'));
  const home = join(root, 'home');
  mkdirSync(home, { recursive: true });
  const tls = h.makeTlsFixture(root, { host });
  const tarball = h.makeTarball(root, { donsetch: BIN_BODY });
  const mirror = await h.startReleaseServer(tls, {
    tag: TAG,
    asset,
    tarballPath: tarball,
    sha: h.sha256File(tarball),
  });
  const proxy = await startConnectProxy({ want });
  const tools = join(root, 'tools');
  mkdirSync(tools, { recursive: true });
  for (const tool of ['tar', 'gzip']) symlinkSync(h.toolPath(tool), join(tools, tool));
  const extra = {
    DONSETCH_RELEASES_BASE: `https://${host}:${mirror.port}`,
    DONSETCH_INSTALL_TAG: TAG,
    HTTPS_PROXY: `http://${credentials ? `${credentials}@` : ''}127.0.0.1:${proxy.port}`,
  };
  if (trustMirror) extra.NODE_EXTRA_CA_CERTS = tls.certPath;
  if (noProxy) extra.NO_PROXY = noProxy;
  const env = h.cleanEnv({ path: [tools], home, extra });
  const args = wrapper ? [join(pkg, 'bin', 'donsetch.js'), '--version'] : [join(pkg, 'install.js')];
  const started = Date.now();
  let r;
  try {
    r = await h.spawnNode(args, { env, timeout: 30_000 });
  } finally {
    await proxy.close();
    await mirror.close();
  }
  const seconds = (Date.now() - started) / 1000;
  const binary = join(pkg, 'binaries', 'donsetch');
  return {
    r,
    out: `exit ${r.code} after ${seconds.toFixed(1)}s\n${r.stdout}${r.stderr}`,
    seconds,
    proxy,
    mirror,
    installed: existsSync(binary) ? readFileSync(binary, 'utf8') : null,
  };
}

// Every connection the mirror accepted came out of a proxy tunnel,
// every tunnel carried bytes both ways, and SNI named the release host,
// or was absent for an IP address, which RFC 6066 section 3 does not
// allow in SNI (node 22 and 24 both send an IP servername as given).
function assertTunnelled(s, host) {
  const tunnels = JSON.stringify(s.proxy.tunnels);
  assert.equal(s.proxy.tunnels.length, 2, `one tunnel per download: ${tunnels}`);
  for (const t of s.proxy.tunnels) {
    assert.ok(t.up > 0 && t.down > 0, `a tunnel carried no bytes: ${tunnels}`);
  }
  const viaProxy = new Set(s.proxy.tunnels.map((t) => t.upstreamPort));
  assert.deepEqual(
    s.mirror.peers.filter((p) => !viaProxy.has(p)),
    [],
    `the mirror accepted a direct connection (tunnels ${tunnels}, mirror peers ${JSON.stringify(s.mirror.peers)})`
  );
  const sni = net.isIP(host) ? null : host;
  assert.deepEqual(s.mirror.sni, [sni, sni], 'SNI');
}

const skip = () => (!h.IS_POSIX || !h.assetName() ? 'posix fixture' : false);

// The mirror is reachable directly here too: the bytes must still go
// through the proxy.
test('an open CONNECT proxy carries both downloads, and the installer exits promptly', async (t) => {
  if (skip()) return t.skip(skip());
  const s = await scenario('proxy-open', { host: '127.0.0.1' });
  assert.equal(s.r.code, 0, s.out);
  assert.equal(s.installed, BIN_BODY, s.out);
  assertTunnelled(s, '127.0.0.1');
  // A direct install exits in well under a second here. A tunnel
  // socket left open holds the process until its peer closes it, which
  // this proxy never does: the spawn timeout (30 s) would end the run.
  assert.ok(s.seconds < 10, `the installer lingered: ${s.out}`);
});

test('credentials in HTTPS_PROXY reach an authenticating proxy', async (t) => {
  if (skip()) return t.skip(skip());
  const want = basic(USER, PASS);
  const s = await scenario('proxy-auth', { want, credentials: userinfo(USER, PASS) });
  assert.deepEqual(s.proxy.tunnels.map((x) => x.auth), [want, want], s.out);
  assert.equal(s.r.code, 0, s.out);
  assert.equal(s.installed, BIN_BODY, s.out);
  assertTunnelled(s, RELEASE_HOST);
});

test("the wrapper's self-healing install goes through the authenticating proxy", async (t) => {
  if (skip()) return t.skip(skip());
  const want = basic(USER, PASS);
  const s = await scenario('proxy-wrapper', { want, credentials: userinfo(USER, PASS), wrapper: true });
  assert.equal(s.r.code, 0, s.out);
  assert.equal(s.r.stdout, 'TARBALL-BINARY --version\n', s.out);
  assert.deepEqual(s.proxy.tunnels.map((x) => x.auth), [want, want], s.out);
  assertTunnelled(s, RELEASE_HOST);
});

// ── Negatives: behaviour that must not change ───────────────────

test('a wrong proxy password still fails with 407, and no output carries it', async (t) => {
  if (skip()) return t.skip(skip());
  const wrong = 'wr0ng:p@ss';
  const s = await scenario('proxy-407', { want: basic(USER, PASS), credentials: userinfo(USER, wrong) });
  assert.equal(s.r.code, 1, s.out);
  assert.match(s.r.stderr, /install failed: proxy CONNECT returned HTTP 407/, s.out);
  assert.equal(s.installed, null, s.out);
  for (const secret of [wrong, encodeURIComponent(wrong), Buffer.from(`${USER}:${wrong}`).toString('base64')]) {
    assert.ok(!s.out.includes(secret), `the output carries the password (${secret}): ${s.out}`);
  }
});

test('certificate verification stays on: an untrusted mirror certificate fails the install', async (t) => {
  if (skip()) return t.skip(skip());
  const s = await scenario('proxy-untrusted', { host: '127.0.0.1', trustMirror: false });
  assert.equal(s.r.code, 1, s.out);
  assert.match(s.r.stderr, /self[- ]signed certificate/, s.out);
  assert.equal(s.installed, null, s.out);
  assert.equal(s.mirror.hits.length, 0, 'no request may reach an unverified mirror');
});

test('NO_PROXY naming the release host keeps the download direct', async (t) => {
  if (skip()) return t.skip(skip());
  const s = await scenario('proxy-bypass', { host: '127.0.0.1', noProxy: '127.0.0.1' });
  assert.equal(s.r.code, 0, s.out);
  assert.equal(s.installed, BIN_BODY, s.out);
  assert.deepEqual(s.proxy.tunnels, [], 'the proxy must see nothing');
  assert.equal(s.mirror.hits.length, 2, s.out);
});
