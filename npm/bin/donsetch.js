#!/usr/bin/env node
'use strict';

// donsetch CLI wrapper: spawn the native binary with forwarded stdio.
//
// For MCP server usage (`donsetch mcp`), the MCP client spawns this
// wrapper as a subprocess and communicates via stdin/stdout (JSON-RPC).
// stdio: 'inherit' pipes the native binary's stdio directly to the
// parent process, so the MCP protocol passes through unmodified.
//
// Signal forwarding: when the MCP client sends SIGTERM/SIGINT to this
// wrapper, we forward it to the native binary so it can clean up
// (close connections, save ghost state, etc.) before exiting.

const { spawn, execFileSync } = require('child_process');
const { existsSync } = require('fs');
const { join } = require('path');

// ── resolve binary path ─────────────────────────────────────────
const binaryName = process.platform === 'win32' ? 'donsetch.exe' : 'donsetch';
const binDir = join(__dirname, '..', 'binaries');
const binaryPath = join(binDir, binaryName);

if (!existsSync(binaryPath)) {
  const installScript = join(__dirname, '..', 'install.js');
  try {
    execFileSync(process.execPath, [installScript], {
      stdio: 'inherit',
      cwd: join(__dirname, '..'),
    });
  } catch (_) {
    process.stderr.write(
      `donsetch: native binary missing and download failed. Retry with network access, ` +
      `or run node ${installScript}; pnpm users: pnpm approve-builds then reinstall.\n`
    );
    process.exit(1);
  }
  if (!existsSync(binaryPath)) {
    process.stderr.write(
      `donsetch: native binary missing and download failed. Retry with network access, ` +
      `or run node ${installScript}; pnpm users: pnpm approve-builds then reinstall.\n`
    );
    process.exit(1);
  }
}

// ── spawn native binary ─────────────────────────────────────────
const child = spawn(binaryPath, process.argv.slice(2), {
  stdio: 'inherit',
  cwd: process.cwd(),
  env: process.env,
  windowsHide: true,
});

// ── forward signals to child ────────────────────────────────────
// When the parent receives SIGTERM/SIGINT (e.g. MCP client
// disconnecting), forward to the child so it can clean up.
let killed = false;

function forwardSignal(sig) {
  if (!killed) {
    killed = true;
    try { child.kill(sig); } catch (_) {}
    // Force exit after 5s if child doesn't respond
    setTimeout(() => process.exit(1), 5000).unref();
  }
}

process.on('SIGTERM', () => forwardSignal('SIGTERM'));
process.on('SIGINT', () => forwardSignal('SIGINT'));
process.on('SIGHUP', () => forwardSignal('SIGHUP'));

// ── exit with child's code ──────────────────────────────────────
child.on('exit', (code, signal) => {
  if (signal) {
    // Re-raise the signal so the parent's exit reflects it
    try { process.kill(process.pid, signal); } catch (_) {}
    // Fallback: exit code for the signal we actually received
    // (SIGINT=2, SIGTERM=15, SIGHUP=1), not always SIGTERM's 143.
    const sigNum = { SIGHUP: 1, SIGINT: 2, SIGQUIT: 3, SIGTERM: 15 }[signal] || 15;
    process.exit(128 + sigNum);
  } else {
    process.exit(code || 0);
  }
});

child.on('error', (err) => {
  process.stderr.write(`donsetch: failed to spawn binary: ${err.message}\n`);
  process.exit(1);
});
