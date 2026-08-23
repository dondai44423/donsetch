/**
 * DonSeTch pi extension — bridges the donsetch MCP binary into pi.
 *
 * `pi install npm:donsetch` installs this package. At session_start the
 * extension spawns `donsetch mcp`, performs the MCP handshake, discovers
 * tools via tools/list, and registers each one natively with
 * pi.registerTool(). Tool calls are proxied to the binary over stdio.
 *
 * Zero maintenance: tool definitions are fetched dynamically from the
 * binary. When donsetch adds or changes tools, this extension picks
 * them up automatically — no code changes needed here.
 *
 * Auto-download: if the binary is missing (e.g. postinstall was
 * blocked by npm 10+), the extension runs install.js at session_start
 * to fetch it from GitHub Releases.
 *
 * Custom TUI: each tool has clean renderCall/renderResult showing
 * a compact summary card — not the full raw output. The LLM still
 * receives complete content; the user sees a minimal status line +
 * one-line preview. Amber theme matching DonSeTch's identity.
 */
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import { spawn, execFileSync, type ChildProcess } from "node:child_process";
import { existsSync } from "node:fs";
import { join } from "node:path";
import { Text } from "@earendil-works/pi-tui";

// ── Constants ──
const INIT_TIMEOUT_MS = 10_000;
const CALL_TIMEOUT_MS = 120_000;
const SHUTDOWN_GRACE_MS = 2_000;

// ── TUI rendering note ──
// Pi's TUI wraps tool calls in its own green (success) / red (failure)
// highlight. We output PLAIN TEXT only — no ANSI codes at all.
// pi-tui's truncateToWidth injects \x1b[0m RESET around the ellipsis,
// which breaks pi's overlay mid-line. We use our own truncate()
// that adds zero ANSI codes.

/** Plain text truncation — no ANSI codes. */
function truncate(text: string, maxLen: number): string {
  if (text.length <= maxLen) return text;
  if (maxLen <= 3) return text.slice(0, maxLen);
  return text.slice(0, maxLen - 3) + "...";
}

// ── Tool icons ──
const ICONS: Record<string, string> = {
  web_fetch:  "\u{1F310}",  // 🌐
  web_search: "\u{1F50E}",  // 🔎
  web_crawl:  "\u{1F577}\u{FE0F}",  // 🕷️
};

// ── MCP client state ──
let proc: ChildProcess | null = null;
let nextId = 1;
const pending = new Map<
  number,
  { resolve: (v: any) => void; reject: (e: any) => void; timer: ReturnType<typeof setTimeout> }
>();
let initialized = false;
const toolNames: string[] = [];

// ── Binary resolution ──

function getBinaryPath(): string {
  const pkgDir = __dirname;
  const binaryName = process.platform === "win32" ? "donsetch.exe" : "donsetch";
  return join(pkgDir, "binaries", binaryName);
}

function ensureBinary(): string {
  const binaryPath = getBinaryPath();
  if (existsSync(binaryPath)) return binaryPath;

  const installScript = join(__dirname, "install.js");
  if (!existsSync(installScript)) {
    throw new Error(
      `donsetch binary not found at ${binaryPath} and install.js is missing. ` +
      `Run \`npm rebuild donsetch\` or \`npm install -g --allow-scripts=donsetch donsetch@latest\`.`
    );
  }

  try {
    execFileSync("node", [installScript], {
      stdio: "inherit",
      cwd: __dirname,
      timeout: 60_000,
    });
  } catch (err: any) {
    throw new Error(`Failed to download donsetch binary: ${err.message}`);
  }

  if (!existsSync(binaryPath)) {
    throw new Error(
      `donsetch binary still missing after install.js ran. ` +
      `Run \`npm install -g --allow-scripts=donsetch donsetch@latest\` manually.`
    );
  }

  return binaryPath;
}

// ── MCP JSON-RPC 2.0 over stdio ──

