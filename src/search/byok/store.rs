//! BYOK key storage: load/save, state machine, provider chain.
//!
//! File: ~/.cache/donsetch/byok-keys.json
//! Format: { default, providers: [{ name, keys: [{ key, state, ts }] }] }
//!
//! Key states:
//!   active         : ready to use
//!   rate_limited   : 429, auto-recovers after RATE_LIMIT_COOLDOWN
//!   credit_depleted : 402, stays dead until user resets
//!   invalid        : 401/403, permanently dead (wrong/revoked key)

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::cli;

/// Cooldown for rate-limited keys. After this elapses,
/// the key auto-recovers to active on next pick.
const RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(60);

/// Valid provider names (checked at CLI boundary).
pub const PROVIDERS: &[&str] = &[
    "tavily",
    "exa",
    "serper",
    "serpapi",
    "serpbase",
    "bravesearch",
    "tinyfish",
    "parallel",
    "brightdata",
    "unlocker",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyState {
    Active,
    RateLimited,
    CreditDepleted,
    Invalid,
}

impl KeyState {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::RateLimited => "rate-limited",
            Self::CreditDepleted => "credit-depleted",
            Self::Invalid => "invalid",
        }
    }

    pub fn icon(&self) -> &'static str {
        match self {
            Self::Active => "\u{2713}",
            Self::RateLimited => "\u{23F1}",
            Self::CreditDepleted => "\u{2717}",
            Self::Invalid => "\u{2717}",
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct KeyEntry {
    pub key: String,
    pub state: KeyState,
    /// When the state was last changed (Unix epoch seconds).
    /// Used for rate-limit cooldown calculation.
    #[serde(default)]
    pub ts: u64,
}

/// Redacts `key` — a derived Debug would print the plaintext BYOK
/// API key into any log/error output that formats a `KeyEntry` (or
/// a `ProviderConfig`/`ByokConfig` containing one) with `{:?}`. The
/// existing debug logging in `byok::mod` is careful to print only
/// `key.chars().take(8)`, but that's a manual convention, not
/// something the type system enforces — this closes the gap so a
/// future `{cfg:?}`-style dump can't defeat it by accident.
impl std::fmt::Debug for KeyEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyEntry")
            .field("key", &"***")
            .field("state", &self.state)
            .field("ts", &self.ts)
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    pub keys: Vec<KeyEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ByokConfig {
    pub default: String,
    pub providers: Vec<ProviderConfig>,
}

impl ByokConfig {
    pub fn empty() -> Self {
        Self {
            default: String::new(),
            providers: Vec::new(),
        }
    }

    /// Load from disk. Returns empty config if file missing
    /// or corrupt (with a warning to stderr).
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::empty();
        };
        match std::fs::read_to_string(&path) {
            Ok(raw) => match serde_json::from_str(&raw) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("[byok] warning: corrupt key file ({e}), ignoring");
                    Self::empty()
                }
            },
            Err(_) => Self::empty(),
        }
    }

    /// Save to disk with restrictive permissions (0600).
    pub fn save(&self) {
        let Some(path) = config_path() else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match serde_json::to_string_pretty(self) {
            Ok(json) => {
                let tmp = path.with_extension("tmp");
                // Create the tmp file 0600 BEFORE writing key
                // material : the old write-then-chmod path left a
                // world-readable file behind on any crash.
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
                    // Restrict permissions: only owner can read.
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let _ =
                            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
                    }
                }
            }
            Err(e) => eprintln!("[byok] warning: failed to save keys ({e})"),
        }
    }

    /// True if at least one provider has at least one key.
    pub fn is_configured(&self) -> bool {
        self.providers.iter().any(|p| !p.keys.is_empty())
    }

    /// Serialize to JSON string (for export).
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".into())
    }

    /// Deserialize from JSON (for import). Validates structure.
    pub fn from_json(json: &str) -> Result<Self, String> {
        let cfg: ByokConfig =
            serde_json::from_str(json).map_err(|e| format!("invalid config: {e}"))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Validate: provider names must be known, keys non-empty.
    /// The default must be "local", a configured provider, or a
    /// well-formed plugin-name-shaped string (plugins live in a
    /// separate store; the runtime picker resolves the rest).
    pub fn validate(&self) -> Result<(), String> {
        for p in &self.providers {
            if !PROVIDERS.contains(&p.name.as_str()) {
                return Err(format!("unknown provider: {}", p.name));
            }
            for k in &p.keys {
                if k.key.trim().is_empty() {
                    return Err(format!("provider {} has an empty key", p.name));
                }
            }
        }
        if !self.default.is_empty()
            && self.default != "local"
            && !self.providers.iter().any(|p| p.name == self.default)
            && !is_plugin_shaped(&self.default)
        {
            return Err(format!(
                "default '{}' is not a configured provider",
                self.default
            ));
        }
        Ok(())
    }

    /// Add a key to a provider. Creates the provider if new.
    /// If this is the first key ever, sets it as default.
    pub fn add_key(&mut self, provider: &str, key: &str) {
        // Check if provider already exists.
        if let Some(p) = self.providers.iter_mut().find(|p| p.name == provider) {
            // Don't add duplicate keys.
            if p.keys.iter().any(|k| k.key == key) {
                return;
            }
            p.keys.push(KeyEntry {
                key: key.to_string(),
                state: KeyState::Active,
                ts: now_ts(),
            });
        } else {
            self.providers.push(ProviderConfig {
                name: provider.to_string(),
                keys: vec![KeyEntry {
                    key: key.to_string(),
                    state: KeyState::Active,
                    ts: now_ts(),
                }],
            });
        }
        // First provider added becomes the default.
        if self.default.is_empty() {
            self.default = provider.to_string();
        }
    }

    /// Remove a specific key from a provider, or all keys
    /// if key_str is None. Returns true if any keys were removed.
    pub fn remove_keys(&mut self, provider: &str, key_str: Option<&str>) -> bool {
        let Some(p) = self.providers.iter_mut().find(|p| p.name == provider) else {
            return false;
        };
        let before = p.keys.len();
        match key_str {
            Some(k) => p.keys.retain(|e| e.key != k),
            None => p.keys.clear(),
        }
        let removed = p.keys.len() < before;
        // Remove provider if no keys left.
        if p.keys.is_empty() {
            self.providers.retain(|p| p.name != provider);
            // Fix default if it was this provider.
            if self.default == provider {
                self.default = self
                    .providers
                    .first()
                    .map(|p| p.name.clone())
                    .unwrap_or_default();
            }
        }
        removed
    }

    /// Set the default search method. Accepts "local" (use the
    /// keyless engine first, BYOK as fallback) or a configured
    /// provider name. Returns false only for an unknown provider.
    pub fn set_default(&mut self, provider: &str) -> bool {
        if provider == "local" {
            self.default = "local".to_string();
            return true;
        }
        if self.providers.iter().any(|p| p.name == provider) {
            self.default = provider.to_string();
            true
        } else {
            false
        }
    }

    /// True if "local" is the default search method (local-first,
    /// BYOK fallback). When false, BYOK is tried first.
    pub fn is_local_default(&self) -> bool {
        self.default == "local"
    }

    /// Reset key states to active. If provider is None, resets all.
    pub fn reset_states(&mut self, provider: Option<&str>) {
        for p in &mut self.providers {
            if provider.is_some_and(|name| name != p.name) {
                continue;
            }
            for k in &mut p.keys {
                k.state = KeyState::Active;
                k.ts = now_ts();
            }
        }
    }

    /// Pick the next usable (provider, key) pair, skipping
    /// any pairs in the `skip` set. This is used to avoid
    /// retrying keys that had transient errors (5xx, network)
    /// in the same search call : without it, pick_key() would
    /// return the same active key again, infinite loop.
    pub fn pick_key_skipping(
        &mut self,
        skip: &std::collections::HashSet<(String, String)>,
    ) -> Option<(String, String)> {
        // Build the priority order: default first, then rest.
        let mut order: Vec<String> = Vec::with_capacity(self.providers.len());
        if !self.default.is_empty() {
            order.push(self.default.clone());
        }
        for p in &self.providers {
            if p.name != self.default {
                order.push(p.name.clone());
            }
        }

        for name in &order {
            let Some(p) = self.providers.iter_mut().find(|p| &p.name == name) else {
                continue;
            };
            for k in &mut p.keys {
                match k.state {
                    KeyState::Active => {
                        let pair = (p.name.clone(), k.key.clone());
                        if skip.contains(&pair) {
                            continue;
                        }
                        return Some(pair);
                    }
                    KeyState::RateLimited => {
                        // Auto-recover if cooldown passed.
                        let elapsed = now_ts().saturating_sub(k.ts);
                        if Duration::from_secs(elapsed) >= RATE_LIMIT_COOLDOWN {
                            k.state = KeyState::Active;
                            k.ts = now_ts();
                            let pair = (p.name.clone(), k.key.clone());
                            if skip.contains(&pair) {
                                continue;
                            }
                            return Some(pair);
                        }
                    }
                    KeyState::CreditDepleted | KeyState::Invalid => {}
                }
            }
        }
        None
    }

    /// Pick the next usable (provider, key) pair.
    /// Tries the default provider first, then others in order.
    /// of configuration. Within a provider, tries keys in order.
    /// Auto-recovers rate-limited keys whose cooldown has passed.
    /// Returns None if no usable key exists.
    #[allow(dead_code)]
    pub fn pick_key(&mut self) -> Option<(String, String)> {
        // Build the priority order: default first, then rest.
        let mut order: Vec<String> = Vec::with_capacity(self.providers.len());
        if !self.default.is_empty() {
            order.push(self.default.clone());
        }
        for p in &self.providers {
            if p.name != self.default {
                order.push(p.name.clone());
            }
        }

        for name in &order {
            let Some(p) = self.providers.iter_mut().find(|p| &p.name == name) else {
                continue;
            };
            for k in &mut p.keys {
                match k.state {
                    KeyState::Active => {
                        return Some((p.name.clone(), k.key.clone()));
                    }
                    KeyState::RateLimited => {
                        // Auto-recover if cooldown passed.
                        let elapsed = now_ts().saturating_sub(k.ts);
                        if Duration::from_secs(elapsed) >= RATE_LIMIT_COOLDOWN {
                            k.state = KeyState::Active;
                            k.ts = now_ts();
                            return Some((p.name.clone(), k.key.clone()));
                        }
                    }
                    KeyState::CreditDepleted | KeyState::Invalid => {}
                }
            }
        }
        None
    }

    /// Update the state of a specific key within a provider.
    pub fn update_key_state(&mut self, provider: &str, key: &str, state: KeyState) {
        let Some(p) = self.providers.iter_mut().find(|p| p.name == provider) else {
            return;
        };
        for k in &mut p.keys {
            if k.key == key {
                k.state = state;
                k.ts = now_ts();
                return;
            }
        }
    }
}

