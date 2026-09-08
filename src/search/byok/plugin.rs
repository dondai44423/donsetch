//! BYOK search plugins : user-registered executables that answer
//! queries over a stdin/stdout JSON contract.
//!
//! A plugin is registered by the user via
//! `donsetch keys add plugin <name> --cmd 'program [args...]'`
//! and then behaves like any other BYOK provider in the default /
//! fallback chain: DonSeTch spawns it, feeds it the query, and
//! parses the results. The contract (format=1) is:
//!
//! Request (stdin, one JSON document, then EOF):
//!   {"format":1,"query":"...","max_results":8,"intent":"web","deadline_ms":30000}
//!
//! Response (stdout, one JSON document):
//!   {"format":1,"results":[{"title":"...","url":"https://...",
//!                           "snippet":"...","score":0.9}],"degraded":false}
//!
//! Errors: non-zero exit (stderr is the message) or the envelope
//! {"format":1,"error":"...","retryable":true} with any exit code.
//!
//! Runtime discipline: direct exec (never a shell), hard stdout/
//! stderr caps, per-plugin timeout with SIGKILL, kill-on-drop so
//! MCP cancellation can never orphan a child. Full contract and
//! rationale: design/byok-plugins.md.

use std::collections::HashSet;
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::{Intent, KeyError, ProviderResult, SearchHit};
use crate::search::byok::store::PROVIDERS;

/// The contract version we emit and accept. Bump (and add a
/// parser arm) when the envelope shape changes; old adapters
/// keep working because the version travels with every message.
pub const FORMAT_VERSION: u32 = 1;

const MAX_STDOUT_BYTES: u64 = 8 * 1024 * 1024; // 8 MiB
const MAX_STDERR_BYTES: u64 = 64 * 1024;
const MAX_SNIPPET_CHARS: usize = 8 * 1024;
const MAX_RESULTS: usize = 50;
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
pub const MIN_TIMEOUT_MS: u64 = 1_000;
pub const MAX_TIMEOUT_MS: u64 = 300_000;

/// Keyless engine ids and reserved words: a plugin must not be
/// able to masquerade as one of these (attribution honesty).
const RESERVED_NAMES: &[&str] = &[
    "google", "bing", "ddg", "ddg_lite", "ddg_html", "mojeek", "yahoo", "brave", "local",
];

// ── config ────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginDef {
    /// argv, tokenized once at registration (never re-split).
    pub cmd: Vec<String>,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
}

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_MS
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PluginConfig {
    #[serde(default = "default_version")]
    pub version: u32,
    pub plugins: std::collections::BTreeMap<String, PluginDef>,
    /// Registration order (BTreeMap sorts names; fallback
    /// priority follows registration, not the alphabet).
    #[serde(default)]
    pub order: Vec<String>,
}

fn default_version() -> u32 {
    FORMAT_VERSION
}

impl PluginConfig {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Load from disk. Missing or corrupt file degrades to an
    /// empty config with a warning (mirrors byok-keys.json).
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::empty();
        };
        match std::fs::read_to_string(&path) {
            Ok(raw) => match serde_json::from_str(&raw) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("[byok] warning: corrupt plugin file ({e}), ignoring");
                    Self::empty()
                }
            },
            Err(_) => Self::empty(),
        }
    }

    /// Save to disk, 0600, atomic tmp+rename.
    pub fn save(&self) {
        let Some(path) = config_path() else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match serde_json::to_string_pretty(self) {
            Ok(json) => {
                let tmp = path.with_extension("tmp");
                let write_ok = {
                    #[cfg(unix)]
                    {
                        use std::io::Write;
                        use std::os::unix::fs::OpenOptionsExt;
                        std::fs::OpenOptions::new()
                            .write(true)
                            .create(true)
                            .truncate(true)
                            .mode(0o600)
                            .open(&tmp)
                            .and_then(|mut f| f.write_all(json.as_bytes()))
                            .is_ok()
                    }
                    #[cfg(not(unix))]
                    {
                        std::fs::write(&tmp, json).is_ok()
                    }
                };
                if write_ok {
                    let _ = std::fs::rename(&tmp, &path);
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let _ =
                            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
                    }
                }
            }
            Err(e) => eprintln!("[byok] warning: failed to save plugins ({e})"),
        }
    }

    pub fn is_configured(&self) -> bool {
        !self.plugins.is_empty()
    }

    pub fn is_registered(&self, name: &str) -> bool {
        self.plugins.contains_key(name)
    }

    /// Registration-order names.
    pub fn names(&self) -> impl Iterator<Item = &String> {
        self.order.iter().filter(|n| self.plugins.contains_key(*n))
    }

    /// Add or replace a plugin. Returns Err with a reason when
    /// the name is invalid or collides with a native surface.
    pub fn add(
        &mut self,
        name: &str,
        cmd: Vec<String>,
        timeout_ms: u64,
        keyed_providers: &HashSet<String>,
    ) -> Result<(), String> {
        validate_plugin_name(name, keyed_providers)?;
        self.plugins.insert(
            name.to_string(),
            PluginDef {
                cmd,
                timeout_ms: timeout_ms.clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS),
            },
        );
        if !self.order.iter().any(|n| n == name) {
            self.order.push(name.to_string());
        }
        Ok(())
    }

    /// Remove a plugin. Returns true if one was removed.
    pub fn remove(&mut self, name: &str) -> bool {
        let removed = self.plugins.remove(name).is_some();
        self.order.retain(|n| n != name);
        removed
    }
}