function startServer(): Promise<void> {
  if (proc && initialized) return Promise.resolve();
  if (proc && !initialized) return Promise.reject(new Error("donsetch MCP server is still initializing"));

  return new Promise((resolve, reject) => {
    let binaryPath: string;
    try {
      binaryPath = ensureBinary();
    } catch (err: any) {
      reject(err);
      return;
    }

    try {
      // --supervised: crash-only daemon. If the MCP server is SIGKILLed
      // (OOM, crash), the supervisor respawns it and replays in-flight
      // requests — pi users never see a dead tool until the process
      // itself dies.
      proc = spawn(binaryPath, ["mcp", "--supervised"], {
        stdio: ["pipe", "pipe", "pipe"],
        env: { ...process.env },
        windowsHide: true,
      });
    } catch (err: any) {
      reject(new Error(`Failed to spawn donsetch MCP server: ${err.message}`));
      return;
    }

    let buffer = "";

    proc.stdout?.on("data", (chunk: Buffer) => {
      buffer += chunk.toString();
      const lines = buffer.split("\n");
      buffer = lines.pop() || "";
      for (const line of lines) {
        if (!line.trim()) continue;
        try {
          const msg = JSON.parse(line);
          if (msg.id != null && pending.has(msg.id)) {
            const entry = pending.get(msg.id)!;
            pending.delete(msg.id);
            clearTimeout(entry.timer);
            if (msg.error) {
              entry.reject(new Error(msg.error.message || "MCP error"));
            } else {
              entry.resolve(msg.result);
            }
          }
        } catch {
          /* ignore non-JSON lines on stdout */
        }
      }
    });

    proc.stderr?.on("data", (chunk: Buffer) => {
      process.stderr.write(chunk);
    });

    proc.on("error", (err) => {
      proc = null;
      initialized = false;
      for (const [, e] of pending) {
        clearTimeout(e.timer);
        e.reject(err);
      }
      pending.clear();
    });

    proc.on("exit", (code) => {
      proc = null;
      initialized = false;
      for (const [, e] of pending) {
        clearTimeout(e.timer);
        e.reject(new Error(`donsetch MCP server exited (code ${code})`));
      }
      pending.clear();
    });

    sendRequest(
      "initialize",
      {
        protocolVersion: "2024-11-05",
        capabilities: {},
        clientInfo: { name: "pi-donsetch", version: "1.0.0" },
      },
      INIT_TIMEOUT_MS
    )
      .then(() => {
        sendNotification("notifications/initialized", {});
        initialized = true;
        resolve();
      })
      .catch(reject);
  });
}

function sendRequest(
  method: string,
  params: any,
  timeoutMs = CALL_TIMEOUT_MS,
  signal?: AbortSignal
): Promise<any> {
  return new Promise((resolve, reject) => {
    if (!proc?.stdin?.writable) {
      reject(new Error("donsetch MCP server not running"));
      return;
    }
    const id = nextId++;
    let settled = false;
    const finish = (fn: (v: any) => void, v: any) => {
      if (settled) return;
      settled = true;
      signal?.removeEventListener("abort", onAbort);
      fn(v);
    };
    const timer = setTimeout(() => {
      if (pending.delete(id)) {
        finish(reject, new Error(`MCP request timeout (${timeoutMs}ms): ${method}`));
      }
    }, timeoutMs);
    // v3 real MCP cancellation: forward pi's abort to the server so the
    // in-flight fetch/crawl actually stops server-side, then settle
    // locally. The caller maps this to a graceful "Cancelled" result.
    const onAbort = () => {
      if (pending.delete(id)) {
        clearTimeout(timer);
        sendNotification("notifications/cancelled", { id, reason: "client aborted" });
        finish(reject, new Error("cancelled"));
      }
    };
    if (signal) {
      if (signal.aborted) {
        onAbort();
        return;
      }
      signal.addEventListener("abort", onAbort, { once: true });
    }
    pending.set(id, {
      resolve: (v: any) => {
        clearTimeout(timer);
        finish(resolve, v);
      },
      reject: (e: any) => {
        clearTimeout(timer);
        finish(reject, e);
      },
      timer,
    });
    const msg = JSON.stringify({ jsonrpc: "2.0", id, method, params });
    proc.stdin.write(msg + "\n");
  });
}

function sendNotification(method: string, params: any): void {
  if (!proc?.stdin?.writable) return;
  const msg = JSON.stringify({ jsonrpc: "2.0", method, params });
  proc.stdin.write(msg + "\n");
}

async function callMcpTool(name: string, args: any, signal?: AbortSignal): Promise<any> {
  return sendRequest("tools/call", { name, arguments: args ?? {} }, CALL_TIMEOUT_MS, signal);
}

function killServer(): void {
  if (proc) {
    try {
      proc.stdin?.end();
      proc.kill("SIGTERM");
      const p = proc;
      setTimeout(() => {
        try { p.kill("SIGKILL"); } catch {}
      }, SHUTDOWN_GRACE_MS);
    } catch {}
    proc = null;
  }
  initialized = false;
  toolNames.length = 0;
  for (const [, e] of pending) {
    clearTimeout(e.timer);
    e.reject(new Error("donsetch MCP server killed"));
  }
  pending.clear();
}