/// Thread-safe wrapper for runtime use.
pub struct ByokStore {
    config: Mutex<ByokConfig>,
}

impl Default for ByokStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ByokStore {
    pub fn new() -> Self {
        Self {
            config: Mutex::new(ByokConfig::load()),
        }
    }

    pub fn is_configured(&self) -> bool {
        self.config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_configured()
    }

    /// True if "local" is the default search method.
    pub fn is_local_default(&self) -> bool {
        self.config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_local_default()
    }

    pub fn pick_key_skipping(
        &self,
        skip: &std::collections::HashSet<(String, String)>,
    ) -> Option<(String, String)> {
        self.config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pick_key_skipping(skip)
    }

    pub fn update_key_state(&self, provider: &str, key: &str, state: KeyState) {
        let mut cfg = self
            .config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cfg.update_key_state(provider, key, state);
        cfg.save();
    }

    /// The configured default (may name a plugin; may be empty).
    pub fn current_default(&self) -> String {
        self.config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .default
            .clone()
    }

    /// Reload config from disk (picks up CLI key changes).
    pub fn reload(&self) {
        let new_cfg = ByokConfig::load();
        *self
            .config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = new_cfg;
    }
}

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A default that is neither "local" nor a keyed provider may
/// still name a BYOK plugin: plugins are stored separately, so
/// validation here checks only the shape (lowercase
/// [a-z0-9][a-z0-9_-]* up to 32 chars). The runtime picker
/// resolves whether a plugin of that name actually exists.
pub(crate) fn is_plugin_shaped(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    name.len() <= 32
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

fn config_path() -> Option<PathBuf> {
    Some(crate::paths::cache_dir().join("byok-keys.json"))
}

// ── CLI rendering ──────────────────────────────────────────

/// Render the key list for `donsetch keys list`.
pub fn render_list(cfg: &ByokConfig) {
    if !cfg.is_configured() {
        println!("  {}", cli::dim("no keys configured"));
        println!();
        println!(
            "  Add a key with:  {}",
            cli::bold("donsetch keys add <provider> <key>")
        );
        println!(
            "  Providers:       {}",
            cli::dim("tavily, exa, serper, serpbase, tinyfish, parallel, brightdata")
        );
        return;
    }

    cli::print_title("BYOK Search Providers");
    println!();

    // Show "local" as the active default when set.
    if cfg.default == "local" {
        println!(
            "  {} {} {}",
            cli::green("\u{25C6}"),
            cli::bold("local"),
            cli::dim("(default)")
        );
        println!(
            "    {} keyless 5-engine search (local-first, BYOK fallback)",
            cli::dim("")
        );
        println!();
    }

    for p in &cfg.providers {
        let is_default = p.name == cfg.default;
        let marker = if is_default {
            cli::green("\u{25C6}")
        } else {
            cli::dim("\u{25C7}")
        };
        let label = if is_default {
            format!(
                "{} {} {}",
                marker,
                cli::bold(&p.name),
                cli::dim("(default)")
            )
        } else {
            format!("{} {}", marker, p.name)
        };
        println!("  {label}");

        for k in &p.keys {
            let state_label = match k.state {
                KeyState::Active => cli::green(k.state.icon()),
                KeyState::RateLimited => cli::yellow(k.state.icon()),
                KeyState::CreditDepleted | KeyState::Invalid => cli::red(k.state.icon()),
            };
            let masked = mask_key(&k.key);
            println!(
                "    {} {} {}",
                state_label,
                cli::dim(&masked),
                cli::dim(k.state.label()),
            );
        }
        println!();
    }

    println!(
        "  {}  {} active  {} rate-limited  {} dead",
        cli::dim("legend:"),
        cli::green("\u{2713}"),
        cli::yellow("\u{23F1}"),
        cli::red("\u{2717}"),
    );
    println!("  {}  {}", cli::dim("default:"), cli::green(&cfg.default));

    // Warn if no usable keys remain : search will fall back
    // to the local keyless engine.
    let any_active = cfg
        .providers
        .iter()
        .flat_map(|p| &p.keys)
        .any(|k| matches!(k.state, KeyState::Active | KeyState::RateLimited));
    if !any_active {
        println!();
        println!(
            "  {} all keys are dead : search falls back to local engine",
            cli::yellow("\u{26A0}")
        );
        println!(
            "     run {} to revive them",
            cli::bold("donsetch keys reset")
        );
    }

    cli::print_footer();
}

/// Mask a key for display: show first 8 and last 4 chars.
fn mask_key(key: &str) -> String {
    if key.len() <= 14 {
        return key.to_string();
    }
    // Char-boundary-safe: a pasted key containing multi-byte chars
    // would panic on a raw byte slice.
    let head: String = key.chars().take(8).collect();
    let tail: String = key
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}...{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_key_and_propagates_through_containers() {
        let mut cfg = ByokConfig::empty();
        cfg.add_key("tavily", "tvly-s3cret-actual-key");

        let entry_out = format!("{:?}", cfg.providers[0].keys[0]);
        assert!(
            !entry_out.contains("tvly-s3cret-actual-key"),
            "leaked key: {entry_out}"
        );
        assert!(entry_out.contains("***"));

        // The derived Debug on the containing structs calls
        // KeyEntry's own (redacted) fmt for each element — the leak
        // must not resurface just by formatting the outer config.
        let cfg_out = format!("{cfg:?}");
        assert!(
            !cfg_out.contains("tvly-s3cret-actual-key"),
            "leaked key via container Debug: {cfg_out}"
        );
        assert!(cfg_out.contains("***"));
    }

    #[test]
    fn add_key_creates_provider_and_sets_default() {
        let mut cfg = ByokConfig::empty();
        cfg.add_key("tavily", "tvly-test123");
        assert_eq!(cfg.default, "tavily");
        assert_eq!(cfg.providers.len(), 1);
        assert_eq!(cfg.providers[0].keys.len(), 1);
        assert_eq!(cfg.providers[0].keys[0].state, KeyState::Active);
    }

    #[test]
    fn add_key_to_existing_provider_stacks() {
        let mut cfg = ByokConfig::empty();
        cfg.add_key("tavily", "tvly-key1");
        cfg.add_key("tavily", "tvly-key2");
        assert_eq!(cfg.providers[0].keys.len(), 2);
    }

    #[test]
    fn add_duplicate_key_is_noop() {
        let mut cfg = ByokConfig::empty();
        cfg.add_key("tavily", "tvly-key1");
        cfg.add_key("tavily", "tvly-key1");
        assert_eq!(cfg.providers[0].keys.len(), 1);
    }

    #[test]
    fn remove_specific_key() {
        let mut cfg = ByokConfig::empty();
        cfg.add_key("tavily", "tvly-key1");
        cfg.add_key("tavily", "tvly-key2");
        assert!(cfg.remove_keys("tavily", Some("tvly-key1")));
        assert_eq!(cfg.providers[0].keys.len(), 1);
        assert_eq!(cfg.providers[0].keys[0].key, "tvly-key2");
    }

    #[test]
    fn remove_all_keys_removes_provider_and_fixes_default() {
        let mut cfg = ByokConfig::empty();
        cfg.add_key("tavily", "tvly-key1");
        cfg.add_key("exa", "exa-key1");
        assert_eq!(cfg.default, "tavily");
        cfg.remove_keys("tavily", None);
        assert_eq!(cfg.providers.len(), 1);
        assert_eq!(cfg.default, "exa");
    }

    #[test]
    fn pick_key_returns_default_first() {
        let mut cfg = ByokConfig::empty();
        cfg.add_key("tavily", "tvly-key1");
        cfg.add_key("exa", "exa-key1");
        cfg.set_default("exa");
        let (provider, key) = cfg.pick_key().unwrap();
        assert_eq!(provider, "exa");
        assert_eq!(key, "exa-key1");
    }

    #[test]
    fn pick_key_skips_dead_keys() {
        let mut cfg = ByokConfig::empty();
        cfg.add_key("tavily", "tvly-key1");
        cfg.add_key("tavily", "tvly-key2");
        cfg.update_key_state("tavily", "tvly-key1", KeyState::CreditDepleted);
        let (_, key) = cfg.pick_key().unwrap();
        assert_eq!(key, "tvly-key2");
    }

    #[test]
    fn pick_key_falls_to_next_provider() {
        let mut cfg = ByokConfig::empty();
        cfg.add_key("tavily", "tvly-key1");
        cfg.add_key("exa", "exa-key1");
        cfg.update_key_state("tavily", "tvly-key1", KeyState::Invalid);
        let (provider, _) = cfg.pick_key().unwrap();
        assert_eq!(provider, "exa");
    }

    #[test]
    fn pick_key_returns_none_when_all_dead() {
        let mut cfg = ByokConfig::empty();
        cfg.add_key("tavily", "tvly-key1");
        cfg.update_key_state("tavily", "tvly-key1", KeyState::CreditDepleted);
        assert!(cfg.pick_key().is_none());
    }

    #[test]
    fn rate_limited_auto_recovers() {
        let mut cfg = ByokConfig::empty();
        cfg.add_key("tavily", "tvly-key1");
        // Manually set to rate_limited with old timestamp.
        cfg.providers[0].keys[0].state = KeyState::RateLimited;
        cfg.providers[0].keys[0].ts = now_ts().saturating_sub(120); // 2 min ago
        let (_, key) = cfg.pick_key().unwrap();
        assert_eq!(key, "tvly-key1");
        assert_eq!(cfg.providers[0].keys[0].state, KeyState::Active);
    }

    #[test]
    fn rate_limited_stays_limited_within_cooldown() {
        let mut cfg = ByokConfig::empty();
        cfg.add_key("tavily", "tvly-key1");
        cfg.providers[0].keys[0].state = KeyState::RateLimited;
        cfg.providers[0].keys[0].ts = now_ts(); // just now
        assert!(cfg.pick_key().is_none());
    }

    #[test]
    fn reset_states_revives_all() {
        let mut cfg = ByokConfig::empty();
        cfg.add_key("tavily", "tvly-key1");
        cfg.add_key("exa", "exa-key1");
        cfg.update_key_state("tavily", "tvly-key1", KeyState::CreditDepleted);
        cfg.update_key_state("exa", "exa-key1", KeyState::Invalid);
        cfg.reset_states(None);
        assert_eq!(cfg.providers[0].keys[0].state, KeyState::Active);
        assert_eq!(cfg.providers[1].keys[0].state, KeyState::Active);
    }

    #[test]
    fn reset_states_single_provider() {
        let mut cfg = ByokConfig::empty();
        cfg.add_key("tavily", "tvly-key1");
        cfg.add_key("exa", "exa-key1");
        cfg.update_key_state("tavily", "tvly-key1", KeyState::Invalid);
        cfg.update_key_state("exa", "exa-key1", KeyState::Invalid);
        cfg.reset_states(Some("tavily"));
        assert_eq!(cfg.providers[0].keys[0].state, KeyState::Active);
        assert_eq!(cfg.providers[1].keys[0].state, KeyState::Invalid);
    }

    // ── local-default tests ──────────────────────────────

    #[test]
    fn set_default_local_without_keys() {
        let mut cfg = ByokConfig::empty();
        assert!(cfg.set_default("local"));
        assert_eq!(cfg.default, "local");
        assert!(cfg.is_local_default());
    }

    #[test]
    fn set_default_local_with_keys() {
        let mut cfg = ByokConfig::empty();
        cfg.add_key("tavily", "tvly-key1");
        assert_eq!(cfg.default, "tavily");
        assert!(cfg.set_default("local"));
        assert_eq!(cfg.default, "local");
        assert!(cfg.is_local_default());
    }

    #[test]
    fn set_default_local_back_to_provider() {
        let mut cfg = ByokConfig::empty();
        cfg.add_key("tavily", "tvly-key1");
        cfg.set_default("local");
        assert!(cfg.is_local_default());
        assert!(cfg.set_default("tavily"));
        assert!(!cfg.is_local_default());
        assert_eq!(cfg.default, "tavily");
    }

    #[test]
    fn add_key_preserves_local_default() {
        let mut cfg = ByokConfig::empty();
        cfg.set_default("local");
        cfg.add_key("tavily", "tvly-key1");
        // Default should stay "local", not switch to tavily.
        assert_eq!(cfg.default, "local");
        assert!(cfg.is_local_default());
        assert_eq!(cfg.providers.len(), 1);
    }

    #[test]
    fn pick_key_skipping_works_with_local_default() {
        let mut cfg = ByokConfig::empty();
        cfg.add_key("tavily", "tvly-key1");
        cfg.add_key("exa", "exa-key1");
        cfg.set_default("local");
        // pick_key_skipping should still find keys : "local" is
        // not a provider, so it's skipped and providers are tried
        // in config order.
        let skip = std::collections::HashSet::new();
        let (provider, _) = cfg.pick_key_skipping(&skip).unwrap();
        // First in config order (not "local" since it's not a provider).
        assert_eq!(provider, "tavily");
    }

    #[test]
    fn remove_keys_preserves_local_default() {
        let mut cfg = ByokConfig::empty();
        cfg.add_key("tavily", "tvly-key1");
        cfg.add_key("exa", "exa-key1");
        cfg.set_default("local");
        // Remove exa : default should stay "local".
        cfg.remove_keys("exa", None);
        assert_eq!(cfg.default, "local");
        assert!(cfg.is_local_default());
        assert_eq!(cfg.providers.len(), 1);
    }

    #[test]
    fn remove_all_keys_with_local_default() {
        let mut cfg = ByokConfig::empty();
        cfg.add_key("tavily", "tvly-key1");
        cfg.set_default("local");
        cfg.remove_keys("tavily", None);
        // Default stays "local" (not reset to empty) because
        // the removed provider wasn't the default.
        assert_eq!(cfg.default, "local");
        assert!(!cfg.is_configured());
    }

    #[test]
    fn is_local_default_false_for_provider() {
        let mut cfg = ByokConfig::empty();
        cfg.add_key("tavily", "tvly-key1");
        assert!(!cfg.is_local_default());
    }

    #[test]
    fn is_local_default_false_for_empty() {
        let cfg = ByokConfig::empty();
        assert!(!cfg.is_local_default());
    }

    // ── export/import tests ──────────────────────────────

    #[test]
    fn to_json_round_trip() {
        let mut cfg = ByokConfig::empty();
        cfg.add_key("tavily", "tvly-key1");
        cfg.add_key("exa", "exa-key1");
        cfg.set_default("local");
        let json = cfg.to_json();
        let restored = ByokConfig::from_json(&json).unwrap();
        assert_eq!(restored.default, "local");
        assert_eq!(restored.providers.len(), 2);
        assert_eq!(restored.providers[0].name, "tavily");
        assert_eq!(restored.providers[0].keys[0].key, "tvly-key1");
        assert_eq!(restored.providers[1].name, "exa");
    }

    #[test]
    fn from_json_rejects_unknown_provider() {
        let json = r#"{"default":"","providers":[{"name":"unknown","keys":[{"key":"x","state":"active","ts":0}]}]}"#;
        assert!(ByokConfig::from_json(json).is_err());
    }

    #[test]
    fn from_json_accepts_plugin_shaped_default() {
        // Plugin names live in a separate store: the default may
        // name one as long as the shape is right.
        let json = r#"{"default":"searxng","providers":[{"name":"tavily","keys":[{"key":"x","state":"active","ts":0}]}]}"#;
        assert!(ByokConfig::from_json(json).is_ok());
        let json_bad_shape = r#"{"default":"Bad Default!","providers":[]}"#;
        assert!(ByokConfig::from_json(json_bad_shape).is_err());
    }

    #[test]
    fn from_json_rejects_invalid_default() {
        let json = r#"{"default":"ghost／etc","providers":[{"name":"tavily","keys":[{"key":"x","state":"active","ts":0}]}]}"#;
        assert!(ByokConfig::from_json(json).is_err());
    }

    #[test]
    fn from_json_accepts_local_default() {
        let json = r#"{"default":"local","providers":[{"name":"tavily","keys":[{"key":"x","state":"active","ts":0}]}]}"#;
        let cfg = ByokConfig::from_json(json).unwrap();
        assert!(cfg.is_local_default());
    }

    #[test]
    fn from_json_rejects_empty_key() {
        let json = r#"{"default":"","providers":[{"name":"tavily","keys":[{"key":"","state":"active","ts":0}]}]}"#;
        assert!(ByokConfig::from_json(json).is_err());
    }

    #[test]
    fn from_json_rejects_malformed() {
        assert!(ByokConfig::from_json("not json").is_err());
        assert!(ByokConfig::from_json("{}").is_err());
    }
}