/// Validate a plugin name: charset, length, and collisions with
/// native provider names, keyless engine ids and "local".
pub fn validate_plugin_name(name: &str, keyed_providers: &HashSet<String>) -> Result<(), String> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err("plugin name must not be empty".to_string());
    };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return Err(format!(
            "invalid plugin name {name:?}: must start with a lowercase letter or digit"
        ));
    }
    if name.len() > 32
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        return Err(format!(
            "invalid plugin name {name:?}: use a-z, 0-9, -, _ (max 32 chars)"
        ));
    }
    if PROVIDERS.contains(&name) {
        return Err(format!(
            "{name:?} is a native provider: add its key with `donsetch keys add {name} <key>`"
        ));
    }
    if RESERVED_NAMES.contains(&name) {
        return Err(format!(
            "{name:?} is a builtin search engine name: pick a provider-flavored name"
        ));
    }
    if keyed_providers.contains(name) {
        return Err(format!(
            "{name:?} is already configured as a native provider with keys"
        ));
    }
    Ok(())
}

/// Tokenize a command string the way a POSIX shell would split
/// it (whitespace outside quotes; single quotes literal; double
/// quotes with \" and \\ escapes). The result is stored as argv
/// and never re-interpreted, which also makes Windows paths with
/// spaces safe.
pub fn tokenize_cmd(input: &str) -> Result<Vec<String>, String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut chars = input.chars().peekable();
    let mut in_token = false;
    let mut error: Option<String> = None;

    while let Some(c) = chars.next() {
        if error.is_some() {
            break;
        }
        match c {
            ' ' | '\t' | '\n' | '\r' => {
                if in_token {
                    tokens.push(std::mem::take(&mut cur));
                    in_token = false;
                }
            }
            '\'' => {
                in_token = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c2) => cur.push(c2),
                        None => {
                            error = Some("unterminated single quote".to_string());
                            break;
                        }
                    }
                }
            }
            '"' => {
                in_token = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some('"') => cur.push('"'),
                            Some('\\') => cur.push('\\'),
                            Some(e) => {
                                // Unknown escape: keep the backslash
                                // and the char (sh-like leniency).
                                cur.push('\\');
                                cur.push(e);
                            }
                            None => cur.push('\\'),
                        },
                        Some(c2) => cur.push(c2),
                        None => {
                            error = Some("unterminated double quote".to_string());
                            break;
                        }
                    }
                }
            }
            _ => {
                in_token = true;
                if c == '\0' {
                    error = Some("command must not contain NUL bytes".to_string());
                } else {
                    cur.push(c);
                }
            }
        }
    }
    if let Some(e) = error {
        return Err(e);
    }
    if in_token {
        tokens.push(cur);
    }
    if tokens.is_empty() {
        return Err("command must not be empty".to_string());
    }
    Ok(tokens)
}

fn config_path() -> Option<std::path::PathBuf> {
    Some(crate::paths::cache_dir().join("plugins.json"))
}

// ── request / response ─────────────────────────────────────

fn intent_str(intent: &Intent) -> &'static str {
    match intent {
        Intent::Web => "web",
        Intent::Code => "code",
        Intent::Paper => "paper",
        Intent::News => "news",
        Intent::Entity => "entity",
    }
}

fn build_request(query: &str, max: usize, intent: &Intent, timeout_ms: u64) -> String {
    serde_json::json!({
        "format": FORMAT_VERSION,
        "query": query,
        "max_results": max.clamp(1, MAX_RESULTS),
        "intent": intent_str(intent),
        "deadline_ms": timeout_ms,
    })
    .to_string()
}

