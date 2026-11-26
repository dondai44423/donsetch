//! Interactive Crimson-Noir Terminal Settings Control Center (`donsetch settings`).
//!
//! Full-screen, flicker-free, cross-platform TUI supporting:
//! - 8 category tabs (Search, Fetch, Providers, Proxies, Browser, Server, Rules, Presets)
//! - Instant toggles, fine/coarse sliders, enum selectors, masked secret editors
//! - Live search across all tabs (`/`)
//! - Full BYOK key & custom plugin management (`byok-keys.json` + `byok-plugins.json`)
//! - Full SOCKS5/HTTP proxy pool management + async live proxy probing (`proxies.txt`)
//! - Per-host policy rule builder, toggle, removal, and live URL matcher
//! - One-click presets (including Slow / High-RTT Network for #470/#478)
//! - Custom Crimson-Noir animations (header laser sweep, live comet scanner,
//!   tactile toggle/slider gleam flash, and smooth status toasts)

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use crate::config::{self, DonsetchConfig, Loaded, TomlEditValue};
use crate::egress::proxy::{self, Proxy, ProxyScheme};
use crate::rules::{DenyKind, RuleAction, RuleSet, UrlRule};
use crate::search::byok::plugin::{
    DEFAULT_TIMEOUT_MS, PluginConfig, tokenize_cmd, validate_plugin_name,
};
use crate::search::byok::store::{ByokConfig, KeyState, PROVIDERS, is_fetch_side};

use super::schema::{self, ControlKind, PRESETS, Preset, SETTINGS, SettingSpec, TabId};
use super::term::{self, KeyEvent, TerminalGuard};