function isAlive(): boolean {
  return proc !== null && !proc.killed && proc.stdin?.writable === true;
}

// ── TUI helpers ──

/** Extract a clean preview line from markdown content. */
function getPreview(text: string, maxLen = 72): string {
  const lines = text.split("\n");
  for (const line of lines) {
    let clean = line.replace(/^#+\s*/, "").replace(/\*\*([^*]+)\*\*/g, "$1").trim();
    if (clean.length > 0 && !clean.startsWith("{") && !clean.startsWith("[")) {
      return truncate(clean, maxLen);
    }
  }
  return "";
}

/** Count numbered search results in text. */
function countSearchResults(text: string): number {
  const matches = text.match(/^\d+\.\s/gm);
  return matches ? matches.length : 0;
}

/** Extract first search result title. */
function getFirstResultTitle(text: string): string {
  const match = text.match(/^\d+\.\s+\*\*(.+?)\*\*/m);
  return match ? match[1] : "";
}

/** Count pages from crawl output (## headings or numbered pages). */
function countCrawlPages(text: string): number {
  const matches = text.match(/^##\s/gm);
  return matches ? matches.length : 0;
}

/** Extract domain from URL for display. */
function shortUrl(url: string): string {
  try {
    const u = new URL(url);
    return u.hostname + (u.pathname !== "/" ? u.pathname.slice(0, 30) : "");
  } catch {
    return truncate(url, 50);
  }
}

/**
 * Extract the search provider from structuredContent.
 * Returns "local" when provider is null/undefined (local keyless
 * engine), or the provider name ("exa", "tavily", "serper",
 * "tinyfish") for BYOK.
 */
function getSearchProvider(sc: any): string {
  if (!sc) return "local";
  const provider = sc.provider;
  if (!provider || provider === "null") return "local";
  return String(provider);
}

/**
 * Extract the fetch source label from structuredContent.
 * Returns "cache" when tier is "1(warm)" (warm cookies, not a
 * fresh fetch), "ghost" when tier starts with "2" (browser
 * escalation), or "" for a normal tier-1 fetch.
 */
function getFetchSource(sc: any): string {
  if (!sc) return "";
  const tier = String(sc.tier || "");
  if (tier.includes("warm")) return "cache";
  if (tier.startsWith("2")) return "ghost";
  return "";
}

/**
 * Extract the fetch verdict/content status from structuredContent.
 * Returns "blocked" when content_ok is false or verdict is not
 * ContentOk, "" otherwise.
 */
function getFetchStatus(sc: any): string {
  if (!sc) return "";
  if (sc.content_ok === false) return "blocked";
  if (sc.thin === true) return "thin";
  return "";
}

// ── Extension ──

export default function (pi: ExtensionAPI) {
  pi.on("session_start", async () => {
    try {
      await startServer();
    } catch (err: any) {
      process.stderr.write(`[donsetch] failed to start MCP server: ${err.message}\n`);
      return;
    }

    let toolsResult: any;
    try {
      toolsResult = await sendRequest("tools/list", {});
    } catch (err: any) {
      process.stderr.write(`[donsetch] failed to list tools: ${err.message}\n`);
      return;
    }

    const mcpTools: any[] = toolsResult?.tools ?? [];
    if (mcpTools.length === 0) {
      process.stderr.write("[donsetch] no tools discovered from MCP server\n");
      return;
    }

    for (const mcpTool of mcpTools) {
      const name = mcpTool.name;
      if (!name) continue;
      toolNames.push(name);

      const description = mcpTool.description || mcpTool.name;
      const inputSchema = mcpTool.inputSchema || { type: "object", properties: {} };
      const toolName = name;
      const icon = ICONS[toolName] ?? "\u25C6";

      pi.registerTool({
        name: toolName,
        label: toolName,
        description,
        parameters: Type.Unsafe(inputSchema) as any,
        async execute(_toolCallId, params, _signal) {
          if (!isAlive()) {
            try {
              await startServer();
            } catch (err: any) {
              return {
                content: [{ type: "text", text: `donsetch MCP server crashed and could not restart: ${err.message}` }],
                isError: true,
              };
            }
          }

          try {
            const result = await callMcpTool(toolName, params, _signal);
            // Join all content text blocks, skipping [meta] blocks.
            // [meta] blocks contain compact metadata for clients
            // (Claude Code, VSCode) that drop text when
            // structuredContent is present. Pi reads structuredContent
            // directly for details, so meta blocks are redundant here.
            const text = (result?.content ?? [])
              .map((b: any) => b?.text ?? "")
              .filter((t: string) => !t.startsWith("[meta]"))
              .join("") || "";
            const isErr = result?.isError ?? false;
            const sc = result?.structuredContent ?? null;

            // Build details for TUI rendering
            const details: any = {
              mcpTool: toolName,
              isError: isErr,
              chars: text.length,
            };

            if (toolName === "web_search") {
              details.results = countSearchResults(text);
              details.topResult = getFirstResultTitle(text);
              details.provider = getSearchProvider(sc);
            } else if (toolName === "web_fetch") {
              details.source = getFetchSource(sc);
              details.status = getFetchStatus(sc);
              if (sc?.stitched) details.stitched = sc.stitched;
            } else if (toolName === "web_crawl") {
              details.pages = countCrawlPages(text);
            }

            // For errors, extract error text + v3 stable error code
            if (isErr) {
              details.error = getPreview(text, 60);
              if (sc?.code) details.code = sc.code;
            } else {
              details.preview = getPreview(text);
            }

            return {
              content: result?.content ?? [{ type: "text", text: "No output" }],
              details,
              isError: isErr,
            };
          } catch (err: any) {
            // User pressed Esc in pi: we already told the server to stop
            // (notifications/cancelled). Graceful non-error result, per
            // pi's extension contract for aborted calls.
            if (err.message === "cancelled") {
              return {
                content: [{ type: "text", text: "Cancelled" }],
                details: { mcpTool: toolName, cancelled: true },
              };
            }
            return {
              content: [{ type: "text", text: `donsetch MCP call failed: ${err.message}` }],
              details: { mcpTool: toolName, isError: true, error: err.message },
              isError: true,
            };
          }
        },

        renderCall(args: any, _theme: any) {
          let key = "";
          if (args?.url) {
            key = shortUrl(args.url);
          } else if (args?.query) {
            key = truncate(`"${args.query}"`, 50);
          }
          // Plain text only — no ANSI codes. Pi wraps tool calls
          // in its own green (success) / red (failure) highlight.
          // Any ANSI we emit breaks pi's overlay mid-line.
          return new Text(
            `${icon} ${toolName}  ${key}`,
            0, 0
          );
        },

        renderResult(result: any, opts: any, _theme: any) {
          if (opts?.isPartial) {
            return new Text(`${toolName} working…`, 0, 0);
          }

          const isErr = result?.isError || result?.details?.isError;
          const d = result?.details ?? {};

          // Build metadata string per tool
          let meta = "";
          if (toolName === "web_fetch") {
            const parts: string[] = [];
            parts.push(`${(d.chars ?? 0).toLocaleString()} chars`);
            if (d.source === "cache") parts.push("via cache");
            else if (d.source === "ghost") parts.push("via ghost");
            if (d.status === "blocked") parts.push("blocked");
            else if (d.status === "thin") parts.push("thin");
            if (d.stitched) parts.push(`stitched \u00D7${d.stitched}`);
            meta = parts.join(" \u00B7 ");
          } else if (toolName === "web_search") {
            const count = d.results ?? 0;
            const provider = d.provider ?? "local";
            meta = `${count} result${count !== 1 ? "s" : ""} \u00B7 via ${provider}`;
          } else if (toolName === "web_crawl") {
            const pages = d.pages ?? 0;
            meta = `${pages} page${pages !== 1 ? "s" : ""}`;
          }

          // Build line 2: preview or error
          let line2 = "";
          if (d.cancelled) {
            line2 = "cancelled";
          } else if (isErr) {
            line2 = d.code ? `[${d.code}] ${d.error || "failed"}` : d.error || "failed";
          } else if (toolName === "web_search" && d.topResult) {
            line2 = truncate(d.topResult, 70);
          } else if (d.preview) {
            line2 = d.preview;
          }

          // Plain text only — no ANSI. Pi handles all coloring.
          const line1 = `${toolName} \u00B7 ${meta}`;
          const output = line2
            ? `${line1}\n  ${line2}`
            : line1;

          return new Text(output, 0, 0);
        },
      });
    }

    process.stderr.write(`[donsetch] ${mcpTools.length} tools registered: ${toolNames.join(", ")}\n`);
  });

  pi.on("session_shutdown", () => {
    killServer();
  });
}