/// Parse + validate a stdout envelope into hits. Invalid entries
/// are dropped (bad title/url), other problems are errors naming
/// the exact cause. Returns (hits, degraded, dropped_count).
///
/// No per-query cap here (#164): the parse-side bound is
/// MAX_RESULTS; the final cap to the requested max happens in
/// `to_merged` AFTER URL dedup. Truncating before dedup let
/// duplicate-heavy plugin output silently deliver fewer unique
/// results than the agent asked for.
fn parse_envelope(
    bytes: &[u8],
    plugin_name: &str,
) -> Result<(Vec<SearchHit>, bool, usize), String> {
    let v: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|e| format!("plugin {plugin_name}: stdout is not valid JSON: {e}"))?;
    let obj = v
        .as_object()
        .ok_or_else(|| format!("plugin {plugin_name}: stdout is not a JSON object"))?;

    if let Some(fmt) = obj.get("format").and_then(|f| f.as_u64())
        && fmt != FORMAT_VERSION as u64
    {
        return Err(format!(
            "plugin {plugin_name}: unsupported format {fmt} (expected {FORMAT_VERSION})"
        ));
    }

    if let Some(err) = obj.get("error") {
        let msg = err
            .as_str()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unspecified plugin error".to_string());
        // #164: cap like the sibling stderr trim (600 chars). An
        // envelope error must not ride the full 8 MiB stdout budget
        // onto the model surface.
        let msg: String = msg.chars().take(600).collect();
        return Err(format!("plugin {plugin_name}: {msg}"));
    }

    let results = obj
        .get("results")
        .and_then(|r| r.as_array())
        .ok_or_else(|| {
            format!(
                "plugin {plugin_name}: envelope has no \"results\" array (format {FORMAT_VERSION})"
            )
        })?;

    let degraded = obj
        .get("degraded")
        .and_then(|d| d.as_bool())
        .unwrap_or(false);

    let mut hits: Vec<SearchHit> = Vec::new();
    let mut dropped = 0usize;
    for item in results.iter().take(MAX_RESULTS) {
        let Some(entry) = item.as_object() else {
            dropped += 1;
            continue;
        };
        let title = entry
            .get("title")
            .and_then(|t| t.as_str())
            .map(str::trim)
            .unwrap_or("");
        let url = match entry.get("url").and_then(|u| u.as_str()) {
            Some(u) if is_http_url(u) => u.to_string(),
            Some(u) => {
                dropped += 1;
                if std::env::var_os("DONSEEK_DEBUG").is_some() {
                    eprintln!("[plugin] {plugin_name}: dropped result with bad url: {u:?}");
                }
                continue;
            }
            None => {
                dropped += 1;
                continue;
            }
        };
        if title.is_empty() {
            dropped += 1;
            continue;
        }
        let snippet = entry
            .get("snippet")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .chars()
            .take(MAX_SNIPPET_CHARS)
            .collect();
        let score = entry
            .get("score")
            .and_then(|s| s.as_f64())
            .map(|s| s.clamp(0.0, 1.0) as f32)
            .unwrap_or(1.0);
        hits.push(SearchHit {
            title: title.to_string(),
            url,
            snippet,
            score,
        });
    }
    if results.len() > MAX_RESULTS && std::env::var_os("DONSEEK_DEBUG").is_some() {
        eprintln!(
            "[plugin] {plugin_name}: truncated {} results to {MAX_RESULTS}",
            results.len()
        );
    }
    if !results.is_empty() && hits.is_empty() {
        return Err(format!(
            "plugin {plugin_name}: all {} results failed validation (need non-empty title and an http(s) url)",
            results.len()
        ));
    }
    Ok((hits, degraded, dropped))
}

fn is_http_url(u: &str) -> bool {
    match url::Url::parse(u) {
        Ok(p) => matches!(p.scheme(), "http" | "https"),
        Err(_) => false,
    }
}

// ── execution ──────────────────────────────────────────────