#[derive(Debug, Clone)]
enum Row {
    Header(String),
    Setting(&'static SettingSpec),
    DefaultProvider,
    Provider(&'static str),
    AddPlugin,
    Plugin(String),
    AddProxy,
    ProxyItem(usize),
    AddRule,
    TestRule,
    RuleItem(String),
    Preset(&'static Preset),
}

impl Row {
    fn selectable(&self) -> bool {
        !matches!(self, Self::Header(_))
    }
}

#[derive(Debug, Clone)]
enum Modal {
    EditSetting {
        spec: &'static SettingSpec,
        buffer: String,
        revealed: bool,
        error: Option<String>,
    },
    AddProviderKey {
        provider: &'static str,
        buffer: String,
        revealed: bool,
        error: Option<String>,
    },
    ConfirmRemoveProvider {
        provider: &'static str,
        count: usize,
    },
    AddPlugin {
        step: u8,
        name: String,
        cmd: String,
        timeout_secs: String,
        error: Option<String>,
    },
    ConfirmRemovePlugin {
        name: String,
    },
    AddProxy {
        buffer: String,
        error: Option<String>,
    },
    ConfirmClearProxies {
        count: usize,
    },
    AddRule {
        step: u8,
        pattern: String,
        action_deny: bool,
        message: String,
        kind_permanent: bool,
        reason: String,
        error: Option<String>,
    },
    TestRuleUrl {
        buffer: String,
        result: Option<String>,
    },
    ConfirmQuit,
    Help,
}

struct PendingProbe {
    proxy_id: String,
    handle: tokio::task::JoinHandle<crate::cli::proxy::ProbeResult>,
}

struct App {
    loaded: Loaded,
    saved_cfg: DonsetchConfig,
    draft_cfg: DonsetchConfig,
    staged: BTreeMap<(String, String), TomlEditValue>,
    byok_cfg: ByokConfig,
    plugin_cfg: PluginConfig,
    proxies: Vec<Proxy>,
    proxy_probes: HashMap<String, (bool, String)>,
    pending_probe: Option<PendingProbe>,
    active_tab: TabId,
    selected: usize,
    scroll: usize,
    search_mode: bool,
    search_query: String,
    modal: Option<Modal>,
    tick: usize,
    boot_frames: usize,
    tab_sweep: usize,
    flash: Option<(usize, usize)>,
    toast: Option<(String, bool, usize)>,
    should_quit: bool,
    saved_summary: Option<String>,
}

impl App {
    fn new(loaded: Loaded) -> Self {
        let saved_cfg = loaded.config.clone();
        let draft_cfg = saved_cfg.clone();
        let byok_cfg = ByokConfig::load();
        let plugin_cfg = PluginConfig::load();
        let proxies = proxy::load_config();
        let mut app = Self {
            loaded,
            saved_cfg,
            draft_cfg,
            staged: BTreeMap::new(),
            byok_cfg,
            plugin_cfg,
            proxies,
            proxy_probes: HashMap::new(),
            pending_probe: None,
            active_tab: TabId::Search,
            selected: 0,
            scroll: 0,
            search_mode: false,
            search_query: String::new(),
            modal: None,
            tick: 0,
            boot_frames: 14,
            tab_sweep: 0,
            flash: None,
            toast: None,
            should_quit: false,
            saved_summary: None,
        };
        app.snap_selection_to_selectable();
        app
    }

    fn reload_disk_config(&mut self) {
        if let Ok(fresh) = config::load_for_editor() {
            self.saved_cfg = fresh.config.clone();
            self.draft_cfg = fresh.config.clone();
            self.loaded = fresh;
            // Re-apply any remaining staged changes.
            let keys: Vec<_> = self.staged.clone().into_iter().collect();
            for ((sec, key), val) in keys {
                if let Some(spec) = schema::find_setting(&format!("{sec}.{key}")) {
                    let effective = if matches!(val, TomlEditValue::Unset) {
                        schema::get_value(&DonsetchConfig::default(), spec)
                    } else {
                        val
                    };
                    let _ = schema::set_in_memory(&mut self.draft_cfg, spec, &effective);
                }
            }
        }
    }

    fn set_toast(&mut self, msg: impl Into<String>, is_error: bool) {
        self.toast = Some((msg.into(), is_error, 95));
    }

    fn trigger_flash(&mut self, row_idx: usize) {
        self.flash = Some((row_idx, 7));
    }

    fn rows(&self) -> Vec<Row> {
        let mut out = Vec::new();
        if !self.search_query.trim().is_empty() {
            let needle = self.search_query.trim().to_ascii_lowercase();
            out.push(Row::Header(format!(
                "Search Results for \"{}\"",
                self.search_query.trim()
            )));
            for spec in SETTINGS {
                let hay = format!(
                    "{} {} {} {} {} {}",
                    spec.section,
                    spec.key,
                    spec.label,
                    spec.summary,
                    spec.group,
                    spec.env_var()
                )
                .to_ascii_lowercase();
                if hay.contains(&needle) {
                    out.push(Row::Setting(spec));
                }
            }
            for &p in PROVIDERS {
                if p.contains(&needle) || "byok provider key".contains(&needle) {
                    out.push(Row::Provider(p));
                }
            }
            for preset in PRESETS {
                let hay = format!("{} {} {}", preset.id, preset.name, preset.tagline)
                    .to_ascii_lowercase();
                if hay.contains(&needle) {
                    out.push(Row::Preset(preset));
                }
            }
            return out;
        }

        match self.active_tab {
            TabId::Search | TabId::Fetch | TabId::Browser | TabId::Server => {
                self.push_tab_settings(&mut out, self.active_tab);
            }
            TabId::Providers => {
                out.push(Row::Header("Search Routing & Self-Hosted".into()));
                out.push(Row::DefaultProvider);
                if let Some(sx) = schema::find_setting("search.searxng_url") {
                    out.push(Row::Setting(sx));
                }
                out.push(Row::Header(
                    "Native BYOK Providers (Enter/a: add key · r: reset · d: remove)".into(),
                ));
                for &p in PROVIDERS {
                    out.push(Row::Provider(p));
                }
                out.push(Row::Header("Custom Executable Search Plugins".into()));
                out.push(Row::AddPlugin);
                for name in self.plugin_cfg.names() {
                    out.push(Row::Plugin(name.clone()));
                }
            }
            TabId::Proxies => {
                self.push_tab_settings(&mut out, TabId::Proxies);
                out.push(Row::Header(format!(
                    "Interactive Proxy Pool ({} configured · t: probe · d: remove · c: clear)",
                    self.proxies.len()
                )));
                out.push(Row::AddProxy);
                for i in 0..self.proxies.len() {
                    out.push(Row::ProxyItem(i));
                }
            }
            TabId::Rules => {
                self.push_tab_settings(&mut out, TabId::Rules);
                out.push(Row::Header(format!(
                    "Per-Host Rules ({} configured · Space: toggle · d: remove)",
                    self.draft_cfg.rules.url.len()
                )));
                out.push(Row::AddRule);
                out.push(Row::TestRule);
                for pattern in self.draft_cfg.rules.url.keys() {
                    out.push(Row::RuleItem(pattern.clone()));
                }
            }
            TabId::Presets => {
                out.push(Row::Header(
                    "Curated Presets (Enter/Space stages preset; press s to save)".into(),
                ));
                for preset in PRESETS {
                    out.push(Row::Preset(preset));
                }
                self.push_tab_settings(&mut out, TabId::Presets);
            }
        }
        out
    }

    fn push_tab_settings(&self, out: &mut Vec<Row>, tab: TabId) {
        let mut last_group = "";
        for spec in SETTINGS.iter().filter(|s| s.tab == tab) {
            if spec.group != last_group {
                out.push(Row::Header(spec.group.to_string()));
                last_group = spec.group;
            }
            out.push(Row::Setting(spec));
        }
    }

    fn snap_selection_to_selectable(&mut self) {
        let rows = self.rows();
        if rows.is_empty() {
            self.selected = 0;
            self.scroll = 0;
            return;
        }
        if self.selected >= rows.len() {
            self.selected = rows.len() - 1;
        }
        if !rows[self.selected].selectable() {
            if let Some(pos) = rows
                .iter()
                .enumerate()
                .skip(self.selected)
                .find(|(_, r)| r.selectable())
                .map(|(i, _)| i)
            {
                self.selected = pos;
            } else if let Some(pos) = rows
                .iter()
                .enumerate()
                .take(self.selected)
                .rev()
                .find(|(_, r)| r.selectable())
                .map(|(i, _)| i)
            {
                self.selected = pos;
            }
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let rows = self.rows();
        if rows.is_empty() {
            return;
        }
        let mut idx = self.selected as isize;
        let len = rows.len() as isize;
        for _ in 0..rows.len() {
            idx += delta.signum();
            if idx < 0 {
                idx = len - 1;
            } else if idx >= len {
                idx = 0;
            }
            if rows[idx as usize].selectable() {
                self.selected = idx as usize;
                return;
            }
        }
    }

    fn switch_tab(&mut self, tab: TabId) {
        if self.active_tab != tab {
            self.active_tab = tab;
            self.selected = 0;
            self.scroll = 0;
            self.tab_sweep = 6;
            self.snap_selection_to_selectable();
        }
    }

    fn stage_setting(&mut self, spec: &SettingSpec, val: TomlEditValue) -> Result<(), String> {
        let preview_val = if matches!(val, TomlEditValue::Unset) {
            schema::get_value(&DonsetchConfig::default(), spec)
        } else {
            val.clone()
        };
        schema::set_in_memory(&mut self.draft_cfg, spec, &preview_val)?;
        let key = (spec.section.to_string(), spec.key.to_string());
        if !matches!(val, TomlEditValue::Unset) && schema::get_value(&self.saved_cfg, spec) == val {
            self.staged.remove(&key);
        } else {
            self.staged.insert(key, val);
        }
        Ok(())
    }

    fn save_staged(&mut self) -> bool {
        if self.staged.is_empty() {
            self.set_toast("No unsaved config file changes", false);
            return true;
        }
        let entries: Vec<(String, String, TomlEditValue)> = self
            .staged
            .iter()
            .map(|((s, k), v)| (s.clone(), k.clone(), v.clone()))
            .collect();
        let refs: Vec<(&str, &str, TomlEditValue)> = entries
            .iter()
            .map(|(s, k, v)| (s.as_str(), k.as_str(), v.clone()))
            .collect();
        match config::set_file_values(&refs) {
            Ok(path) => {
                let count = entries.len();
                self.staged.clear();
                self.reload_disk_config();
                let word = if count == 1 { "setting" } else { "settings" };
                let msg = format!("Saved {count} {word} to {}", path.display());
                self.saved_summary = Some(msg.clone());
                self.set_toast(msg, false);
                true
            }
            Err(e) => {
                self.set_toast(format!("Save failed: {e}"), true);
                false
            }
        }
    }

    fn available_default_providers(&self) -> Vec<String> {
        let mut choices = vec!["local".to_string()];
        if !self.draft_cfg.search.searxng_url.trim().is_empty() {
            choices.push("searxng".to_string());
        }
        for p in &self.byok_cfg.providers {
            if !p.keys.is_empty() && !is_fetch_side(&p.name) && !choices.contains(&p.name) {
                choices.push(p.name.clone());
            }
        }
        for name in self.plugin_cfg.names() {
            if !choices.contains(name) {
                choices.push(name.clone());
            }
        }
        choices
    }

    fn current_default_provider(&self) -> String {
        if self.byok_cfg.default.is_empty() {
            if !self.draft_cfg.search.searxng_url.trim().is_empty() {
                "searxng".into()
            } else {
                "local".into()
            }
        } else {
            self.byok_cfg.default.clone()
        }
    }

    fn cycle_default_provider(&mut self, dir: isize) {
        let choices = self.available_default_providers();
        if choices.len() <= 1 {
            self.set_toast(
                "Add a provider key, plugin, or SearXNG URL first to switch default",
                false,
            );
            return;
        }
        let cur = self.current_default_provider();
        let idx = choices.iter().position(|c| c == &cur).unwrap_or(0) as isize;
        let next = (idx + dir).rem_euclid(choices.len() as isize) as usize;
        let chosen = choices[next].clone();
        self.byok_cfg.default = chosen.clone();
        self.byok_cfg.save();
        self.trigger_flash(self.selected);
        self.set_toast(format!("Default search provider → {chosen}"), false);
    }

    fn adjust_current(&mut self, dir: isize, coarse: bool) {
        let rows = self.rows();
        let Some(row) = rows.get(self.selected).cloned() else {
            return;
        };
        match row {
            Row::DefaultProvider => self.cycle_default_provider(dir),
            Row::Setting(spec) => match spec.control {
                ControlKind::Toggle => {
                    let cur = matches!(
                        schema::get_value(&self.draft_cfg, spec),
                        TomlEditValue::Bool(true)
                    );
                    let next = if dir == 0 { !cur } else { dir > 0 };
                    if let Err(e) = self.stage_setting(spec, TomlEditValue::Bool(next)) {
                        self.set_toast(e, true);
                    } else {
                        self.trigger_flash(self.selected);
                    }
                }
                ControlKind::SliderInt {
                    min,
                    max,
                    step,
                    coarse: cstep,
                    ..
                } => {
                    let cur = match schema::get_value(&self.draft_cfg, spec) {
                        TomlEditValue::Int(n) => n,
                        _ => min,
                    };
                    let delta = if coarse { cstep } else { step };
                    let next = (cur + (dir as i64) * delta).clamp(min, max);
                    if next != cur {
                        if let Err(e) = self.stage_setting(spec, TomlEditValue::Int(next)) {
                            self.set_toast(e, true);
                        } else {
                            self.trigger_flash(self.selected);
                        }
                    }
                }
                ControlKind::SliderFloat {
                    min,
                    max,
                    step,
                    coarse: cstep,
                } => {
                    let cur = match schema::get_value(&self.draft_cfg, spec) {
                        TomlEditValue::Float(f) => f,
                        _ => min,
                    };
                    let delta = if coarse { cstep } else { step };
                    let next =
                        (((cur + (dir as f64) * delta).clamp(min, max)) * 100.0).round() / 100.0;
                    if (next - cur).abs() > 1e-9 {
                        if let Err(e) = self.stage_setting(spec, TomlEditValue::Float(next)) {
                            self.set_toast(e, true);
                        } else {
                            self.trigger_flash(self.selected);
                        }
                    }
                }
                ControlKind::Enum(choices) => {
                    let cur = match schema::get_value(&self.draft_cfg, spec) {
                        TomlEditValue::Str(s) => s,
                        _ => choices.first().copied().unwrap_or_default().to_string(),
                    };
                    let idx = choices.iter().position(|c| *c == cur).unwrap_or(0) as isize;
                    let step = if dir == 0 { 1 } else { dir };
                    let next = (idx + step).rem_euclid(choices.len() as isize) as usize;
                    if let Err(e) =
                        self.stage_setting(spec, TomlEditValue::Str(choices[next].to_string()))
                    {
                        self.set_toast(e, true);
                    } else {
                        self.trigger_flash(self.selected);
                    }
                }
                ControlKind::Text | ControlKind::Url | ControlKind::Secret | ControlKind::List => {
                    if dir == 0 {
                        let cur = schema::get_value(&self.draft_cfg, spec);
                        self.modal = Some(Modal::EditSetting {
                            spec,
                            buffer: schema::editable_text(&cur),
                            revealed: !matches!(spec.control, ControlKind::Secret),
                            error: None,
                        });
                    }
                }
            },
            Row::RuleItem(pattern) if dir == 0 => {
                if let Some(mut rule) = self.draft_cfg.rules.url.get(&pattern).cloned() {
                    rule.enabled = !rule.enabled;
                    match config::upsert_file_rule(&pattern, &rule) {
                        Ok(_) => {
                            self.reload_disk_config();
                            self.trigger_flash(self.selected);
                            self.set_toast(
                                format!(
                                    "Rule `{pattern}` {}",
                                    if rule.enabled { "enabled" } else { "disabled" }
                                ),
                                false,
                            );
                        }
                        Err(e) => self.set_toast(format!("Rule update failed: {e}"), true),
                    }
                }
            }
            Row::Preset(preset) if dir == 0 => {
                self.apply_preset(preset);
            }
            _ => {}
        }
    }

    fn apply_preset(&mut self, preset: &Preset) {
        let mut applied = 0usize;
        for (section, key, raw) in preset.changes {
            if let Some(spec) = schema::find_setting(&format!("{section}.{key}")) {
                match schema::parse_value(spec, raw).and_then(|v| self.stage_setting(spec, v)) {
                    Ok(()) => applied += 1,
                    Err(e) => {
                        self.set_toast(e, true);
                        return;
                    }
                }
            }
        }
        self.trigger_flash(self.selected);
        self.set_toast(
            format!(
                "Staged preset \"{}\" ({applied} settings) — press s to save",
                preset.name
            ),
            false,
        );
    }

    fn activate_current(&mut self) {
        let rows = self.rows();
        let Some(row) = rows.get(self.selected).cloned() else {
            return;
        };
        match row {
            Row::Header(_) => {}
            Row::Setting(spec) => match spec.control {
                ControlKind::Toggle | ControlKind::Enum(_) => self.adjust_current(0, false),
                ControlKind::SliderInt { .. }
                | ControlKind::SliderFloat { .. }
                | ControlKind::Text
                | ControlKind::Url
                | ControlKind::Secret
                | ControlKind::List => {
                    let cur = schema::get_value(&self.draft_cfg, spec);
                    self.modal = Some(Modal::EditSetting {
                        spec,
                        buffer: schema::editable_text(&cur),
                        revealed: !matches!(spec.control, ControlKind::Secret),
                        error: None,
                    });
                }
            },
            Row::DefaultProvider => self.cycle_default_provider(1),
            Row::Provider(provider) => {
                self.modal = Some(Modal::AddProviderKey {
                    provider,
                    buffer: String::new(),
                    revealed: false,
                    error: None,
                });
            }
            Row::AddPlugin => {
                self.modal = Some(Modal::AddPlugin {
                    step: 0,
                    name: String::new(),
                    cmd: String::new(),
                    timeout_secs: (DEFAULT_TIMEOUT_MS / 1000).to_string(),
                    error: None,
                });
            }
            Row::Plugin(name) => {
                self.plugin_cfg.mark_state(&name, KeyState::Active);
                self.plugin_cfg.save();
                self.trigger_flash(self.selected);
                self.set_toast(format!("Reset plugin `{name}` state to active"), false);
            }
            Row::AddProxy => {
                self.modal = Some(Modal::AddProxy {
                    buffer: String::new(),
                    error: None,
                });
            }
            Row::ProxyItem(idx) => {
                self.start_proxy_probe(idx);
            }
            Row::AddRule => {
                self.modal = Some(Modal::AddRule {
                    step: 0,
                    pattern: String::new(),
                    action_deny: true,
                    message: String::new(),
                    kind_permanent: false,
                    reason: String::new(),
                    error: None,
                });
            }
            Row::TestRule => {
                self.modal = Some(Modal::TestRuleUrl {
                    buffer: "https://".into(),
                    result: None,
                });
            }
            Row::RuleItem(_) => self.adjust_current(0, false),
            Row::Preset(preset) => self.apply_preset(preset),
        }
    }

    fn start_proxy_probe(&mut self, idx: usize) {
        if self.pending_probe.is_some() {
            self.set_toast("Already probing a proxy...", false);
            return;
        }
        let Some(px) = self.proxies.get(idx).cloned() else {
            return;
        };
        let proxy_id = px.id();
        self.set_toast(format!("Probing {proxy_id}..."), false);
        let handle = tokio::spawn(async move { crate::cli::proxy::probe_proxy(&px).await });
        self.pending_probe = Some(PendingProbe { proxy_id, handle });
    }

    fn poll_pending_probe(&mut self) {
        let finished = self
            .pending_probe
            .as_ref()
            .is_some_and(|p| p.handle.is_finished());
        if !finished {
            return;
        }
        let Some(pending) = self.pending_probe.take() else {
            return;
        };
        if let Some(res) = futures_util::FutureExt::now_or_never(pending.handle) {
            match res {
                Ok(pr) => {
                    let ms = pr.latency.as_millis();
                    if pr.alive {
                        let ip = pr.exit_ip.unwrap_or_else(|| "ok".into());
                        let label = format!("{ms}ms · {ip}");
                        self.proxy_probes
                            .insert(pending.proxy_id.clone(), (true, label.clone()));
                        self.set_toast(
                            format!("Proxy {} alive ({label})", pending.proxy_id),
                            false,
                        );
                    } else {
                        let err = pr.error.unwrap_or_else(|| "unreachable".into());
                        let short = term::truncate_plain(&err, 36);
                        self.proxy_probes
                            .insert(pending.proxy_id.clone(), (false, short.clone()));
                        self.set_toast(format!("Proxy {} failed: {short}", pending.proxy_id), true);
                    }
                }
                Err(_) => {
                    self.set_toast(format!("Probe task aborted for {}", pending.proxy_id), true);
                }
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        // Any keypress skips the intro sweep immediately.
        self.boot_frames = 0;

        if self.modal.is_some() {
            self.handle_modal_key(key);
            return;
        }

        if self.search_mode {
            match key {
                KeyEvent::Esc => {
                    self.search_mode = false;
                    self.search_query.clear();
                    self.selected = 0;
                    self.scroll = 0;
                    self.snap_selection_to_selectable();
                }
                KeyEvent::Enter => {
                    self.search_mode = false;
                    self.snap_selection_to_selectable();
                }
                KeyEvent::Backspace => {
                    self.search_query.pop();
                    self.selected = 0;
                    self.scroll = 0;
                    self.snap_selection_to_selectable();
                }
                KeyEvent::Up => self.move_selection(-1),
                KeyEvent::Down => self.move_selection(1),
                KeyEvent::Char(c) if !c.is_control() => {
                    self.search_query.push(c);
                    self.selected = 0;
                    self.scroll = 0;
                    self.snap_selection_to_selectable();
                }
                _ => {}
            }
            return;
        }

        match key {
            KeyEvent::Ctrl('c') => {
                self.should_quit = true;
            }
            KeyEvent::Char('q') => {
                if !self.staged.is_empty() {
                    self.modal = Some(Modal::ConfirmQuit);
                } else {
                    self.should_quit = true;
                }
            }
            KeyEvent::Esc => {
                if !self.search_query.is_empty() {
                    self.search_query.clear();
                    self.selected = 0;
                    self.scroll = 0;
                    self.snap_selection_to_selectable();
                } else if !self.staged.is_empty() {
                    self.modal = Some(Modal::ConfirmQuit);
                } else {
                    self.should_quit = true;
                }
            }
            KeyEvent::Tab | KeyEvent::Char(']') => {
                self.search_query.clear();
                self.switch_tab(self.active_tab.next());
            }
            KeyEvent::BackTab | KeyEvent::Char('[') => {
                self.search_query.clear();
                self.switch_tab(self.active_tab.prev());
            }
            KeyEvent::Char(c @ '1'..='8') => {
                self.search_query.clear();
                let idx = (c as u8 - b'1') as usize;
                if let Some(&tab) = TabId::ALL.get(idx) {
                    self.switch_tab(tab);
                }
            }
            KeyEvent::Up | KeyEvent::Char('k') => self.move_selection(-1),
            KeyEvent::Down | KeyEvent::Char('j') => self.move_selection(1),
            KeyEvent::PageUp => {
                for _ in 0..8 {
                    self.move_selection(-1);
                }
            }
            KeyEvent::PageDown => {
                for _ in 0..8 {
                    self.move_selection(1);
                }
            }
            KeyEvent::Home | KeyEvent::Char('g') => {
                self.selected = 0;
                self.scroll = 0;
                self.snap_selection_to_selectable();
            }
            KeyEvent::End | KeyEvent::Char('G') => {
                let len = self.rows().len();
                self.selected = len.saturating_sub(1);
                self.snap_selection_to_selectable();
            }
            KeyEvent::Left | KeyEvent::Char('h') => self.adjust_current(-1, false),
            KeyEvent::Right | KeyEvent::Char('l') => self.adjust_current(1, false),
            KeyEvent::ShiftLeft | KeyEvent::Char('-') => self.adjust_current(-1, true),
            KeyEvent::ShiftRight | KeyEvent::Char('+') | KeyEvent::Char('=') => {
                self.adjust_current(1, true);
            }
            KeyEvent::Char(' ') => self.adjust_current(0, false),
            KeyEvent::Enter => self.activate_current(),
            KeyEvent::Char('/') | KeyEvent::Ctrl('f') => {
                self.search_mode = true;
            }
            KeyEvent::Char('s') | KeyEvent::Ctrl('s') => {
                self.save_staged();
            }
            KeyEvent::Char('?') => {
                self.modal = Some(Modal::Help);
            }
            KeyEvent::Char('u') => {
                let rows = self.rows();
                if let Some(Row::Setting(spec)) = rows.get(self.selected).cloned() {
                    if let Err(e) = self.stage_setting(spec, TomlEditValue::Unset) {
                        self.set_toast(e, true);
                    } else {
                        self.trigger_flash(self.selected);
                        self.set_toast(
                            format!(
                                "Staged reset to default for {} (press s to save)",
                                spec.full_key()
                            ),
                            false,
                        );
                    }
                }
            }
            KeyEvent::Char('a') => {
                let rows = self.rows();
                match rows.get(self.selected) {
                    Some(Row::Provider(p)) => {
                        self.modal = Some(Modal::AddProviderKey {
                            provider: p,
                            buffer: String::new(),
                            revealed: false,
                            error: None,
                        });
                    }
                    _ => match self.active_tab {
                        TabId::Providers => {
                            self.modal = Some(Modal::AddPlugin {
                                step: 0,
                                name: String::new(),
                                cmd: String::new(),
                                timeout_secs: (DEFAULT_TIMEOUT_MS / 1000).to_string(),
                                error: None,
                            });
                        }
                        TabId::Proxies => {
                            self.modal = Some(Modal::AddProxy {
                                buffer: String::new(),
                                error: None,
                            });
                        }
                        TabId::Rules => {
                            self.modal = Some(Modal::AddRule {
                                step: 0,
                                pattern: String::new(),
                                action_deny: true,
                                message: String::new(),
                                kind_permanent: false,
                                reason: String::new(),
                                error: None,
                            });
                        }
                        _ => {}
                    },
                }
            }
            KeyEvent::Char('r') => {
                let rows = self.rows();
                match rows.get(self.selected).cloned() {
                    Some(Row::Provider(provider)) => {
                        self.byok_cfg.reset_states(Some(provider));
                        self.byok_cfg.save();
                        self.trigger_flash(self.selected);
                        self.set_toast(format!("Reset all {provider} key states to active"), false);
                    }
                    Some(Row::Plugin(name)) => {
                        self.plugin_cfg.mark_state(&name, KeyState::Active);
                        self.plugin_cfg.save();
                        self.trigger_flash(self.selected);
                        self.set_toast(format!("Reset plugin `{name}` state to active"), false);
                    }
                    _ => {}
                }
            }
            KeyEvent::Char('t') => {
                let rows = self.rows();
                match rows.get(self.selected) {
                    Some(Row::ProxyItem(idx)) => self.start_proxy_probe(*idx),
                    _ if self.active_tab == TabId::Rules => {
                        self.modal = Some(Modal::TestRuleUrl {
                            buffer: "https://".into(),
                            result: None,
                        });
                    }
                    _ => {}
                }
            }
            KeyEvent::Char('c')
                if self.active_tab == TabId::Proxies && !self.proxies.is_empty() =>
            {
                self.modal = Some(Modal::ConfirmClearProxies {
                    count: self.proxies.len(),
                });
            }
            KeyEvent::Char('d') | KeyEvent::Delete => {
                let rows = self.rows();
                match rows.get(self.selected).cloned() {
                    Some(Row::Provider(provider)) => {
                        let count = self
                            .byok_cfg
                            .providers
                            .iter()
                            .find(|p| p.name == provider)
                            .map(|p| p.keys.len())
                            .unwrap_or(0);
                        if count == 0 {
                            self.set_toast(format!("No keys configured for {provider}"), false);
                        } else {
                            self.modal = Some(Modal::ConfirmRemoveProvider { provider, count });
                        }
                    }
                    Some(Row::Plugin(name)) => {
                        self.modal = Some(Modal::ConfirmRemovePlugin { name });
                    }
                    Some(Row::ProxyItem(idx)) => {
                        if idx < self.proxies.len() {
                            let removed = self.proxies.remove(idx);
                            match proxy::save_config(&self.proxies) {
                                Ok(()) => {
                                    self.proxy_probes.remove(&removed.id());
                                    self.snap_selection_to_selectable();
                                    self.set_toast(
                                        format!("Removed proxy {} from pool", removed.id()),
                                        false,
                                    );
                                }
                                Err(e) => {
                                    self.proxies.insert(idx, removed);
                                    self.set_toast(format!("Failed saving proxies: {e}"), true);
                                }
                            }
                        }
                    }
                    Some(Row::RuleItem(pattern)) => match config::remove_file_rule(&pattern) {
                        Ok(_) => {
                            self.reload_disk_config();
                            self.snap_selection_to_selectable();
                            self.set_toast(format!("Removed rule `{pattern}`"), false);
                        }
                        Err(e) => self.set_toast(format!("Failed removing rule: {e}"), true),
                    },
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn handle_modal_key(&mut self, key: KeyEvent) {
        let Some(mut modal) = self.modal.take() else {
            return;
        };
        match &mut modal {
            Modal::Help => {
                // Any key closes help (modal was already taken above).
            }
            Modal::ConfirmQuit => {
                match key {
                    KeyEvent::Char('s') | KeyEvent::Char('S') | KeyEvent::Enter => {
                        if self.save_staged() {
                            self.should_quit = true;
                        }
                        return;
                    }
                    KeyEvent::Char('d') | KeyEvent::Char('D') => {
                        self.staged.clear();
                        self.should_quit = true;
                        return;
                    }
                    KeyEvent::Esc | KeyEvent::Char('c') | KeyEvent::Char('q') => return,
                    _ => {}
                }
                self.modal = Some(modal);
            }
            Modal::EditSetting {
                spec,
                buffer,
                revealed,
                error,
            } => {
                match key {
                    KeyEvent::Esc => return,
                    KeyEvent::Ctrl('r') => *revealed = !*revealed,
                    KeyEvent::Backspace => {
                        buffer.pop();
                        *error = None;
                    }
                    KeyEvent::Enter => {
                        let parsed = schema::parse_value(spec, buffer);
                        match parsed.and_then(|v| self.stage_setting(spec, v)) {
                            Ok(()) => {
                                self.trigger_flash(self.selected);
                                self.set_toast(
                                    format!("Updated {} (press s to save)", spec.full_key()),
                                    false,
                                );
                                return;
                            }
                            Err(e) => *error = Some(e),
                        }
                    }
                    KeyEvent::Char(c) if !c.is_control() => {
                        buffer.push(c);
                        *error = None;
                    }
                    _ => {}
                }
                self.modal = Some(modal);
            }
            Modal::AddProviderKey {
                provider,
                buffer,
                revealed,
                error,
            } => {
                match key {
                    KeyEvent::Esc => return,
                    KeyEvent::Ctrl('r') => *revealed = !*revealed,
                    KeyEvent::Backspace => {
                        buffer.pop();
                        *error = None;
                    }
                    KeyEvent::Enter => {
                        let trimmed = buffer.trim();
                        if trimmed.is_empty() {
                            *error = Some("API key cannot be empty".into());
                        } else if *provider == "unlocker"
                            && let Err(e) = crate::fetch::bypass::parse_key(
                                trimmed,
                                crate::fetch::bypass::DEFAULT_ZONE,
                            )
                        {
                            *error = Some(e.to_string());
                        } else if *provider == "brightdata"
                            && let Err(e) = crate::search::byok::brightdata_key_parts(trimmed)
                        {
                            *error = Some(e);
                        } else {
                            let prov = *provider;
                            self.byok_cfg.add_key(prov, trimmed);
                            self.byok_cfg.save();
                            self.trigger_flash(self.selected);
                            self.set_toast(
                                format!("Added key for {prov} (saved 0600 to byok-keys.json)"),
                                false,
                            );
                            return;
                        }
                    }
                    KeyEvent::Char(c) if !c.is_control() => {
                        buffer.push(c);
                        *error = None;
                    }
                    _ => {}
                }
                self.modal = Some(modal);
            }
            Modal::ConfirmRemoveProvider { provider, .. } => {
                match key {
                    KeyEvent::Char('y') | KeyEvent::Char('Y') | KeyEvent::Enter => {
                        let prov = *provider;
                        self.byok_cfg.remove_keys(prov, None);
                        self.byok_cfg.save();
                        self.set_toast(format!("Removed all keys for {prov}"), false);
                        return;
                    }
                    KeyEvent::Esc | KeyEvent::Char('n') | KeyEvent::Char('N') => return,
                    _ => {}
                }
                self.modal = Some(modal);
            }
            Modal::AddPlugin {
                step,
                name,
                cmd,
                timeout_secs,
                error,
            } => {
                match key {
                    KeyEvent::Esc => return,
                    KeyEvent::Tab => {
                        *step = (*step + 1) % 3;
                        *error = None;
                    }
                    KeyEvent::BackTab => {
                        *step = (*step + 2) % 3;
                        *error = None;
                    }
                    KeyEvent::Backspace => {
                        match *step {
                            0 => {
                                name.pop();
                            }
                            1 => {
                                cmd.pop();
                            }
                            _ => {
                                timeout_secs.pop();
                            }
                        }
                        *error = None;
                    }
                    KeyEvent::Enter => {
                        if *step < 2 {
                            *step += 1;
                            *error = None;
                        } else {
                            let pname = name.trim().to_ascii_lowercase();
                            let keyed: std::collections::HashSet<String> = self
                                .byok_cfg
                                .providers
                                .iter()
                                .map(|p| p.name.clone())
                                .collect();
                            let res = validate_plugin_name(&pname, &keyed).and_then(|()| {
                                let argv = tokenize_cmd(cmd.trim())?;
                                let secs = timeout_secs
                                    .trim()
                                    .parse::<u64>()
                                    .ok()
                                    .filter(|&s| s > 0)
                                    .ok_or_else(|| {
                                        "timeout must be a positive integer (seconds)".to_string()
                                    })?;
                                let ms = secs.saturating_mul(1000);
                                self.plugin_cfg.add(&pname, argv, ms, &keyed)?;
                                self.plugin_cfg.save();
                                if self.byok_cfg.default.is_empty() {
                                    self.byok_cfg.default = pname.clone();
                                    self.byok_cfg.save();
                                }
                                Ok(())
                            });
                            match res {
                                Ok(()) => {
                                    self.set_toast(
                                        format!("Registered search plugin `{pname}`"),
                                        false,
                                    );
                                    return;
                                }
                                Err(e) => *error = Some(e),
                            }
                        }
                    }
                    KeyEvent::Char(c) if !c.is_control() => {
                        match *step {
                            0 => name.push(c),
                            1 => cmd.push(c),
                            _ => {
                                if c.is_ascii_digit() {
                                    timeout_secs.push(c);
                                }
                            }
                        }
                        *error = None;
                    }
                    _ => {}
                }
                self.modal = Some(modal);
            }
            Modal::ConfirmRemovePlugin { name } => {
                match key {
                    KeyEvent::Char('y') | KeyEvent::Char('Y') | KeyEvent::Enter => {
                        let pname = name.clone();
                        self.plugin_cfg.remove(&pname);
                        self.plugin_cfg.save();
                        if self.byok_cfg.default == pname {
                            self.byok_cfg.default = "local".into();
                            self.byok_cfg.save();
                        }
                        self.snap_selection_to_selectable();
                        self.set_toast(format!("Removed plugin `{pname}`"), false);
                        return;
                    }
                    KeyEvent::Esc | KeyEvent::Char('n') | KeyEvent::Char('N') => return,
                    _ => {}
                }
                self.modal = Some(modal);
            }
            Modal::AddProxy { buffer, error } => {
                match key {
                    KeyEvent::Esc => return,
                    KeyEvent::Backspace => {
                        buffer.pop();
                        *error = None;
                    }
                    KeyEvent::Enter => match Proxy::parse(buffer.trim()) {
                        Ok(px) => {
                            let id = px.id();
                            if self.proxies.iter().any(|existing| existing.id() == id) {
                                *error = Some(format!("Proxy {id} is already in the pool"));
                            } else {
                                self.proxies.push(px);
                                match proxy::save_config(&self.proxies) {
                                    Ok(()) => {
                                        self.set_toast(
                                            format!("Added proxy {id} (press t to probe live)"),
                                            false,
                                        );
                                        return;
                                    }
                                    Err(e) => {
                                        self.proxies.pop();
                                        *error = Some(format!("Failed saving proxies: {e}"));
                                    }
                                }
                            }
                        }
                        Err(e) => *error = Some(format!("Invalid proxy URL: {e}")),
                    },
                    KeyEvent::Char(c) if !c.is_control() => {
                        buffer.push(c);
                        *error = None;
                    }
                    _ => {}
                }
                self.modal = Some(modal);
            }
            Modal::ConfirmClearProxies { .. } => {
                match key {
                    KeyEvent::Char('y') | KeyEvent::Char('Y') | KeyEvent::Enter => {
                        match proxy::save_config(&[]) {
                            Ok(()) => {
                                self.proxies.clear();
                                self.proxy_probes.clear();
                                self.snap_selection_to_selectable();
                                self.set_toast("Cleared all pool proxies", false);
                                return;
                            }
                            Err(e) => {
                                self.set_toast(format!("Failed clearing pool: {e}"), true);
                                return;
                            }
                        }
                    }
                    KeyEvent::Esc | KeyEvent::Char('n') | KeyEvent::Char('N') => return,
                    _ => {}
                }
                self.modal = Some(modal);
            }
            Modal::AddRule {
                step,
                pattern,
                action_deny,
                message,
                kind_permanent,
                reason,
                error,
            } => {
                match key {
                    KeyEvent::Esc => return,
                    KeyEvent::Tab => {
                        *step = (*step + 1) % 5;
                        *error = None;
                    }
                    KeyEvent::BackTab => {
                        *step = (*step + 4) % 5;
                        *error = None;
                    }
                    KeyEvent::Left | KeyEvent::Right | KeyEvent::Char(' ')
                        if *step == 1 || *step == 3 =>
                    {
                        if *step == 1 {
                            *action_deny = !*action_deny;
                        } else {
                            *kind_permanent = !*kind_permanent;
                        }
                        *error = None;
                    }
                    KeyEvent::Backspace => {
                        match *step {
                            0 => {
                                pattern.pop();
                            }
                            2 => {
                                message.pop();
                            }
                            4 => {
                                reason.pop();
                            }
                            _ => {}
                        }
                        *error = None;
                    }
                    KeyEvent::Enter => {
                        if *step < 4 {
                            *step += 1;
                            *error = None;
                        } else {
                            let pat = pattern.trim().to_string();
                            let rule = UrlRule {
                                enabled: true,
                                action: if *action_deny {
                                    RuleAction::Deny
                                } else {
                                    RuleAction::Allow
                                },
                                message: (!message.trim().is_empty())
                                    .then(|| message.trim().to_string()),
                                kind: if *kind_permanent {
                                    DenyKind::Permanent
                                } else {
                                    DenyKind::Walled
                                },
                                reason: (!reason.trim().is_empty())
                                    .then(|| reason.trim().to_string()),
                            };
                            match config::upsert_file_rule(&pat, &rule) {
                                Ok(_) => {
                                    self.reload_disk_config();
                                    self.set_toast(
                                        format!("Saved rule `{pat}` to donsetch.toml"),
                                        false,
                                    );
                                    return;
                                }
                                Err(e) => *error = Some(e),
                            }
                        }
                    }
                    KeyEvent::Char(c) if !c.is_control() => {
                        match *step {
                            0 => pattern.push(c),
                            2 => message.push(c),
                            4 => reason.push(c),
                            _ => {}
                        }
                        *error = None;
                    }
                    _ => {}
                }
                self.modal = Some(modal);
            }
            Modal::TestRuleUrl { buffer, result } => {
                match key {
                    KeyEvent::Esc => return,
                    KeyEvent::Backspace => {
                        buffer.pop();
                        *result = None;
                    }
                    KeyEvent::Enter => {
                        let raw = buffer.trim();
                        match url::Url::parse(raw) {
                            Err(e) => *result = Some(format!("Invalid URL: {e}")),
                            Ok(u) => match RuleSet::compile(&self.draft_cfg.rules) {
                                Err(e) => *result = Some(format!("Rule compile error: {e}")),
                                Ok(rs) => {
                                    if self.draft_cfg.rules.mode == crate::rules::RulesMode::Off {
                                        *result =
                                            Some("ALLOWED (rules.mode is currently off)".into());
                                    } else if let Some(rule) = rs.eval(&u) {
                                        if let Some(denial) = rule.denial() {
                                            *result = Some(format!(
                                                "DENIED by `{}` [{} / {}]: {}",
                                                denial.rule,
                                                denial.code(),
                                                denial.kind,
                                                denial.message
                                            ));
                                        } else {
                                            *result = Some(format!(
                                                "ALLOWED by explicit allow rule `{}`",
                                                rule.key
                                            ));
                                        }
                                    } else {
                                        *result = Some("ALLOWED (no matching rule)".into());
                                    }
                                }
                            },
                        }
                    }
                    KeyEvent::Char(c) if !c.is_control() => {
                        buffer.push(c);
                        *result = None;
                    }
                    _ => {}
                }
                self.modal = Some(modal);
            }
        }
    }

    fn render(&mut self, cols: usize, rows_h: usize) -> Vec<String> {
        let mut frame = Vec::with_capacity(rows_h);
        let inner_w = cols.saturating_sub(2);

        // ── 1. Animated Crimson-Noir Header ──
        let scanner = term::comet_scanner(self.tick / 2);
        let title = format!(
            "  {} {}  {}",
            term::fg_crimson("\u{25C6}"),
            term::fg_white_bold("DonSeTch Settings"),
            term::fg_dim(&format!("v{}", env!("CARGO_PKG_VERSION")))
        );
        let status_pill = if let Some(probe) = &self.pending_probe {
            term::pill_badge_warn(&format!(" PROBING {} ", probe.proxy_id))
        } else if !self.staged.is_empty() {
            let pulse = (self.tick / 8).is_multiple_of(2);
            let txt = format!(" \u{25CF} {} UNSAVED (s to save) ", self.staged.len());
            if pulse {
                term::pill_badge_warn(&txt)
            } else {
                term::pill_badge_crimson(&txt)
            }
        } else {
            term::pill_badge_ok(" \u{2713} SYNCED ")
        };
        let left_hdr = format!("{title}   {scanner}");
        let gap =
            cols.saturating_sub(term::visible_len(&left_hdr) + term::visible_len(&status_pill) + 2);
        frame.push(format!("{left_hdr}{}{status_pill}", " ".repeat(gap)));

        // Animated sweep divider (intro laser sweep + tab-switch accent).
        let sweep_pos = if self.boot_frames > 0 {
            let progress = 14usize.saturating_sub(self.boot_frames);
            Some((progress * inner_w) / 14)
        } else if self.tab_sweep > 0 {
            let progress = 6usize.saturating_sub(self.tab_sweep);
            Some((progress * inner_w) / 6)
        } else {
            None
        };
        frame.push(format!(
            "  {}",
            term::sweep_rule(inner_w.saturating_sub(2), sweep_pos)
        ));

        // ── 2. Category Tab Strip + Live Search Pill ──
        let mut tab_line = String::from("  ");
        for (i, &tab) in TabId::ALL.iter().enumerate() {
            let label = format!(" {} {} ", i + 1, tab.label());
            let pill = if self.search_query.is_empty() && self.active_tab == tab {
                term::pill_active_tab(&label)
            } else {
                term::pill_inactive_tab(&label)
            };
            if i > 0 {
                tab_line.push(' ');
            }
            tab_line.push_str(&pill);
        }
        let search_badge = if self.search_mode {
            let cur = if (self.tick / 6).is_multiple_of(2) {
                "\u{258C}"
            } else {
                " "
            };
            format!(
                "  {}",
                term::pill_badge_crimson(&format!(" / {}{cur} ", self.search_query))
            )
        } else if !self.search_query.is_empty() {
            format!(
                "  {}",
                term::pill_badge_warn(&format!(" / \"{}\" (Esc) ", self.search_query))
            )
        } else {
            format!("  {}", term::fg_dim("[/] filter"))
        };
        if term::visible_len(&tab_line) + term::visible_len(&search_badge) + 2 <= cols {
            tab_line.push_str(&search_badge);
        }
        frame.push(tab_line);

        let sub = if !self.search_query.is_empty() {
            format!(
                "Filtering across all tabs for \"{}\" — Esc clears filter",
                self.search_query
            )
        } else {
            self.active_tab.subtitle().to_string()
        };
        frame.push(format!(
            "  {} {}",
            term::fg_crimson("\u{25B8}"),
            term::fg_dim(&term::truncate_plain(&sub, inner_w.saturating_sub(4)))
        ));

        // ── 3. Main Workspace (Two-column on >=98 cols, stacked on narrow) ──
        let list_rows = self.rows();
        let footer_h = 3usize;
        let header_h = frame.len();
        let body_h = rows_h.saturating_sub(header_h + footer_h).max(6);

        let split = cols >= 98;
        let (list_h, left_w, right_w) = if split {
            let rw = (cols * 38 / 100).clamp(34, 48);
            let lw = cols.saturating_sub(rw + 5);
            (body_h, lw, rw)
        } else {
            let drawer_h = 6.min(body_h / 3);
            (
                body_h.saturating_sub(drawer_h + 1),
                cols.saturating_sub(4),
                cols.saturating_sub(4),
            )
        };

        // Keep selection centered/visible within scroll window.
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + list_h {
            self.scroll = self.selected + 1 - list_h;
        }
        if self.scroll > list_rows.len().saturating_sub(list_h) {
            self.scroll = list_rows.len().saturating_sub(list_h);
        }

        let mut left_lines: Vec<String> = Vec::with_capacity(list_h);
        if list_rows.is_empty() {
            left_lines.push(format!("  {}", term::fg_dim("No matching settings.")));
        } else {
            let end = (self.scroll + list_h).min(list_rows.len());
            for (i, row) in list_rows.iter().enumerate().take(end).skip(self.scroll) {
                let is_sel = i == self.selected;
                let is_flash = self.flash.is_some_and(|(fi, _)| fi == i);
                left_lines.push(self.render_row(row, left_w, is_sel, is_flash));
            }
        }
        while left_lines.len() < list_h {
            left_lines.push(String::new());
        }

        let inspector_lines = self.render_inspector(list_rows.get(self.selected), right_w, body_h);

        if split {
            for r in 0..body_h {
                let l = left_lines.get(r).map(String::as_str).unwrap_or("");
                let right = inspector_lines.get(r).map(String::as_str).unwrap_or("");
                let padded_l = term::pad_right_styled(l, left_w);
                frame.push(format!(
                    "  {padded_l} {} {right}",
                    term::fg_muted("\u{2502}")
                ));
            }
        } else {
            for l in left_lines {
                frame.push(format!("  {l}"));
            }
            frame.push(format!(
                "  {}",
                term::fg_muted(&"\u{2500}".repeat(inner_w.saturating_sub(2)))
            ));
            let drawer_h = body_h.saturating_sub(list_h + 1);
            for r in 0..drawer_h {
                let line = inspector_lines.get(r).map(String::as_str).unwrap_or("");
                frame.push(format!("  {line}"));
            }
        }

        // ── 4. Toast & Keybind Footer ──
        frame.push(format!(
            "  {}",
            term::fg_muted(&"\u{2500}".repeat(inner_w.saturating_sub(2)))
        ));
        if let Some((msg, is_err, _)) = &self.toast {
            let icon = if *is_err {
                term::fg_crimson_bold("\u{2717}")
            } else {
                term::fg_emerald("\u{2713}")
            };
            let styled = if *is_err {
                term::fg_ember(&term::truncate_plain(msg, inner_w.saturating_sub(6)))
            } else {
                term::fg_gold(&term::truncate_plain(msg, inner_w.saturating_sub(6)))
            };
            frame.push(format!("  {icon} {styled}"));
        } else {
            let cfg_hint = config::config_file_target()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "(config file disabled)".into());
            frame.push(format!(
                "  {} {}",
                term::fg_dim("config:"),
                term::fg_soft(&term::truncate_plain(&cfg_hint, inner_w.saturating_sub(12)))
            ));
        }

        let keys_help = if self.search_mode {
            "Type to filter · ↑↓ move · Enter lock filter · Esc clear"
        } else {
            "↑↓ move · ←→ adjust · Shift+←→ fast · Space toggle · Enter edit · / search · u reset · s save · ? help · q quit"
        };
        frame.push(format!(
            "  {}",
            term::fg_dim(&term::truncate_plain(keys_help, inner_w.saturating_sub(2)))
        ));

        // ── 5. Modal Overlay (replaces center lines cleanly) ──
        if let Some(modal) = &self.modal {
            self.overlay_modal(&mut frame, modal, cols, rows_h);
        }

        frame
    }

    fn render_row(&self, row: &Row, width: usize, selected: bool, flash: bool) -> String {
        match row {
            Row::Header(title) => {
                let text = format!(
                    "\u{2500}\u{2500} {} ",
                    term::truncate_plain(title, width.saturating_sub(6))
                );
                let pad = width.saturating_sub(text.chars().count());
                format!(
                    "{}{}",
                    term::fg_ember(&text),
                    term::fg_muted(&"\u{2500}".repeat(pad))
                )
            }
            Row::Setting(spec) => {
                let val = schema::get_value(&self.draft_cfg, spec);
                let dirty = self
                    .staged
                    .contains_key(&(spec.section.to_string(), spec.key.to_string()));
                let marker = if selected {
                    term::fg_crimson_bold("\u{25B8} ")
                } else if dirty {
                    term::fg_gold("\u{25CF} ")
                } else {
                    "  ".to_string()
                };
                let control_str = self.render_control(spec, &val, flash, width >= 54);
                let ctrl_w = term::visible_len(&control_str);
                let label_budget = width.saturating_sub(ctrl_w + 4).max(12);
                let label_plain = term::truncate_plain(spec.label, label_budget);
                let label_styled = if selected {
                    term::fg_white_bold(&label_plain)
                } else if dirty {
                    term::fg_gold(&label_plain)
                } else {
                    term::fg_soft(&label_plain)
                };
                let left = format!("{marker}{label_styled}");
                let gap = width
                    .saturating_sub(term::visible_len(&left) + ctrl_w)
                    .max(1);
                let line = format!("{left}{}{control_str}", " ".repeat(gap));
                if selected {
                    term::bg_row_selected(&term::pad_right_styled(&line, width))
                } else {
                    line
                }
            }
            Row::DefaultProvider => {
                let cur = self.current_default_provider();
                let marker = if selected {
                    term::fg_crimson_bold("\u{25B8} ")
                } else {
                    "  ".to_string()
                };
                let chip = if flash {
                    term::pill_badge_warn(&format!(" \u{2039} {cur} \u{203A} "))
                } else if cur == "local" {
                    term::fg_cyan(&format!("\u{2039} {cur} (keyless first) \u{203A}"))
                } else {
                    term::fg_emerald(&format!("\u{2039} {cur} (provider first) \u{203A}"))
                };
                let label = if selected {
                    term::fg_white_bold("Default Search Provider")
                } else {
                    term::fg_soft("Default Search Provider")
                };
                let left = format!("{marker}{label}");
                let gap = width
                    .saturating_sub(term::visible_len(&left) + term::visible_len(&chip))
                    .max(1);
                let line = format!("{left}{}{chip}", " ".repeat(gap));
                if selected {
                    term::bg_row_selected(&term::pad_right_styled(&line, width))
                } else {
                    line
                }
            }
            Row::Provider(provider) => {
                let marker = if selected {
                    term::fg_crimson_bold("\u{25B8} ")
                } else {
                    "  ".to_string()
                };
                let entry = self.byok_cfg.providers.iter().find(|p| p.name == *provider);
                let status = match entry {
                    Some(pc) if !pc.keys.is_empty() => {
                        let active = pc
                            .keys
                            .iter()
                            .filter(|k| k.state == KeyState::Active)
                            .count();
                        let cool = pc
                            .keys
                            .iter()
                            .filter(|k| k.state == KeyState::RateLimited)
                            .count();
                        let dead = pc.keys.len() - active - cool;
                        let mut parts = Vec::new();
                        if active > 0 {
                            parts.push(term::fg_emerald(&format!("\u{2713} {active} active")));
                        }
                        if cool > 0 {
                            parts.push(term::fg_gold(&format!("\u{23F1} {cool} cooling")));
                        }
                        if dead > 0 {
                            parts.push(term::fg_crimson(&format!("\u{2717} {dead} dead")));
                        }
                        parts.join(" ")
                    }
                    _ => term::fg_dim("not configured  [Enter: add]"),
                };
                let role = if is_fetch_side(provider) {
                    format!("{provider} (fetch bypass)")
                } else {
                    (*provider).to_string()
                };
                let label = if selected {
                    term::fg_white_bold(&role)
                } else {
                    term::fg_soft(&role)
                };
                let left = format!("{marker}{label}");
                let gap = width
                    .saturating_sub(term::visible_len(&left) + term::visible_len(&status))
                    .max(1);
                let line = format!("{left}{}{status}", " ".repeat(gap));
                if selected {
                    term::bg_row_selected(&term::pad_right_styled(&line, width))
                } else {
                    line
                }
            }
            Row::AddPlugin | Row::AddProxy | Row::AddRule | Row::TestRule => {
                let (icon, label, tag) = match row {
                    Row::AddPlugin => ("+", "Register Custom Search Plugin", "[Enter]"),
                    Row::AddProxy => ("+", "Add Proxy to Pool (SOCKS5 / HTTP)", "[Enter]"),
                    Row::AddRule => ("+", "Add Per-Host Deny / Allow Rule", "[Enter]"),
                    Row::TestRule => ("\u{25C7}", "Test Any URL Against Active Rules", "[Enter]"),
                    _ => unreachable!(),
                };
                let marker = if selected {
                    term::fg_crimson_bold("\u{25B8} ")
                } else {
                    "  ".to_string()
                };
                let left = format!(
                    "{marker}{} {}",
                    term::fg_emerald(icon),
                    term::fg_white_bold(label)
                );
                let right = term::fg_cyan(tag);
                let gap = width
                    .saturating_sub(term::visible_len(&left) + term::visible_len(&right))
                    .max(1);
                let line = format!("{left}{}{right}", " ".repeat(gap));
                if selected {
                    term::bg_row_selected(&term::pad_right_styled(&line, width))
                } else {
                    line
                }
            }
            Row::Plugin(name) => {
                let marker = if selected {
                    term::fg_crimson_bold("\u{25B8} ")
                } else {
                    "  ".to_string()
                };
                let detail = self
                    .plugin_cfg
                    .plugins
                    .get(name)
                    .map(|d| format!("{} · {}s", d.state.label(), d.timeout_ms / 1000))
                    .unwrap_or_default();
                let left = format!(
                    "{marker}{}",
                    if selected {
                        term::fg_white_bold(name)
                    } else {
                        term::fg_soft(name)
                    }
                );
                let right = term::fg_cyan(&detail);
                let gap = width
                    .saturating_sub(term::visible_len(&left) + term::visible_len(&right))
                    .max(1);
                let line = format!("{left}{}{right}", " ".repeat(gap));
                if selected {
                    term::bg_row_selected(&term::pad_right_styled(&line, width))
                } else {
                    line
                }
            }
            Row::ProxyItem(idx) => {
                let marker = if selected {
                    term::fg_crimson_bold("\u{25B8} ")
                } else {
                    "  ".to_string()
                };
                let Some(px) = self.proxies.get(*idx) else {
                    return String::new();
                };
                let scheme = match px.scheme {
                    ProxyScheme::Socks5 => term::fg_cyan("SOCKS5"),
                    ProxyScheme::Http => term::fg_gold("HTTP  "),
                };
                let id = px.id();
                let auth = if px.user.is_empty() { "" } else { " · auth" };
                let right = match self.proxy_probes.get(&id) {
                    Some((true, detail)) => term::fg_emerald(&format!("\u{2713} {detail}")),
                    Some((false, err)) => term::fg_crimson(&format!("\u{2717} {err}")),
                    None => term::fg_dim("[t: probe · d: del]"),
                };
                let left = format!(
                    "{marker}{scheme} {}{}",
                    if selected {
                        term::fg_white_bold(&id)
                    } else {
                        term::fg_soft(&id)
                    },
                    term::fg_dim(auth)
                );
                let gap = width
                    .saturating_sub(term::visible_len(&left) + term::visible_len(&right))
                    .max(1);
                let line = format!("{left}{}{right}", " ".repeat(gap));
                if selected {
                    term::bg_row_selected(&term::pad_right_styled(&line, width))
                } else {
                    line
                }
            }
            Row::RuleItem(pattern) => {
                let marker = if selected {
                    term::fg_crimson_bold("\u{25B8} ")
                } else {
                    "  ".to_string()
                };
                let rule = self.draft_cfg.rules.url.get(pattern);
                let (badge, pill) = match rule {
                    Some(r) => {
                        let b = match r.action {
                            RuleAction::Deny => term::fg_crimson_bold("DENY "),
                            RuleAction::Allow => term::fg_emerald("ALLOW"),
                        };
                        let p = if r.enabled {
                            term::pill_on(flash)
                        } else {
                            term::pill_off(flash)
                        };
                        (b, p)
                    }
                    None => (term::fg_dim("RULE "), term::pill_off(false)),
                };
                let left = format!(
                    "{marker}{badge} {}",
                    if selected {
                        term::fg_white_bold(&term::truncate_plain(
                            pattern,
                            width.saturating_sub(22),
                        ))
                    } else {
                        term::fg_soft(&term::truncate_plain(pattern, width.saturating_sub(22)))
                    }
                );
                let gap = width
                    .saturating_sub(term::visible_len(&left) + term::visible_len(&pill))
                    .max(1);
                let line = format!("{left}{}{pill}", " ".repeat(gap));
                if selected {
                    term::bg_row_selected(&term::pad_right_styled(&line, width))
                } else {
                    line
                }
            }
            Row::Preset(preset) => {
                let marker = if selected {
                    term::fg_crimson_bold("\u{25B8} ")
                } else {
                    "  ".to_string()
                };
                let left = format!(
                    "{marker}{} {}",
                    term::fg_gold("\u{26A1}"),
                    if selected {
                        term::fg_white_bold(preset.name)
                    } else {
                        term::fg_soft(preset.name)
                    }
                );
                let right = if flash {
                    term::pill_badge_ok(" STAGED ")
                } else {
                    term::fg_cyan("[Enter: stage]")
                };
                let gap = width
                    .saturating_sub(term::visible_len(&left) + term::visible_len(&right))
                    .max(1);
                let line = format!("{left}{}{right}", " ".repeat(gap));
                if selected {
                    term::bg_row_selected(&term::pad_right_styled(&line, width))
                } else {
                    line
                }
            }
        }
    }

    fn render_control(
        &self,
        spec: &SettingSpec,
        val: &TomlEditValue,
        flash: bool,
        wide: bool,
    ) -> String {
        match spec.control {
            ControlKind::Toggle => {
                if matches!(val, TomlEditValue::Bool(true)) {
                    term::pill_on(flash)
                } else {
                    term::pill_off(flash)
                }
            }
            ControlKind::SliderInt { min, max, .. } => {
                let n = match val {
                    TomlEditValue::Int(v) => *v,
                    _ => min,
                };
                let ratio = if max > min {
                    (n - min) as f64 / (max - min) as f64
                } else {
                    0.0
                };
                let bar = term::render_slider(ratio, if wide { 12 } else { 8 }, flash, self.tick);
                let label = format!("{:>10}", schema::display_value(spec, val));
                let painted = if flash {
                    term::fg_gold(&label)
                } else {
                    term::fg_cyan(&label)
                };
                format!("{bar} {painted}")
            }
            ControlKind::SliderFloat { min, max, .. } => {
                let f = match val {
                    TomlEditValue::Float(v) => *v,
                    _ => min,
                };
                let ratio = if max > min {
                    (f - min) / (max - min)
                } else {
                    0.0
                };
                let bar = term::render_slider(ratio, if wide { 12 } else { 8 }, flash, self.tick);
                let label = format!("{:>6}", schema::display_value(spec, val));
                let painted = if flash {
                    term::fg_gold(&label)
                } else {
                    term::fg_cyan(&label)
                };
                format!("{bar} {painted}")
            }
            ControlKind::Enum(_) => {
                let shown = schema::display_value(spec, val);
                if flash {
                    term::pill_badge_warn(&format!(" \u{2039} {shown} \u{203A} "))
                } else {
                    term::fg_cyan(&format!("\u{2039} {shown} \u{203A}"))
                }
            }
            ControlKind::Text | ControlKind::Url | ControlKind::Secret | ControlKind::List => {
                let shown = schema::display_value(spec, val);
                let clipped = term::truncate_plain(&shown, if wide { 24 } else { 16 });
                if shown == "(unset)" || shown == "(empty)" {
                    term::fg_dim(&clipped)
                } else if flash {
                    term::fg_gold(&clipped)
                } else {
                    term::fg_cyan(&clipped)
                }
            }
        }
    }

    fn render_inspector(&self, row: Option<&Row>, width: usize, max_lines: usize) -> Vec<String> {
        let mut out = Vec::new();
        let Some(row) = row else {
            out.push(term::fg_dim("Select a setting to inspect details."));
            return out;
        };
        match row {
            Row::Setting(spec) => {
                let val = schema::get_value(&self.draft_cfg, spec);
                let def_val = schema::get_value(&DonsetchConfig::default(), spec);
                let origin = schema::origin_label(&self.loaded, spec);
                let dirty = self
                    .staged
                    .contains_key(&(spec.section.to_string(), spec.key.to_string()));

                out.push(term::fg_white_bold(&term::truncate_plain(
                    spec.label, width,
                )));
                out.push(term::fg_crimson(&spec.full_key()));
                out.push(String::new());
                for line in term::wrap_plain(spec.help, width) {
                    out.push(term::fg_soft(&line));
                }
                out.push(String::new());
                out.push(format!(
                    "{} {}",
                    term::fg_dim("Value:  "),
                    term::fg_cyan(&term::truncate_plain(
                        &schema::display_value(spec, &val),
                        width.saturating_sub(9)
                    ))
                ));
                out.push(format!(
                    "{} {}",
                    term::fg_dim("Default:"),
                    term::fg_soft(&term::truncate_plain(
                        &schema::display_value(spec, &def_val),
                        width.saturating_sub(9)
                    ))
                ));
                let src_display = if dirty {
                    term::fg_gold("unsaved change (press s to write)")
                } else if schema::is_env_origin(&origin) {
                    term::fg_gold(&format!("env: {origin}"))
                } else if origin.starts_with("file:") {
                    term::fg_emerald("donsetch.toml")
                } else {
                    term::fg_dim("compiled default")
                };
                out.push(format!("{} {src_display}", term::fg_dim("Source: ")));
                if schema::is_env_origin(&origin) {
                    out.push(String::new());
                    for line in term::wrap_plain(
                        &format!(
                            "Note: {origin} is set in this shell and overrides donsetch.toml until unset."
                        ),
                        width,
                    ) {
                        out.push(term::fg_gold(&line));
                    }
                }
                out.push(String::new());
                out.push(term::fg_dim("TOML:"));
                out.push(term::fg_soft(&term::truncate_plain(
                    &schema::toml_preview(spec, &val),
                    width,
                )));
                out.push(format!(
                    "{} {}",
                    term::fg_dim("Env:"),
                    term::fg_dim(&term::truncate_plain(
                        &spec.env_var(),
                        width.saturating_sub(5)
                    ))
                ));
                out.push(format!(
                    "{} {}",
                    term::fg_dim("CLI:"),
                    term::fg_cyan(&term::truncate_plain(
                        spec.cli_hint,
                        width.saturating_sub(5)
                    ))
                ));
            }
            Row::DefaultProvider => {
                out.push(term::fg_white_bold("Default Search Provider"));
                out.push(term::fg_crimson("donsetch keys default <provider|local>"));
                out.push(String::new());
                for line in term::wrap_plain(
                    "Choose `local` to run keyless search first (falling back to configured providers on errors), or pick a configured provider/SearXNG/plugin to run it first.",
                    width,
                ) {
                    out.push(term::fg_soft(&line));
                }
                out.push(String::new());
                out.push(format!(
                    "{} {}",
                    term::fg_dim("Available:"),
                    term::fg_cyan(&self.available_default_providers().join(", "))
                ));
                out.push(term::fg_dim("Use ← / → or Space to switch immediately."));
            }
            Row::Provider(provider) => {
                out.push(term::fg_white_bold(&format!("Provider: {provider}")));
                out.push(term::fg_crimson(&format!(
                    "donsetch keys add {provider} <KEY>"
                )));
                out.push(String::new());
                let role_help = if is_fetch_side(provider) {
                    "Fetch-only Bright Data Web Unlocker zone (`TOKEN::ZONE`). Only invoked after a local bot wall; never enters the search chain."
                } else if *provider == "brightdata" {
                    "Bright Data SERP API (`TOKEN::SERP_ZONE`). Supports stacked keys and automatic cooldown recovery."
                } else {
                    "Native BYOK search provider. Stack multiple keys per provider for automatic failover and 60s rate-limit recovery."
                };
                for line in term::wrap_plain(role_help, width) {
                    out.push(term::fg_soft(&line));
                }
                out.push(String::new());
                out.push(term::fg_cyan("Enter / a  Add API key (saved 0600)"));
                out.push(term::fg_cyan("r          Reset key states to active"));
                out.push(term::fg_cyan("d          Remove all keys for provider"));
            }
            Row::AddPlugin => {
                out.push(term::fg_white_bold("Register Custom Search Plugin"));
                out.push(term::fg_crimson(
                    "donsetch keys add plugin <name> --cmd '...'",
                ));
                out.push(String::new());
                for line in term::wrap_plain(
                    "Connect any executable that reads one JSON request from stdin and writes JSON results to stdout (no shell, bounded 8 MiB stdout).",
                    width,
                ) {
                    out.push(term::fg_soft(&line));
                }
            }
            Row::Plugin(name) => {
                out.push(term::fg_white_bold(&format!("Plugin: {name}")));
                if let Some(def) = self.plugin_cfg.plugins.get(name) {
                    out.push(String::new());
                    out.push(format!(
                        "{} {}",
                        term::fg_dim("Command:"),
                        term::fg_soft(&term::truncate_plain(
                            &def.cmd.join(" "),
                            width.saturating_sub(9)
                        ))
                    ));
                    out.push(format!(
                        "{} {} s",
                        term::fg_dim("Timeout:"),
                        def.timeout_ms / 1000
                    ));
                    out.push(format!(
                        "{} {}",
                        term::fg_dim("State:  "),
                        def.state.label()
                    ));
                }
                out.push(String::new());
                out.push(term::fg_cyan("r  Reset plugin state to active"));
                out.push(term::fg_cyan("d  Remove plugin"));
            }
            Row::AddProxy => {
                out.push(term::fg_white_bold("Add Proxy to Pool"));
                out.push(term::fg_crimson("donsetch proxy add <url>"));
                out.push(String::new());
                for line in term::wrap_plain(
                    "Accepts `socks5://user:pass@host:1080`, `http://user:pass@host:8080`, or `host:port`. Stored owner-only (0600) in proxies.txt.",
                    width,
                ) {
                    out.push(term::fg_soft(&line));
                }
            }
            Row::ProxyItem(idx) => {
                if let Some(px) = self.proxies.get(*idx) {
                    out.push(term::fg_white_bold(&px.id()));
                    out.push(term::fg_crimson(match px.scheme {
                        ProxyScheme::Socks5 => "SOCKS5 (remote DNS, stealth-preserving)",
                        ProxyScheme::Http => "HTTP CONNECT",
                    }));
                    out.push(String::new());
                    if let Some((ok, detail)) = self.proxy_probes.get(&px.id()) {
                        let st = if *ok {
                            term::fg_emerald(&format!("Alive: {detail}"))
                        } else {
                            term::fg_crimson(&format!("Failed: {detail}"))
                        };
                        out.push(st);
                    } else {
                        out.push(term::fg_dim("Not probed this session."));
                    }
                    out.push(String::new());
                    out.push(term::fg_cyan("Enter / t  Live probe exit IP & latency"));
                    out.push(term::fg_cyan("d          Remove proxy from pool"));
                    out.push(term::fg_cyan("c          Clear entire pool"));
                }
            }
            Row::AddRule => {
                out.push(term::fg_white_bold("Add Per-Host Rule"));
                out.push(term::fg_crimson("[rules.url.\"<pattern>\"]"));
                out.push(String::new());
                for line in term::wrap_plain(
                    "Patterns: `example.com` (host + subdomains), `.example.com` (exact host), or `https://example.com` (scheme-scoped). Saved & compiled directly into donsetch.toml.",
                    width,
                ) {
                    out.push(term::fg_soft(&line));
                }
            }
            Row::TestRule => {
                out.push(term::fg_white_bold("Live Rule Evaluator"));
                out.push(term::fg_crimson("donsetch rules test <url>"));
                out.push(String::new());
                for line in term::wrap_plain(
                    "Enter any http(s) URL to test offline which rule matches and what denial code & guidance an agent would receive.",
                    width,
                ) {
                    out.push(term::fg_soft(&line));
                }
            }
            Row::RuleItem(pattern) => {
                out.push(term::fg_white_bold(&format!("Rule: {pattern}")));
                if let Some(rule) = self.draft_cfg.rules.url.get(pattern) {
                    out.push(String::new());
                    out.push(format!(
                        "{} {}",
                        term::fg_dim("Action: "),
                        match rule.action {
                            RuleAction::Deny => term::fg_crimson_bold("deny"),
                            RuleAction::Allow => term::fg_emerald("allow"),
                        }
                    ));
                    out.push(format!(
                        "{} {}",
                        term::fg_dim("Enabled:"),
                        if rule.enabled { "true" } else { "false" }
                    ));
                    if rule.action == RuleAction::Deny {
                        out.push(format!(
                            "{} {}",
                            term::fg_dim("Kind:   "),
                            rule.kind.as_str()
                        ));
                    }
                    if let Some(reason) = &rule.reason {
                        out.push(format!(
                            "{} policy.denied.{reason}",
                            term::fg_dim("Code:   ")
                        ));
                    }
                    if let Some(msg) = &rule.message {
                        out.push(String::new());
                        for line in term::wrap_plain(msg, width) {
                            out.push(term::fg_soft(&line));
                        }
                    }
                }
                out.push(String::new());
                out.push(term::fg_cyan("Space / Enter  Toggle enabled"));
                out.push(term::fg_cyan("d              Delete rule"));
            }
            Row::Preset(preset) => {
                out.push(term::fg_white_bold(preset.name));
                out.push(term::fg_gold(preset.tagline));
                out.push(String::new());
                for line in term::wrap_plain(preset.description, width) {
                    out.push(term::fg_soft(&line));
                }
                out.push(String::new());
                out.push(term::fg_dim("Changes staged:"));
                for (sec, key, val) in preset.changes {
                    out.push(format!(
                        "  {} = {}",
                        term::fg_cyan(&format!("{sec}.{key}")),
                        val
                    ));
                }
                out.push(String::new());
                out.push(term::fg_cyan(&format!(
                    "CLI: donsetch settings preset {}",
                    preset.id
                )));
            }
            Row::Header(_) => {}
        }
        out.truncate(max_lines);
        out
    }

    fn overlay_modal(&self, frame: &mut [String], modal: &Modal, cols: usize, rows_h: usize) {
        let box_w = cols.saturating_sub(8).clamp(44, 72);
        let inner = box_w.saturating_sub(4);
        let mut body: Vec<String> = Vec::new();
        let cursor_glyph = if (self.tick / 6).is_multiple_of(2) {
            "\u{258C}"
        } else {
            " "
        };

        let (title, footer): (String, String) = match modal {
            Modal::EditSetting {
                spec,
                buffer,
                revealed,
                error,
            } => {
                for l in term::wrap_plain(spec.summary, inner) {
                    body.push(term::fg_dim(&l));
                }
                body.push(String::new());
                let shown = if *revealed {
                    buffer.clone()
                } else {
                    "*".repeat(buffer.chars().count())
                };
                body.push(format!(
                    "{} {}{}",
                    term::fg_crimson_bold(">"),
                    term::fg_white_bold(&term::truncate_plain(&shown, inner.saturating_sub(4))),
                    term::fg_gold(cursor_glyph)
                ));
                if let Some(err) = error {
                    body.push(String::new());
                    for l in term::wrap_plain(err, inner) {
                        body.push(term::fg_crimson_bold(&l));
                    }
                }
                let f = if matches!(spec.control, ControlKind::Secret) {
                    "Enter apply · Ctrl+R reveal/hide · Esc cancel"
                } else {
                    "Enter apply · Backspace edit · Esc cancel"
                };
                (
                    format!("Edit {} ({})", spec.label, spec.full_key()),
                    f.into(),
                )
            }
            Modal::AddProviderKey {
                provider,
                buffer,
                revealed,
                error,
            } => {
                let hint = if *provider == "brightdata" || *provider == "unlocker" {
                    "Format: TOKEN::ZONE (saved owner-only 0600 in byok-keys.json)"
                } else {
                    "Paste your provider API key (saved owner-only 0600 in byok-keys.json)"
                };
                for l in term::wrap_plain(hint, inner) {
                    body.push(term::fg_dim(&l));
                }
                body.push(String::new());
                let shown = if *revealed {
                    buffer.clone()
                } else {
                    "*".repeat(buffer.chars().count())
                };
                body.push(format!(
                    "{} {}{}",
                    term::fg_crimson_bold(">"),
                    term::fg_white_bold(&term::truncate_plain(&shown, inner.saturating_sub(4))),
                    term::fg_gold(cursor_glyph)
                ));
                if let Some(err) = error {
                    body.push(String::new());
                    body.push(term::fg_crimson_bold(&term::truncate_plain(err, inner)));
                }
                (
                    format!("Add API Key for {provider}"),
                    "Enter save key · Ctrl+R reveal/hide · Esc cancel".into(),
                )
            }
            Modal::ConfirmRemoveProvider { provider, count } => {
                body.push(term::fg_soft(&format!(
                    "Delete all {count} stored key(s) for `{provider}` from byok-keys.json?"
                )));
                (
                    format!("Remove {provider} keys?"),
                    "Y / Enter confirm · N / Esc cancel".into(),
                )
            }
            Modal::AddPlugin {
                step,
                name,
                cmd,
                timeout_secs,
                error,
            } => {
                let fields = [
                    (0u8, "Plugin name: ", name.as_str()),
                    (1u8, "Command:     ", cmd.as_str()),
                    (2u8, "Timeout (s): ", timeout_secs.as_str()),
                ];
                for (i, label, val) in fields {
                    let active = *step == i;
                    let prefix = if active {
                        term::fg_crimson_bold("\u{25B8} ")
                    } else {
                        "  ".to_string()
                    };
                    let cur = if active {
                        term::fg_gold(cursor_glyph)
                    } else {
                        String::new()
                    };
                    body.push(format!(
                        "{prefix}{}{}{cur}",
                        term::fg_dim(label),
                        term::fg_white_bold(&term::truncate_plain(val, inner.saturating_sub(18)))
                    ));
                }
                if let Some(err) = error {
                    body.push(String::new());
                    for l in term::wrap_plain(err, inner) {
                        body.push(term::fg_crimson_bold(&l));
                    }
                }
                (
                    "Register Custom Search Plugin".into(),
                    "Tab next field · Enter next/save · Esc cancel".into(),
                )
            }
            Modal::ConfirmRemovePlugin { name } => {
                body.push(term::fg_soft(
                    "Delete this executable plugin from byok-plugins.json?",
                ));
                (
                    format!("Remove plugin `{name}`?"),
                    "Y / Enter confirm · N / Esc cancel".into(),
                )
            }
            Modal::AddProxy { buffer, error } => {
                body.push(term::fg_dim(
                    "e.g. socks5://user:pass@proxy.example:1080 or http://host:8080",
                ));
                body.push(String::new());
                body.push(format!(
                    "{} {}{}",
                    term::fg_crimson_bold(">"),
                    term::fg_white_bold(&term::truncate_plain(buffer, inner.saturating_sub(4))),
                    term::fg_gold(cursor_glyph)
                ));
                if let Some(err) = error {
                    body.push(String::new());
                    for l in term::wrap_plain(err, inner) {
                        body.push(term::fg_crimson_bold(&l));
                    }
                }
                (
                    "Add Proxy to Pool".into(),
                    "Enter add proxy · Esc cancel".into(),
                )
            }
            Modal::ConfirmClearProxies { count } => {
                body.push(term::fg_soft(&format!(
                    "Remove all {count} proxies from proxies.txt?"
                )));
                (
                    "Clear proxy pool?".into(),
                    "Y / Enter clear all · N / Esc cancel".into(),
                )
            }
            Modal::AddRule {
                step,
                pattern,
                action_deny,
                message,
                kind_permanent,
                reason,
                error,
            } => {
                let action_str = if *action_deny {
                    "\u{2039} deny \u{203A}"
                } else {
                    "\u{2039} allow \u{203A}"
                };
                let kind_str = if *kind_permanent {
                    "\u{2039} permanent \u{203A}"
                } else {
                    "\u{2039} walled \u{203A}"
                };
                let fields = [
                    (0u8, "Host pattern:", pattern.as_str()),
                    (1u8, "Action:      ", action_str),
                    (2u8, "Message:     ", message.as_str()),
                    (3u8, "Deny kind:   ", kind_str),
                    (4u8, "Reason code: ", reason.as_str()),
                ];
                for (i, label, val) in fields {
                    let active = *step == i;
                    let prefix = if active {
                        term::fg_crimson_bold("\u{25B8} ")
                    } else {
                        "  ".to_string()
                    };
                    let cur = if active && matches!(i, 0 | 2 | 4) {
                        term::fg_gold(cursor_glyph)
                    } else {
                        String::new()
                    };
                    body.push(format!(
                        "{prefix}{} {}{cur}",
                        term::fg_dim(label),
                        term::fg_white_bold(&term::truncate_plain(val, inner.saturating_sub(18)))
                    ));
                }
                if let Some(err) = error {
                    body.push(String::new());
                    for l in term::wrap_plain(err, inner) {
                        body.push(term::fg_crimson_bold(&l));
                    }
                }
                (
                    "Add Per-Host Policy Rule".into(),
                    "Tab field · ←→/Space toggle enums · Enter next/save · Esc cancel".into(),
                )
            }
            Modal::TestRuleUrl { buffer, result } => {
                body.push(format!(
                    "{} {}{}",
                    term::fg_crimson_bold(">"),
                    term::fg_white_bold(&term::truncate_plain(buffer, inner.saturating_sub(4))),
                    term::fg_gold(cursor_glyph)
                ));
                if let Some(res) = result {
                    body.push(String::new());
                    for l in term::wrap_plain(res, inner) {
                        if res.starts_with("DENIED") {
                            body.push(term::fg_crimson_bold(&l));
                        } else if res.starts_with("ALLOWED") {
                            body.push(term::fg_emerald(&l));
                        } else {
                            body.push(term::fg_gold(&l));
                        }
                    }
                }
                (
                    "Test URL Against Local Rules".into(),
                    "Enter test URL · Esc close".into(),
                )
            }
            Modal::ConfirmQuit => {
                body.push(term::fg_soft(&format!(
                    "You have {} unsaved change(s) staged for donsetch.toml.",
                    self.staged.len()
                )));
                body.push(String::new());
                body.push(format!(
                    "  {} Save & quit    {} Discard & quit    {} Stay",
                    term::pill_badge_ok(" S "),
                    term::pill_badge_crimson(" D "),
                    term::pill_inactive_tab(" Esc ")
                ));
                (
                    "Unsaved Changes".into(),
                    "S save · D discard · Esc cancel".into(),
                )
            }
            Modal::Help => {
                for (k, desc) in [
                    ("1-8 / Tab / [ ]", "Switch category tabs"),
                    ("↑ ↓ / j k", "Move selection (PgUp/PgDn jumps)"),
                    ("← → / h l", "Slide numbers, cycle enums, or flip toggles"),
                    ("Shift+←→ / - +", "Adjust sliders in larger steps"),
                    ("Space / Enter", "Toggle switch, open editor, or run action"),
                    ("/", "Live search across every setting & provider"),
                    (
                        "s / Ctrl+S",
                        "Validate & atomically save changes to donsetch.toml",
                    ),
                    ("u", "Stage reset to compiled default for selected setting"),
                    (
                        "a / r / t / d",
                        "Add key/proxy/rule · Reset · Probe proxy · Delete",
                    ),
                    ("q / Esc", "Quit (prompts if unsaved changes exist)"),
                ] {
                    body.push(format!(
                        "  {}  {}",
                        term::fg_cyan(&format!("{k:<16}")),
                        term::fg_soft(desc)
                    ));
                }
                (
                    "DonSeTch Settings — Keyboard Controls".into(),
                    "Press any key to close".into(),
                )
            }
        };

        let total_h = body.len() + 5;
        let top = rows_h.saturating_sub(total_h) / 2;
        let pad_left = " ".repeat(cols.saturating_sub(box_w) / 2);

        let mut modal_lines = Vec::with_capacity(total_h);
        let top_bar = format!(
            "\u{2554}\u{2550} {} {}\u{2557}",
            term::fg_white_bold(&term::truncate_plain(&title, inner.saturating_sub(2))),
            "\u{2550}".repeat(
                box_w.saturating_sub(
                    term::truncate_plain(&title, inner.saturating_sub(2))
                        .chars()
                        .count()
                        + 5
                )
            )
        );
        modal_lines.push(format!("{pad_left}{}", term::fg_crimson(&top_bar)));
        for line in body {
            let padded = term::pad_right_styled(&line, inner);
            modal_lines.push(format!(
                "{pad_left}{} {padded} {}",
                term::fg_crimson("\u{2551}"),
                term::fg_crimson("\u{2551}")
            ));
        }
        modal_lines.push(format!(
            "{pad_left}{}",
            term::fg_muted(&format!(
                "\u{255F}{}\u{2562}",
                "\u{2500}".repeat(box_w.saturating_sub(2))
            ))
        ));
        let foot =
            term::pad_right_styled(&term::fg_dim(&term::truncate_plain(&footer, inner)), inner);
        modal_lines.push(format!(
            "{pad_left}{} {foot} {}",
            term::fg_crimson("\u{2551}"),
            term::fg_crimson("\u{2551}")
        ));
        modal_lines.push(format!(
            "{pad_left}{}",
            term::fg_crimson(&format!(
                "\u{255A}{}\u{255D}",
                "\u{2550}".repeat(box_w.saturating_sub(2))
            ))
        ));

        for (offset, line) in modal_lines.into_iter().enumerate() {
            if top + offset < frame.len() {
                frame[top + offset] = line;
            }
        }
    }
}

/// Launch the interactive full-screen `donsetch settings` Control Center.
pub async fn run(loaded: Loaded) -> Result<Option<String>, String> {
    let mut guard =
        TerminalGuard::enter().map_err(|e| format!("cannot initialize terminal UI: {e}"))?;
    let mut app = App::new(loaded);
    let mut last_tick = Instant::now();

    loop {
        app.poll_pending_probe();
        let (cols, rows_h) = term::term_size();
        let frame = app.render(cols, rows_h);
        if let Err(e) = term::flush_frame(&frame) {
            guard.leave();
            return Err(format!("terminal write failed: {e}"));
        }

        if app.should_quit {
            break;
        }

        // Poll input with ~35ms tick for smooth ~28 FPS animations at tiny CPU cost.
        let key = tokio::task::spawn_blocking(|| term::poll_key(35))
            .await
            .ok()
            .flatten();
        if let Some(k) = key {
            app.handle_key(k);
        }

        let now = Instant::now();
        if now.duration_since(last_tick) >= Duration::from_millis(32) {
            last_tick = now;
            app.tick = app.tick.wrapping_add(1);
            app.boot_frames = app.boot_frames.saturating_sub(1);
            app.tab_sweep = app.tab_sweep.saturating_sub(1);
            if let Some((idx, rem)) = app.flash {
                app.flash = (rem > 1).then_some((idx, rem - 1));
            }
            if let Some((msg, err, rem)) = app.toast.take()
                && rem > 1
            {
                app.toast = Some((msg, err, rem - 1));
            }
        }
    }

    guard.leave();
    Ok(app.saved_summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_loaded() -> Loaded {
        Loaded {
            config: DonsetchConfig::default(),
            warnings: Vec::new(),
            file: None,
            merged: ::config::Map::new(),
        }
    }

    #[test]
    fn tui_navigation_toggles_sliders_search_and_presets_work_cleanly() {
        let mut app = App::new(test_loaded());
        assert_eq!(app.active_tab, TabId::Search);

        // First selectable row on Search tab is search.sequential.
        app.handle_key(KeyEvent::Char(' '));
        assert!(app.draft_cfg.search.sequential);
        assert_eq!(app.staged.len(), 1);

        // Move to wave_size and decrement with Left arrow (3 -> 2 -> 1).
        app.handle_key(KeyEvent::Down);
        app.handle_key(KeyEvent::Left);
        app.handle_key(KeyEvent::Left);
        assert_eq!(app.draft_cfg.search.wave_size, 1);

        // Move to timeout_ms and adjust coarsely with ShiftRight (4000 -> 9000).
        app.handle_key(KeyEvent::Down);
        app.handle_key(KeyEvent::ShiftRight);
        assert_eq!(app.draft_cfg.search.timeout_ms, 9000);

        // Switch across all 8 tabs and render both wide and narrow frames.
        for &tab in &TabId::ALL {
            app.switch_tab(tab);
            let wide = app.render(120, 34);
            assert!(!wide.is_empty(), "wide render empty for {tab:?}");
            let narrow = app.render(72, 22);
            assert!(!narrow.is_empty(), "narrow render empty for {tab:?}");
        }

        // Live search filters across all tabs.
        app.handle_key(KeyEvent::Char('/'));
        for c in "searx".chars() {
            app.handle_key(KeyEvent::Char(c));
        }
        app.handle_key(KeyEvent::Enter);
        let filtered = app.rows();
        assert!(
            filtered
                .iter()
                .any(|r| matches!(r, Row::Setting(s) if s.key == "searxng_url")),
            "search filter must surface search.searxng_url"
        );
        app.handle_key(KeyEvent::Esc);
        assert!(app.search_query.is_empty());

        // Help modal opens and closes cleanly, and quitting with unsaved edits prompts.
        app.handle_key(KeyEvent::Char('?'));
        assert!(matches!(app.modal, Some(Modal::Help)));
        let with_help = app.render(110, 32);
        assert!(with_help.iter().any(|l| l.contains("Keyboard Controls")));
        app.handle_key(KeyEvent::Esc);
        assert!(app.modal.is_none());

        app.handle_key(KeyEvent::Char('q'));
        assert!(matches!(app.modal, Some(Modal::ConfirmQuit)));
        app.handle_key(KeyEvent::Char('d'));
        assert!(app.should_quit);
        assert!(app.staged.is_empty());
    }
}