/// Run one plugin query: spawn, feed stdin, collect stdout with
/// caps, enforce the timeout with a hard kill. On MCP
/// cancellation the child is dropped (kill_on_drop) so no orphan
/// can outlive the request.
pub(crate) async fn run_plugin(
    name: &str,
    def: &PluginDef,
    query: &str,
    max: usize,
    intent: &Intent,
) -> ProviderResult {
    let started = Instant::now();
    let request = build_request(query, max, intent, def.timeout_ms);

    let mut child = match spawn_plugin(name, def) {
        Ok(c) => c,
        Err(e) => return Err(KeyError::UnknownError(e)),
    };
    let stderr_pipe = child.stderr.take();
    // Drain stderr from the moment of spawn: a full pipe must
    // never deadlock the adapter while it writes stdout.
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(s) = stderr_pipe {
            use tokio::io::AsyncReadExt;
            let _ = s.take(MAX_STDERR_BYTES + 1).read_to_end(&mut buf).await;
        }
        String::from_utf8_lossy(&buf).into_owned()
    });

    let body_all = async {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // Write the request, then close stdin: the EOF is the
        // end-of-request signal.
        if let Some(mut si) = child.stdin.take() {
            let _ = si.write_all(request.as_bytes()).await;
        }
        // Read stdout with the hard cap.
        let mut body = Vec::new();
        if let Some(so) = child.stdout.take() {
            let _ = so.take(MAX_STDOUT_BYTES + 1).read_to_end(&mut body).await;
        }
        let status = child.wait().await;
        (body, status)
    };

    let timed = tokio::time::timeout(Duration::from_millis(def.timeout_ms), body_all).await;

    let (body, status, stderr) = match timed {
        Ok((body, status)) => {
            let stderr = stderr_task.await.unwrap_or_default();
            (body, status, stderr)
        }
        Err(_) => {
            // Dropping the child (kill_on_drop) SIGKILLs it.
            let ms = started.elapsed().as_millis();
            return Err(KeyError::UnknownError(format!(
                "plugin {name}: timed out after {ms}ms (process killed)"
            )));
        }
    };

    if body.len() as u64 > MAX_STDOUT_BYTES {
        return Err(KeyError::UnknownError(format!(
            "plugin {name}: stdout exceeded the {MAX_STDOUT_BYTES}-byte cap (process killed)"
        )));
    }

    let stderr_trimmed: String = stderr
        .chars()
        .take(600)
        .collect::<String>()
        .trim()
        .to_string();

    match status {
        Ok(code) if code.success() => match parse_envelope(&body, name) {
            Ok((hits, degraded, dropped)) => {
                if dropped > 0 && std::env::var_os("DONSEEK_DEBUG").is_some() {
                    eprintln!("[plugin] {name}: dropped {dropped} invalid result entries");
                }
                let ms = started.elapsed().as_millis() as u64;
                Ok(super::ProviderOutcome { hits, ms, degraded })
            }
            Err(e) => Err(KeyError::UnknownError(e)),
        },
        Ok(code) => {
            // Non-zero exit: prefer the error envelope if stdout
            // happens to be one, else stderr, else the raw code.
            if let Ok((messages, _)) = extract_error_envelope(&body) {
                return Err(KeyError::UnknownError(messages));
            }
            let msg = if !stderr_trimmed.is_empty() {
                stderr_trimmed
            } else {
                format!("exited with status {code}")
            };
            Err(KeyError::UnknownError(format!("plugin {name}: {msg}")))
        }
        Err(e) => Err(KeyError::UnknownError(format!(
            "plugin {name}: failed to collect exit status: {e}"
        ))),
    }
}

/// Best-effort pull of an error envelope from stdout bytes.
fn extract_error_envelope(bytes: &[u8]) -> Result<(String, bool), ()> {
    let v: serde_json::Value = serde_json::from_slice(bytes).map_err(|_| ())?;
    let obj = v.as_object().ok_or(())?;
    let Some(err) = obj.get("error").and_then(|e| e.as_str()) else {
        return Err(());
    };
    let retryable = obj
        .get("retryable")
        .and_then(|r| r.as_bool())
        .unwrap_or(false);
    if err.trim().is_empty() {
        return Err(());
    }
    // #164: cap like the sibling stderr trim (600 chars): an error
    // envelope must not ride the full 8 MiB stdout budget onto the
    // model surface.
    let msg: String = err.trim().chars().take(600).collect();
    Ok((msg, retryable))
}

fn spawn_plugin(name: &str, def: &PluginDef) -> Result<tokio::process::Child, String> {
    let program = def.cmd.first().map(String::as_str).unwrap_or("");
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(&def.cmd[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("DONSETCH_PLUGIN", "1")
        .env("DONSETCH_PLUGIN_NAME", name)
        .current_dir(std::env::temp_dir())
        .kill_on_drop(true);
    cmd.spawn().map_err(|e| {
        let hint = if e.kind() == std::io::ErrorKind::NotFound {
            " (program not found: check the registered command)"
        } else {
            ""
        };
        format!("plugin {name}: failed to start `{program}`: {e}{hint}")
    })
}

/// Thread-safe wrapper for runtime use.
pub struct PluginStore {
    config: std::sync::Mutex<PluginConfig>,
}

impl Default for PluginStore {
    fn default() -> Self {
        Self::new()
    }
}

impl PluginStore {
    pub fn new() -> Self {
        Self {
            config: std::sync::Mutex::new(PluginConfig::load()),
        }
    }

    pub fn is_configured(&self) -> bool {
        self.config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_configured()
    }

    pub fn reload(&self) {
        let new_cfg = PluginConfig::load();
        *self
            .config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = new_cfg;
    }

    /// Snapshot of the plugin definitions (cheap: BTreeMap of
    /// clones; registration is a rare operation).
    pub fn snapshot(&self) -> PluginConfig {
        self.config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

// ── probe (keys add --test) ────────────────────────────────

/// One real query against the adapter, used by
/// `donsetch keys add plugin --test`. Explicit user consent:
/// never invoked automatically.
pub async fn probe(name: &str, def: &PluginDef) -> Result<usize, String> {
    match run_plugin(name, def, "DonSeTch plugin probe", 3, &Intent::Web).await {
        Ok(outcome) => Ok(outcome.hits.len()),
        Err(e) => Err(e.to_string()),
    }
}

// ── tests ──────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn keyed() -> HashSet<String> {
        HashSet::new()
    }

    #[test]
    fn tokenizer_simple() {
        let t = tokenize_cmd("python3 /x/a.py --flag v").unwrap();
        assert_eq!(t, vec!["python3", "/x/a.py", "--flag", "v"]);
    }

    #[test]
    fn tokenizer_single_quotes_literal() {
        let t = tokenize_cmd("prog '/path with space/a.py' arg2").unwrap();
        assert_eq!(t, vec!["prog", "/path with space/a.py", "arg2"]);
    }

    #[test]
    fn tokenizer_double_quotes_escapes() {
        let t = tokenize_cmd(r#"prog "a\"b" 'c d' "e\\f""#).unwrap();
        assert_eq!(t, vec!["prog", "a\"b", "c d", "e\\f"]);
    }

    #[test]
    fn tokenizer_double_inside_single() {
        let t = tokenize_cmd(r#"prog 'say "hi"'"#).unwrap();
        assert_eq!(t, vec!["prog", "say \"hi\""]);
    }

    #[test]
    fn tokenizer_empty_quotes_make_token() {
        let t = tokenize_cmd("prog \"\" x").unwrap();
        assert_eq!(t, vec!["prog", "", "x"]);
    }

    #[test]
    fn tokenizer_unterminated_single() {
        assert!(tokenize_cmd("prog 'abc").is_err());
    }

    #[test]
    fn tokenizer_unterminated_double() {
        assert!(tokenize_cmd("prog \"abc").is_err());
    }

    #[test]
    fn tokenizer_empty_input() {
        assert!(tokenize_cmd("").is_err());
        assert!(tokenize_cmd("   ").is_err());
    }

    #[test]
    fn tokenizer_multiline_whitespace() {
        let t = tokenize_cmd("a\n\tb").unwrap();
        assert_eq!(t, vec!["a", "b"]);
    }

    #[test]
    fn name_validation_charset() {
        assert!(validate_plugin_name("searxng", &keyed()).is_ok());
        assert!(validate_plugin_name("searx-ng_2", &keyed()).is_ok());
        assert!(validate_plugin_name("SearX", &keyed()).is_err());
        assert!(validate_plugin_name("-x", &keyed()).is_err());
        assert!(validate_plugin_name("x/y", &keyed()).is_err());
        assert!(validate_plugin_name("", &keyed()).is_err());
        let long = "a".repeat(33);
        assert!(validate_plugin_name(&long, &keyed()).is_err());
    }

    #[test]
    fn name_validation_collisions() {
        assert!(validate_plugin_name("tavily", &keyed()).is_err());
        assert!(validate_plugin_name("google", &keyed()).is_err());
        assert!(validate_plugin_name("ddg", &keyed()).is_err());
        assert!(validate_plugin_name("local", &keyed()).is_err());
        let mut k = keyed();
        k.insert("mine".to_string());
        assert!(validate_plugin_name("mine", &k).is_err());
    }

    #[test]
    fn config_add_remove_round_trip() {
        let mut cfg = PluginConfig::empty();
        cfg.add(
            "searxng",
            vec!["python3".into(), "/x.py".into()],
            20_000,
            &keyed(),
        )
        .unwrap();
        cfg.add(
            "wiki2",
            vec!["sh".into(), "-c".into(), "probe".into()],
            5_000,
            &keyed(),
        )
        .unwrap();
        assert!(cfg.is_configured());
        assert!(cfg.is_registered("searxng"));
        assert_eq!(cfg.names().count(), 2);
        // Registration order kept.
        assert_eq!(cfg.names().collect::<Vec<_>>(), vec!["searxng", "wiki2"]);
        // Replace updates the definition and keeps order.
        cfg.add("searxng", vec!["python3".into()], 45_000, &keyed())
            .unwrap();
        assert_eq!(cfg.plugins["searxng"].timeout_ms, 45_000);
        assert_eq!(cfg.names().count(), 2);
        assert!(cfg.remove("searxng"));
        assert!(!cfg.remove("searxng"));
        assert_eq!(cfg.names().count(), 1);
    }

    #[test]
    fn config_timeout_clamped() {
        let mut cfg = PluginConfig::empty();
        cfg.add("x", vec!["p".into()], 10, &keyed()).unwrap();
        assert_eq!(cfg.plugins["x"].timeout_ms, MIN_TIMEOUT_MS);
        cfg.add("x", vec!["p".into()], 99_999_999, &keyed())
            .unwrap();
        assert_eq!(cfg.plugins["x"].timeout_ms, MAX_TIMEOUT_MS);
    }

    #[test]
    fn config_add_rejects_bad_name() {
        let mut cfg = PluginConfig::empty();
        assert!(cfg.add("Bad!", vec!["p".into()], 1000, &keyed()).is_err());
        assert!(cfg.add("tavily", vec!["p".into()], 1000, &keyed()).is_err());
        assert!(!cfg.is_configured());
    }

    #[test]
    fn request_envelope_shape() {
        let r = build_request("café 東京", 7, &Intent::Code, 12345);
        let v: serde_json::Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["format"], 1);
        assert_eq!(v["query"], "café 東京");
        assert_eq!(v["max_results"], 7);
        assert_eq!(v["intent"], "code");
        assert_eq!(v["deadline_ms"], 12345);
    }

    #[test]
    fn parse_envelope_valid() {
        let env = r#"{"format":1,"results":[
            {"title":"A","url":"https://a.com","snippet":"s","score":0.9},
            {"title":"B","url":"https://b.com"}
        ]}"#;
        let (hits, degraded, dropped) = parse_envelope(env.as_bytes(), "t").unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "A");
        assert!((hits[0].score - 0.9).abs() < 0.001);
        assert_eq!(hits[1].score, 1.0); // default
        assert!(!degraded);
        assert_eq!(dropped, 0);
    }

    #[test]
    fn parse_envelope_empty_results_ok() {
        let env = r#"{"format":1,"results":[]}"#;
        let (hits, degraded, _) = parse_envelope(env.as_bytes(), "t").unwrap();
        assert!(hits.is_empty());
        assert!(!degraded);
    }

    #[test]
    fn parse_envelope_drops_bad_entries() {
        let env = r#"{"format":1,"results":[
            {"title":"OK","url":"https://ok.com"},
            {"title":"","url":"https://x.com"},
            {"title":"JS","url":"javascript:alert(1)"},
            {"title":"Ftp","url":"ftp://x.com"},
            {"title":"NoUrl"},
            "not-an-object"
        ]}"#;
        let (hits, _, dropped) = parse_envelope(env.as_bytes(), "t").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "OK");
        assert_eq!(dropped, 5);
    }

    #[test]
    fn parse_envelope_all_dropped_is_error() {
        let env = r#"{"format":1,"results":[{"title":"","url":"https://x.com"}]}"#;
        let e = parse_envelope(env.as_bytes(), "t").unwrap_err();
        assert!(e.contains("failed validation"), "{e}");
    }

    #[test]
    fn parse_envelope_score_clamped_and_capped() {
        let env = r#"{"format":1,"results":[{"title":"A","url":"https://a.com","score":9.5}]}"#;
        let (hits, _, _) = parse_envelope(env.as_bytes(), "t").unwrap();
        assert_eq!(hits[0].score, 1.0);
    }

    #[test]
    fn parse_envelope_snippet_capped() {
        let big = "x".repeat(20_000);
        let env = format!(
            r#"{{"format":1,"results":[{{"title":"A","url":"https://a.com","snippet":"{big}"}}]}}"#
        );
        let (hits, _, _) = parse_envelope(env.as_bytes(), "t").unwrap();
        assert_eq!(hits[0].snippet.chars().count(), MAX_SNIPPET_CHARS);
    }

    #[test]
    fn parse_envelope_keeps_hits_past_max_for_downstream_dedup() {
        // #164: capping to max at parse time (BEFORE URL dedup in
        // to_merged) made duplicate-heavy plugin output deliver fewer
        // unique results than requested. Parse keeps every valid hit
        // (bounded only by MAX_RESULTS); the max cap is applied after
        // dedup in to_merged.
        let mut items = Vec::new();
        for i in 0..10 {
            items.push(format!(r#"{{"title":"T{i}","url":"https://t{i}.com"}}"#));
        }
        let env = format!(r#"{{"format":1,"results":[{}]}}"#, items.join(","));
        let (hits, _, _) = parse_envelope(env.as_bytes(), "t").unwrap();
        assert_eq!(hits.len(), 10, "parse must not pre-truncate to max");
    }

    #[test]
    fn parse_envelope_format_mismatch() {
        let env = r#"{"format":7,"results":[]}"#;
        let e = parse_envelope(env.as_bytes(), "t").unwrap_err();
        assert!(e.contains("unsupported format 7"), "{e}");
    }

    #[test]
    fn parse_envelope_missing_results() {
        let env = r#"{"format":1,"foo":1}"#;
        let e = parse_envelope(env.as_bytes(), "t").unwrap_err();
        assert!(e.contains("results"), "{e}");
    }

    #[test]
    fn parse_envelope_error_envelope() {
        let env = r#"{"format":1,"error":"rate limit hit","retryable":true}"#;
        let e = parse_envelope(env.as_bytes(), "t").unwrap_err();
        assert!(e.contains("rate limit hit"), "{e}");
    }

    #[test]
    fn parse_envelope_error_is_capped_before_model_surface() {
        // #164: an oversized error string must not reach the model
        // surface whole; it is trimmed to the same 600-char budget as
        // sibling stderr.
        let big = "x".repeat(5000);
        let env = format!(r#"{{"format":1,"error":"{big}"}}"#);
        let e = parse_envelope(env.as_bytes(), "t").unwrap_err();
        let payload = e.strip_prefix("plugin t: ").unwrap();
        assert_eq!(payload.chars().count(), 600, "error text must be capped");
    }

    #[test]
    fn parse_envelope_not_json() {
        let e = parse_envelope(b"<html>oops</html>", "t").unwrap_err();
        assert!(e.contains("not valid JSON"), "{e}");
    }

    #[test]
    fn parse_envelope_degraded_flag() {
        let env = r#"{"format":1,"results":[{"title":"A","url":"https://a.com"}],"degraded":true}"#;
        let (_, degraded, _) = parse_envelope(env.as_bytes(), "t").unwrap();
        assert!(degraded);
    }

    #[test]
    fn extract_error_envelope_works() {
        let (msg, retryable) =
            extract_error_envelope(br#"{"format":1,"error":"boom","retryable":true}"#).unwrap();
        assert_eq!(msg, "boom");
        assert!(retryable);
        assert!(extract_error_envelope(b"{}").is_err());
        assert!(extract_error_envelope(b"<html>").is_err());
    }

    #[test]
    fn extract_error_envelope_caps_long_message() {
        // #164: mirror the stderr trim; a huge error envelope must not
        // surface unbounded.
        let big = "y".repeat(8000);
        let env = format!(r#"{{"error":"{big}"}}"#);
        let (msg, retryable) = extract_error_envelope(env.as_bytes()).unwrap();
        assert_eq!(msg.chars().count(), 600, "envelope error must be capped");
        assert!(!retryable);
    }

    #[test]
    fn url_validator() {
        assert!(is_http_url("https://a.com"));
        assert!(is_http_url("http://a.com:8080/x?y=1"));
        assert!(!is_http_url("javascript:alert(1)"));
        assert!(!is_http_url("ftp://a.com"));
        assert!(!is_http_url("data:text/plain,x"));
        assert!(!is_http_url("not a url"));
    }

    // ── spawn tests (real subprocesses, no network) ─────────

    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_roundtrip_sh() {
        // Reads stdin, echoes a valid envelope on stdout.
        let def = PluginDef {
            cmd: vec![
                "/bin/sh".into(),
                "-c".into(),
                r#"read -r line; printf '%s
' '{"format":1,"results":[{"title":"echo","url":"https://echo.example"}]}'"#
                    .into(),
            ],
            timeout_ms: 10_000,
        };
        let outcome = run_plugin("shecho", &def, "hello world", 5, &Intent::Web)
            .await
            .unwrap();
        assert_eq!(outcome.hits.len(), 1);
        assert_eq!(outcome.hits[0].title, "echo");
        // Don't assert ms > 0: a fast spawn+echo can legitimately
        // finish sub-millisecond. Upper bound only.
        assert!(outcome.ms < 60_000);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_nonzero_exit_uses_stderr() {
        let def = PluginDef {
            cmd: vec![
                "/bin/sh".into(),
                "-c".into(),
                "echo 'upstream rate limited' >&2; exit 3".into(),
            ],
            timeout_ms: 10_000,
        };
        let e = run_plugin("failer", &def, "q", 5, &Intent::Web)
            .await
            .unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("upstream rate limited"), "{msg}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_garbage_stdout_is_error() {
        let def = PluginDef {
            cmd: vec![
                "/bin/sh".into(),
                "-c".into(),
                "echo '<html>oops</html>'".into(),
            ],
            timeout_ms: 10_000,
        };
        let e = run_plugin("garbage", &def, "q", 5, &Intent::Web)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("not valid JSON"), "{e}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_huge_stdout_is_capped_and_killed() {
        // Emits megabytes and then sleeps: the cap must fire and
        // the process must die (POSIX pipe SIGPIPE / our kill).
        let def = PluginDef {
            cmd: vec![
                "/bin/sh".into(),
                "-c".into(),
                "yes 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx' | head -c 9000000"
                    .into(),
            ],
            timeout_ms: 10_000,
        };
        let e = run_plugin("flood", &def, "q", 5, &Intent::Web)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("exceeded"), "{e}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_timeout_kills_child() {
        let def = PluginDef {
            cmd: vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
            timeout_ms: 1_500,
        };
        let start = Instant::now();
        let e = run_plugin("slowpoke", &def, "q", 5, &Intent::Web)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("timed out"), "{e}");
        assert!(
            start.elapsed() < Duration::from_secs(12),
            "kill must not wait for the sleep"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_missing_program_is_clear_error() {
        let def = PluginDef {
            cmd: vec!["/nonexistent/definitely-not-here-xyz".into()],
            timeout_ms: 10_000,
        };
        let e = run_plugin("ghostbin", &def, "q", 5, &Intent::Web)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("failed to start"), "{}", e);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_stderr_deadlock_guard() {
        // The child writes ~200KB to stderr (larger than any pipe
        // buffer) and only then a valid envelope on stdout. If
        // stderr were not drained concurrently this would hang
        // until timeout instead of succeeding.
        let def = PluginDef {
            cmd: vec![
                "/bin/sh".into(),
                "-c".into(),
                "head -c 200000 /dev/zero | tr '\\0' 's' >&2; printf '%s\\n' '{\"format\":1,\"results\":[{\"title\":\"ok\",\"url\":\"https://ok.example\"}]}'"
                    .into(),
            ],
            timeout_ms: 15_000,
        };
        let outcome = run_plugin("chatty", &def, "q", 5, &Intent::Web)
            .await
            .unwrap();
        assert_eq!(outcome.hits.len(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_concurrent_plugins_are_independent() {
        // Two parallel searches each spawn their own child: they
        // must not (a) share pipes or (b) serialize. Each child
        // sleeps 1s, so wall time < 2s proves real concurrency.
        // Generous time margins survive slow parallel CI slots:
        // serialized = 6s+ (fails), parallel = ~3.3s (passes).
        let def = PluginDef {
            cmd: vec![
                "/bin/sh".into(),
                "-c".into(),
                "sleep 3; echo '{\"format\":1,\"results\":[{\"title\":\"c\",\"url\":\"https://c.example\"}]}'".into(),
            ],
            timeout_ms: 10_000,
        };
        let def = std::sync::Arc::new(def);
        let start = Instant::now();
        let d1 = def.clone();
        let d2 = def.clone();
        let a = tokio::spawn(async move { run_plugin("conca", &d1, "q1", 5, &Intent::Web).await });
        let b = tokio::spawn(async move { run_plugin("concb", &d2, "q2", 5, &Intent::Web).await });
        let (ra, rb) = tokio::join!(a, b);
        assert_eq!(ra.unwrap().unwrap().hits.len(), 1);
        assert_eq!(rb.unwrap().unwrap().hits.len(), 1);
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "parallel spawns must not serialize"
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn spawn_roundtrip_cmd() {
        // Windows spawn test without argv quoting games : the
        // envelope lives in a helper .bat file (exact bytes on
        // disk, CRLF line endings), and argv carries only the
        // bat path. A leading `set /p` consumes the stdin
        // request first so the full pipe contract is exercised.
        let mut bat = std::env::temp_dir();
        bat.push(format!(
            "donsetch_wintest_{}_{}.bat",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(
            &bat,
            "@echo off\r\nset /p line=\r\necho {\"format\":1,\"results\":[{\"title\":\"win\",\"url\":\"https://win.example\"}]}\r\n",
        )
        .unwrap();
        let def = PluginDef {
            cmd: vec![
                "cmd".into(),
                "/C".into(),
                bat.to_string_lossy().into_owned(),
            ],
            timeout_ms: 10_000,
        };
        let outcome = run_plugin("winecho", &def, "hello", 5, &Intent::Web)
            .await
            .unwrap();
        let _ = std::fs::remove_file(&bat);
        assert_eq!(outcome.hits.len(), 1);
        assert_eq!(outcome.hits[0].title, "win");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn spawn_nonzero_exit_uses_stderr_windows() {
        let def = PluginDef {
            cmd: vec![
                "cmd".into(),
                "/C".into(),
                "echo upstream rate limited 1>&2 & exit /b 3".into(),
            ],
            timeout_ms: 10_000,
        };
        let e = run_plugin("wfail", &def, "q", 5, &Intent::Web)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("rate limited"), "{e}");
    }
}
