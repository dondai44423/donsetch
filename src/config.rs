//! Runtime configuration: one typed struct, layered sources, one home.
//!
//! Layers, lowest to highest precedence:
//!   1. serde defaults (this file, matching the historical runtime defaults)
//!   2. legacy env names (the pre-v4 scattered knobs, exact trigger semantics preserved)
//!   3. `donsetch.toml` file layer (`DONSETCH_CONFIG` explicit, or the default location)
//!   4. new env names: `DONSETCH_<SECTION>__<KEY>` (two underscores = one dot)
//!
//! Standard process vars (HTTP_PROXY, SSL_CERT_FILE, TZ, LANG, NO_COLOR,
//! PLAYWRIGHT_BROWSERS_PATH, PATH, HOME, ...) stay ambient and are read at
//! call time; the config value of the mirrored knob wins over the ambient
//! var whenever it is set.
//!
//! Notes:
//! - `DONSETCH_CACHE_DIR` is NOT a knob: `paths::cache_dir()` keeps reading
//!   it per call so tests can isolate state; `paths.cache_dir` in the TOML
//!   layer is the fallback when the env var is absent.
//! - `DONSETCH_NO_CONFIG_FILE=1` skips the file layer entirely (hermetic
//!   runs, CI).
//! - Values from the new env layer or TOML fail the whole load when
//!   unparsable or out of range (loud, with the key path named). Legacy
//!   names keep their historical silent-fallback semantics, except that a
//!   bad value now also emits a stderr warning.
//! - `cargo test` users are broken by design: the global is frozen at first
//!   use. Our runner is nextest (one process per test), which makes the env
//!   layers behave exactly like before for every test.

use config::Source as _;
use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// The struct
// ---------------------------------------------------------------------------

macro_rules! section {
    ($name:ident { $($field:ident : $ty:ty = $default:expr),* $(,)? }) => {
        #[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
        #[serde(default, deny_unknown_fields)]
        pub struct $name {
            $(pub $field: $ty,)*
        }
        impl Default for $name {
            fn default() -> Self {
                Self { $($field: $default,)* }
            }
        }
    };
}

section!(TransportSection {
    kind: TransportKind = TransportKind::Stdio,
    host: String = "127.0.0.1".into(),
    port: u16 = 8765,
    token: String = String::new(),
    timeout_secs: u64 = 300,
    cors: bool = false,
});

section!(McpSection {
    text_only: bool = false,
    url_handles: bool = true,
});

section!(PathsSection {
    cache_dir: String = String::new(),
});

section!(StateSection {
    no_disk_state: bool = false,
    route_memory: RouteMemory = RouteMemory::ReadWrite,
    cookie_vault: bool = true,
});

section!(ProxySection {
    from_environment: bool = true,
    http: String = String::new(),
    https: String = String::new(),
    all: String = String::new(),
    no_proxy: String = String::new(),
    pool: Vec<String> = Vec::new(),
});

section!(TlsSection {
    cert_file: String = String::new(),
    cert_dir: String = String::new(),
});

section!(PersonaSection {
    timezone: String = String::new(),
    locale: String = String::new(),
});

section!(CliSection {
    color: ColorMode = ColorMode::Auto,
});

section!(FetchSection {
    allow_private_egress: bool = false,
    h3: bool = false,
    alt_svc: bool = true,
    shadow_fetch: ShadowFetch = ShadowFetch::Auto,
    shadow_deadline_ms: u64 = 3000,
    adapters: bool = true,
    adapter_dump_dir: String = String::new(),
    crawl_shape: bool = true,
    prewarm: bool = true,
    pdf_max_mb: usize = 100,
    ocr: bool = true,
    ocr_max_pages: u32 = 25,
});

section!(BypassSection {
    enabled: bool = true,
    zone: String = String::new(),
    max_daily: u32 = 50,
    timeout_secs: u64 = 120,
    render: bool = false,
    endpoint: String = String::new(),
    cache: bool = true,
    cache_ttl_secs: u64 = 21_600,
    cache_max_entries: u32 = 200,
});

section!(SearchSection {
    google_profile: String = String::new(),
    ghost_lane: GhostLane = GhostLane::Auto,
    rerank_topup: bool = true,
    rerank_threads: u32 = 0,
    brightdata_zone: String = String::new(),
});

section!(BrowserSection {
    backend: BrowserBackend = BrowserBackend::Auto,
    chromium_path: String = String::new(),
    no_sandbox: bool = false,
    cloak_auto_download: bool = false,
    cloak_path: String = String::new(),
    cloak_version: String = String::new(),
    cloak_cache_dir: String = String::new(),
    playwright_path: String = String::new(),
    pool_slots: u32 = 0,
    xvfb_display: u16 = 0,
    route_probes: bool = true,
    probe_scan_secs: u64 = 120,
    probe_stale_secs: u64 = 21_600,
    login_force: bool = false,
});

section!(DebugSection {
    ghost: bool = false,
    search: bool = false,
    pdf: bool = false,
    pdf_matrix: bool = false,
    pdf_chars: bool = false,
    pdf_words: bool = false,
    extract: bool = false,
    echo_scorecard: bool = false,
});

#[derive(Debug, Clone, PartialEq, Default, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DonsetchConfig {
    pub transport: TransportSection,
    pub mcp: McpSection,
    pub paths: PathsSection,
    pub state: StateSection,
    pub proxy: ProxySection,
    pub tls: TlsSection,
    pub persona: PersonaSection,
    pub cli: CliSection,
    pub fetch: FetchSection,
    pub bypass: BypassSection,
    pub search: SearchSection,
    pub browser: BrowserSection,
    pub debug: DebugSection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TransportKind {
    #[default]
    Stdio,
    Http,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RouteMemory {
    Off,
    #[default]
    #[serde(alias = "readwrite")]
    ReadWrite,
    #[serde(alias = "readonly")]
    ReadOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ShadowFetch {
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GhostLane {
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BrowserBackend {
    #[default]
    Auto,
    #[serde(alias = "chrome", alias = "original")]
    Chromium,
    #[serde(alias = "headless", alias = "original-headless")]
    Headless,
    #[serde(alias = "cloakbrowser")]
    Cloak,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ColorMode {
    #[default]
    Auto,
    Never,
    Always,
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum ConfigError {
    Env(String),
    File { path: String, message: String },
    Validate(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Env(m) => write!(f, "invalid env value: {m}"),
            ConfigError::File { path, message } => {
                write!(f, "invalid config file {path}: {message}")
            }
            ConfigError::Validate(m) => write!(f, "invalid config value: {m}"),
        }
    }
}

impl std::error::Error for ConfigError {}

pub struct Loaded {
    pub config: DonsetchConfig,
    /// Non-fatal notes: ignored legacy values, unknown DONSETCH_* vars.
    pub warnings: Vec<String>,
    /// The file the TOML layer came from, if any.
    pub file: Option<std::path::PathBuf>,
    /// The fully layered tree, leaf origins intact (for `config show`).
    pub merged: config::Map<String, config::Value>,
}

/// Load the layered config from defaults + env + TOML file and validate it.
pub fn load() -> Result<Loaded, ConfigError> {
    let mut warnings = Vec::new();

    let mut file_layer: Option<config::Map<String, config::Value>> = None;
    let mut file_path: Option<std::path::PathBuf> = None;
    let skip_file = std::env::var("DONSETCH_NO_CONFIG_FILE")
        .ok()
        .map(|v| v.trim().to_ascii_lowercase())
        .is_some_and(|v| !matches!(v.as_str(), "0" | "false" | "off" | "no"));
    if !skip_file {
        match toml_path() {
            Some(path) => {
                let text = std::fs::read_to_string(&path).map_err(|e| ConfigError::File {
                    path: path.display().to_string(),
                    message: format!("unreadable ({e})"),
                })?;
                let source = config::File::from_str(&text, config::FileFormat::Toml);
                let mut map = source.collect().map_err(|e| ConfigError::File {
                    path: path.display().to_string(),
                    message: e.to_string(),
                })?;
                // from_str leaves origins unstamped: stamp every leaf
                // with the file label so the display can name it.
                restamp_origins(&mut map, &Some(format!("file:{}", path.display())));
                file_layer = Some(map);
                file_path = Some(path);
            }
            None => {
                if let Some(explicit) = std::env::var_os("DONSETCH_CONFIG") {
                    return Err(ConfigError::File {
                        path: explicit.to_string_lossy().into_owned(),
                        message: "not found".into(),
                    });
                }
            }
        }
    } else if let Some(explicit) = std::env::var_os("DONSETCH_CONFIG") {
        return Err(ConfigError::Env(format!(
            "DONSETCH_NO_CONFIG_FILE=1 but DONSETCH_CONFIG={} is set; drop one of them",
            explicit.to_string_lossy()
        )));
    }

    let (legacy_map, legacy_warnings) = legacy_layer();
    warnings.extend(legacy_warnings);
    let (env_map, env_warnings, env_errors) = new_env_layer();
    warnings.extend(env_warnings);
    if let Some(e) = env_errors.first() {
        return Err(ConfigError::Env(e.clone()));
    }

    // Validate the file layer on its own so unknown keys and bad types
    // fail loudly with the file path attached, exactly once.
    if let Some(file) = &file_layer {
        let path = file_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let source = deserialize_layer_table(file).map_err(|message| ConfigError::File {
            path: path.clone(),
            message: message.to_string(),
        })?;
        validate_modern_http_token(&source.transport.token).map_err(|message| {
            ConfigError::File {
                path,
                message: message.to_string(),
            }
        })?;
    }

    // The config crate's builder does the layering: sources are added
    // bottom-up (defaults < legacy < file < new env) and each leaf of
    // the merged tree carries the origin of the layer that won it.
    // Presence is preserved by the builder, so a layer setting a field
    // back to its default value still wins and still owns the origin.
    let mut builder = config::Config::builder();
    builder = builder.add_source(
        config::Config::try_from(&DonsetchConfig::default())
            .map_err(|e| ConfigError::Env(format!("serializing defaults: {e}")))?,
    );
    if !legacy_map.is_empty() {
        builder = builder.add_source(MapSource(legacy_map));
    }
    if let Some(file) = file_layer {
        builder = builder.add_source(MapSource(file));
    }
    if !env_map.is_empty() {
        builder = builder.add_source(MapSource(env_map));
    }
    let merged = builder
        .build()
        .map_err(|e| ConfigError::Env(e.to_string()))?;
    let raw: DonsetchConfig = merged
        .clone()
        .try_deserialize()
        .map_err(|e| ConfigError::Env(e.to_string()))?;

    validate(&raw)?;
    Ok(Loaded {
        config: raw,
        warnings,
        file: file_path,
        merged: merged
            .collect()
            .map_err(|e| ConfigError::Env(e.to_string()))?,
    })
}

/// Recursively restamp every leaf origin in a layer map: used for the
/// TOML file (from_str leaves origins unstamped).
fn restamp_origins(map: &mut config::Map<String, config::Value>, origin: &Option<String>) {
    for value in map.values_mut() {
        if let config::ValueKind::Table(inner) = &mut value.kind {
            restamp_origins(inner, origin);
        }
        let kind = std::mem::replace(&mut value.kind, config::ValueKind::Nil);
        *value = config::Value::new(origin.as_ref(), kind);
    }
}

#[derive(Debug, Clone)]
struct MapSource(config::Map<String, config::Value>);
impl config::Source for MapSource {
    fn collect(&self) -> Result<config::Map<String, config::Value>, config::ConfigError> {
        Ok(self.0.clone())
    }
    fn clone_into_box(&self) -> Box<dyn config::Source + Send + Sync> {
        Box::new(self.clone())
    }
}

fn deserialize_layer_table(
    map: &config::Map<String, config::Value>,
) -> Result<DonsetchConfig, ConfigError> {
    let builder = config::Config::builder().add_source(MapSource(map.clone()));
    builder
        .build()
        .and_then(|c| c.try_deserialize::<DonsetchConfig>())
        .map_err(|e| ConfigError::Env(e.to_string()))
}

/// The file layer path: `DONSETCH_CONFIG` explicit, else the default
/// location when the file exists. `None` = no file layer.
fn toml_path() -> Option<std::path::PathBuf> {
    if let Some(explicit) = std::env::var_os("DONSETCH_CONFIG") {
        return Some(std::path::PathBuf::from(explicit));
    }
    let dir = dirs::config_dir()?;
    let path = dir.join("donsetch").join("donsetch.toml");
    path.is_file().then_some(path)
}

// ---------------------------------------------------------------------------
// Legacy env names
// ---------------------------------------------------------------------------

type VMap = config::Map<String, config::Value>;

fn put(map: &mut VMap, path: &str, value: config::ValueKind, origin: &str) {
    let mut parts = path.split('.');
    let head = match parts.next() {
        Some(p) => p,
        None => return,
    };
    let origin = Some(origin.to_string());
    let oref = origin.as_ref();
    match parts.next() {
        None => {
            map.insert(head.to_string(), config::Value::new(oref, value));
        }
        Some(key) => {
            let section = map
                .entry(head.to_string())
                .or_insert_with(|| config::Value::new(None, config::ValueKind::Table(VMap::new())));
            if let config::ValueKind::Table(inner) = &mut section.kind {
                inner.insert(key.to_string(), config::Value::new(oref, value));
            }
        }
    }
}

/// Legacy strict opt-in flag: only `1`, `true`, `on` count.
fn legacy_flag(name: &str) -> bool {
    env_flag_value(std::env::var_os(name).as_deref())
}

fn legacy_layer() -> (VMap, Vec<String>) {
    let mut m = VMap::new();
    let mut warnings = Vec::new();
    let int_env = |name: &str| -> Option<i64> {
        std::env::var(name)
            .ok()
            .and_then(|v| v.trim().parse::<i64>().ok())
    };
    // Legacy numerics keep their historical soft semantics: a value
    // outside the sane range is ignored with a warning, never a hard
    // failure. (New-name env and TOML values stay strict.)
    let put_num = |map: &mut VMap,
                   warnings: &mut Vec<String>,
                   path: &str,
                   origin: &str,
                   n: i64,
                   lo: i64,
                   hi: i64| {
        if (lo..=hi).contains(&n) {
            put(map, path, n.into(), origin);
        } else {
            warnings.push(format!("ignoring {origin}={n}: out of range {lo}..={hi}"));
        }
    };

    // transport
    if std::env::var("DONSETCH_TRANSPORT").as_deref() == Ok("http") {
        put(
            &mut m,
            "transport.kind",
            "http".into(),
            "DONSETCH_TRANSPORT",
        );
    }
    if let Some(v) = std::env::var_os("DONSETCH_HTTP_HOST") {
        put(
            &mut m,
            "transport.host",
            v.to_string_lossy().into_owned().into(),
            "DONSETCH_HTTP_HOST",
        );
    }
    match int_env("DONSETCH_HTTP_PORT") {
        Some(n) => put_num(
            &mut m,
            &mut warnings,
            "transport.port",
            "DONSETCH_HTTP_PORT",
            n,
            1,
            65535,
        ),
        None if std::env::var_os("DONSETCH_HTTP_PORT").is_some() => {
            warnings.push("ignoring DONSETCH_HTTP_PORT: not a number".into())
        }
        None => {}
    }
    match std::env::var("DONSETCH_HTTP_TOKEN") {
        Ok(value) => put(
            &mut m,
            "transport.token",
            value.into(),
            "DONSETCH_HTTP_TOKEN",
        ),
        Err(std::env::VarError::NotUnicode(_)) => {
            warnings.push("ignoring DONSETCH_HTTP_TOKEN: not valid UTF-8".into())
        }
        Err(std::env::VarError::NotPresent) => {}
    }
    match int_env("DONSETCH_HTTP_TIMEOUT_SECS") {
        Some(n) => put_num(
            &mut m,
            &mut warnings,
            "transport.timeout_secs",
            "DONSETCH_HTTP_TIMEOUT_SECS",
            n,
            1,
            3600,
        ),
        None if std::env::var_os("DONSETCH_HTTP_TIMEOUT_SECS").is_some() => {
            warnings.push("ignoring DONSETCH_HTTP_TIMEOUT_SECS: not a number".into())
        }
        None => {}
    }
    if legacy_flag("DONSETCH_HTTP_CORS") {
        put(&mut m, "transport.cors", true.into(), "DONSETCH_HTTP_CORS");
    }

    // mcp
    if legacy_flag("DONSETCH_MCP_TEXT_ONLY") {
        put(
            &mut m,
            "mcp.text_only",
            true.into(),
            "DONSETCH_MCP_TEXT_ONLY",
        );
    }
    if let Some(v) = std::env::var_os("DONSETCH_URL_HANDLES") {
        // Historical semantics: every value except the exact string "off"
        // keeps url handles enabled.
        let on = v.to_str() != Some("off");
        put(&mut m, "mcp.url_handles", on.into(), "DONSETCH_URL_HANDLES");
    }

    // state
    if std::env::var_os("DONSEEK_NO_DISK_STATE").is_some() {
        put(
            &mut m,
            "state.no_disk_state",
            true.into(),
            "DONSEEK_NO_DISK_STATE",
        );
    }
    if legacy_flag("DONSETCH_NO_ROUTE_MEMORY") {
        put(
            &mut m,
            "state.route_memory",
            "off".into(),
            "DONSETCH_NO_ROUTE_MEMORY",
        );
    }
    if legacy_flag("DONSETCH_ROUTE_MEMORY_READONLY") {
        put(
            &mut m,
            "state.route_memory",
            "read_only".into(),
            "DONSETCH_ROUTE_MEMORY_READONLY",
        );
    }
    if legacy_flag("DONSETCH_NO_COOKIE_VAULT") {
        put(
            &mut m,
            "state.cookie_vault",
            false.into(),
            "DONSETCH_NO_COOKIE_VAULT",
        );
    }
    // fetch
    if legacy_flag("DONSETCH_ALLOW_PRIVATE_EGRESS") {
        put(
            &mut m,
            "fetch.allow_private_egress",
            true.into(),
            "DONSETCH_ALLOW_PRIVATE_EGRESS",
        );
    }
    if legacy_flag("DONSETCH_H3") {
        put(&mut m, "fetch.h3", true.into(), "DONSETCH_H3");
    }
    if legacy_flag("DONSETCH_NO_H3") {
        put(&mut m, "fetch.h3", false.into(), "DONSETCH_NO_H3");
    }
    if legacy_flag("DONSETCH_NO_ALT_SVC") {
        put(&mut m, "fetch.alt_svc", false.into(), "DONSETCH_NO_ALT_SVC");
    }
    if legacy_flag("DONSETCH_SHADOW_FETCH") {
        put(
            &mut m,
            "fetch.shadow_fetch",
            "always".into(),
            "DONSETCH_SHADOW_FETCH",
        );
    }
    if legacy_flag("DONSETCH_NO_SHADOW_FETCH") {
        put(
            &mut m,
            "fetch.shadow_fetch",
            "never".into(),
            "DONSETCH_NO_SHADOW_FETCH",
        );
    }
    match int_env("DONSETCH_SHADOW_DEADLINE_MS") {
        Some(n) => put_num(
            &mut m,
            &mut warnings,
            "fetch.shadow_deadline_ms",
            "DONSETCH_SHADOW_DEADLINE_MS",
            n,
            1,
            60000,
        ),
        None if std::env::var_os("DONSETCH_SHADOW_DEADLINE_MS").is_some() => {
            warnings.push("ignoring DONSETCH_SHADOW_DEADLINE_MS: not a number".into())
        }
        None => {}
    }
    if legacy_flag("DONSETCH_NO_CRAWL_SHAPE") {
        put(
            &mut m,
            "fetch.crawl_shape",
            false.into(),
            "DONSETCH_NO_CRAWL_SHAPE",
        );
    }
    if legacy_flag("DONSETCH_NO_PREWARM") {
        put(&mut m, "fetch.prewarm", false.into(), "DONSETCH_NO_PREWARM");
    }
    match int_env("DONSETCH_PDF_MAX_MB") {
        Some(n) => put_num(
            &mut m,
            &mut warnings,
            "fetch.pdf_max_mb",
            "DONSETCH_PDF_MAX_MB",
            n,
            1,
            4096,
        ),
        None if std::env::var_os("DONSETCH_PDF_MAX_MB").is_some() => {
            warnings.push("ignoring DONSETCH_PDF_MAX_MB: not a number".into())
        }
        None => {}
    }
    if let Some(v) = std::env::var_os("DONSHEET_OCR") {
        // Historical semantics: every value except the exact string "off"
        // keeps OCR enabled.
        let on = v.to_str() != Some("off");
        put(&mut m, "fetch.ocr", on.into(), "DONSHEET_OCR");
    }
    match int_env("DONSHEET_OCR_MAX_PAGES") {
        Some(n) => put_num(
            &mut m,
            &mut warnings,
            "fetch.ocr_max_pages",
            "DONSHEET_OCR_MAX_PAGES",
            n,
            1,
            500,
        ),
        None if std::env::var_os("DONSHEET_OCR_MAX_PAGES").is_some() => {
            warnings.push("ignoring DONSHEET_OCR_MAX_PAGES: not a number".into())
        }
        None => {}
    }
    // adapters: any value set (even "0") disables, historically.
    if std::env::var_os("DONSETCH_NO_ADAPTERS").is_some() {
        put(
            &mut m,
            "fetch.adapters",
            false.into(),
            "DONSETCH_NO_ADAPTERS",
        );
    }
    if let Some(v) = std::env::var_os("DONSETCH_ADAPTER_DUMP") {
        put(
            &mut m,
            "fetch.adapter_dump_dir",
            v.to_string_lossy().into_owned().into(),
            "DONSETCH_ADAPTER_DUMP",
        );
    }

    // bypass
    if let Some(v) = std::env::var_os("DONSETCH_BYPASS") {
        // Historical semantics: enabled unless the value is a falsy off-word.
        let off = v
            .to_str()
            .map(|s| {
                ["0", "false", "off", "no", ""].contains(&s.trim().to_ascii_lowercase().as_str())
            })
            .unwrap_or(false);
        put(&mut m, "bypass.enabled", (!off).into(), "DONSETCH_BYPASS");
    }
    if let Some(v) = std::env::var_os("DONSETCH_UNLOCKER_ZONE") {
        put(
            &mut m,
            "bypass.zone",
            v.to_string_lossy().into_owned().into(),
            "DONSETCH_UNLOCKER_ZONE",
        );
    }
    match int_env("DONSETCH_BYPASS_MAX_DAILY") {
        Some(n) => put_num(
            &mut m,
            &mut warnings,
            "bypass.max_daily",
            "DONSETCH_BYPASS_MAX_DAILY",
            n,
            1,
            10000,
        ),
        None if std::env::var_os("DONSETCH_BYPASS_MAX_DAILY").is_some() => {
            warnings.push("ignoring DONSETCH_BYPASS_MAX_DAILY: not a number".into())
        }
        None => {}
    }
    match int_env("DONSETCH_BYPASS_TIMEOUT_SECS") {
        Some(n) => put_num(
            &mut m,
            &mut warnings,
            "bypass.timeout_secs",
            "DONSETCH_BYPASS_TIMEOUT_SECS",
            n,
            1,
            3600,
        ),
        None if std::env::var_os("DONSETCH_BYPASS_TIMEOUT_SECS").is_some() => {
            warnings.push("ignoring DONSETCH_BYPASS_TIMEOUT_SECS: not a number".into())
        }
        None => {}
    }
    if std::env::var("DONSETCH_BYPASS_RENDER")
        .is_ok_and(|v| ["1", "true", "on", "yes"].contains(&v.trim().to_ascii_lowercase().as_str()))
    {
        put(
            &mut m,
            "bypass.render",
            true.into(),
            "DONSETCH_BYPASS_RENDER",
        );
    }
    if let Some(v) = std::env::var_os("DONSETCH_BYPASS_ENDPOINT") {
        put(
            &mut m,
            "bypass.endpoint",
            v.to_string_lossy().into_owned().into(),
            "DONSETCH_BYPASS_ENDPOINT",
        );
    }
    if let Some(v) = std::env::var_os("DONSETCH_BYPASS_CACHE") {
        let off = v
            .to_str()
            .map(|s| {
                ["0", "false", "off", "no", ""].contains(&s.trim().to_ascii_lowercase().as_str())
            })
            .unwrap_or(false);
        put(
            &mut m,
            "bypass.cache",
            (!off).into(),
            "DONSETCH_BYPASS_CACHE",
        );
    }
    match int_env("DONSETCH_BYPASS_CACHE_TTL_SECS") {
        Some(n) => put_num(
            &mut m,
            &mut warnings,
            "bypass.cache_ttl_secs",
            "DONSETCH_BYPASS_CACHE_TTL_SECS",
            n,
            1,
            86400,
        ),
        None if std::env::var_os("DONSETCH_BYPASS_CACHE_TTL_SECS").is_some() => {
            warnings.push("ignoring DONSETCH_BYPASS_CACHE_TTL_SECS: not a number".into())
        }
        None => {}
    }
    match int_env("DONSETCH_BYPASS_CACHE_MAX_ENTRIES") {
        Some(n) => put_num(
            &mut m,
            &mut warnings,
            "bypass.cache_max_entries",
            "DONSETCH_BYPASS_CACHE_MAX_ENTRIES",
            n,
            1,
            100000,
        ),
        None if std::env::var_os("DONSETCH_BYPASS_CACHE_MAX_ENTRIES").is_some() => {
            warnings.push("ignoring DONSETCH_BYPASS_CACHE_MAX_ENTRIES: not a number".into())
        }
        None => {}
    }

    // search
    if let Some(v) = std::env::var_os("DONSETCH_GOOGLE_PROFILE") {
        put(
            &mut m,
            "search.google_profile",
            v.to_string_lossy().into_owned().into(),
            "DONSETCH_GOOGLE_PROFILE",
        );
    }
    if std::env::var_os("DONSEEK_FORCE_GHOST_LANE").is_some() {
        put(
            &mut m,
            "search.ghost_lane",
            "always".into(),
            "DONSEEK_FORCE_GHOST_LANE",
        );
    }
    if std::env::var_os("DONSEEK_NO_GHOST_LANES").is_some() {
        put(
            &mut m,
            "search.ghost_lane",
            "never".into(),
            "DONSEEK_NO_GHOST_LANES",
        );
    }
    if std::env::var_os("DONSEEK_NO_TOPUP").is_some() {
        put(
            &mut m,
            "search.rerank_topup",
            false.into(),
            "DONSEEK_NO_TOPUP",
        );
    }
    match int_env("DONSEEK_RERANK_THREADS") {
        Some(n) => put_num(
            &mut m,
            &mut warnings,
            "search.rerank_threads",
            "DONSEEK_RERANK_THREADS",
            n,
            0,
            64,
        ),
        None if std::env::var_os("DONSEEK_RERANK_THREADS").is_some() => {
            warnings.push("ignoring DONSEEK_RERANK_THREADS: not a number".into())
        }
        None => {}
    }
    if let Some(v) = std::env::var_os("DONSETCH_BRIGHTDATA_ZONE") {
        put(
            &mut m,
            "search.brightdata_zone",
            v.to_string_lossy().into_owned().into(),
            "DONSETCH_BRIGHTDATA_ZONE",
        );
    }

    // browser
    if let Some(v) = std::env::var_os("DONSETCH_BROWSER_BACKEND")
        .or_else(|| std::env::var_os("DONGHOST_BROWSER_BACKEND"))
    {
        let v = v.to_string_lossy().to_ascii_lowercase();
        let backend = match v.as_str() {
            "auto" | "" => "auto",
            "chromium" | "chrome" | "original" => "chromium",
            "headless" | "original-headless" => "headless",
            "cloak" | "cloakbrowser" => "cloak",
            _ => {
                warnings.push(format!(
                    "ignoring unknown browser backend {v:?} (auto | chromium | headless | cloak)"
                ));
                ""
            }
        };
        if !backend.is_empty() {
            put(
                &mut m,
                "browser.backend",
                backend.into(),
                "DONSETCH_BROWSER_BACKEND",
            );
        }
    }
    if let Some(v) = std::env::var_os("DONGHOST_CHROME") {
        put(
            &mut m,
            "browser.chromium_path",
            v.to_string_lossy().into_owned().into(),
            "DONGHOST_CHROME",
        );
    }
    if std::env::var_os("DONGHOST_NO_SANDBOX").is_some_and(|v| v == "1") {
        put(
            &mut m,
            "browser.no_sandbox",
            true.into(),
            "DONGHOST_NO_SANDBOX",
        );
    }
    if std::env::var_os("DONSETCH_CLOAK_AUTO_DOWNLOAD").is_some_and(|v| v == "1") {
        put(
            &mut m,
            "browser.cloak_auto_download",
            true.into(),
            "DONSETCH_CLOAK_AUTO_DOWNLOAD",
        );
    }
    if let Some(v) = std::env::var_os("CLOAKBROWSER_BINARY_PATH") {
        put(
            &mut m,
            "browser.cloak_path",
            v.to_string_lossy().into_owned().into(),
            "CLOAKBROWSER_BINARY_PATH",
        );
    }
    if let Some(v) = std::env::var_os("CLOAKBROWSER_VERSION") {
        put(
            &mut m,
            "browser.cloak_version",
            v.to_string_lossy().into_owned().into(),
            "CLOAKBROWSER_VERSION",
        );
    }
    if let Some(v) = std::env::var_os("CLOAKBROWSER_CACHE_DIR") {
        put(
            &mut m,
            "browser.cloak_cache_dir",
            v.to_string_lossy().into_owned().into(),
            "CLOAKBROWSER_CACHE_DIR",
        );
    }
    let no_pool = std::env::var_os("DONSETCH_NO_GHOST_POOL").is_some();
    if no_pool {
        put(
            &mut m,
            "browser.pool_slots",
            1i64.into(),
            "DONSETCH_NO_GHOST_POOL",
        );
    } else {
        match int_env("DONSETCH_GHOST_POOL_SLOTS") {
            Some(n) => put_num(
                &mut m,
                &mut warnings,
                "browser.pool_slots",
                "DONSETCH_GHOST_POOL_SLOTS",
                n,
                0,
                16,
            ),
            None if std::env::var_os("DONSETCH_GHOST_POOL_SLOTS").is_some() => {
                warnings.push("ignoring DONSETCH_GHOST_POOL_SLOTS: not a number".into())
            }
            None => {}
        }
    }
    if let Some(v) = std::env::var_os("DONSETCH_XVFB_DISPLAY") {
        match v
            .to_str()
            .map(|s| s.trim().trim_start_matches(':').parse::<i64>())
        {
            Some(Ok(n)) if (0..=254).contains(&n) => put(
                &mut m,
                "browser.xvfb_display",
                n.into(),
                "DONSETCH_XVFB_DISPLAY",
            ),
            Some(Ok(n)) => warnings.push(format!(
                "ignoring DONSETCH_XVFB_DISPLAY={n}: out of range 0..=254"
            )),
            _ => warnings.push("ignoring DONSETCH_XVFB_DISPLAY: not a display number".into()),
        }
    }
    if legacy_flag("DONSETCH_NO_ROUTE_PROBES") {
        put(
            &mut m,
            "browser.route_probes",
            false.into(),
            "DONSETCH_NO_ROUTE_PROBES",
        );
    }
    match int_env("DONSETCH_PROBE_SCAN_SECS") {
        Some(n) => put_num(
            &mut m,
            &mut warnings,
            "browser.probe_scan_secs",
            "DONSETCH_PROBE_SCAN_SECS",
            n,
            1,
            3600,
        ),
        None if std::env::var_os("DONSETCH_PROBE_SCAN_SECS").is_some() => {
            warnings.push("ignoring DONSETCH_PROBE_SCAN_SECS: not a number".into())
        }
        None => {}
    }
    match int_env("DONSETCH_PROBE_STALE_SECS") {
        Some(n) => put_num(
            &mut m,
            &mut warnings,
            "browser.probe_stale_secs",
            "DONSETCH_PROBE_STALE_SECS",
            n,
            60,
            604800,
        ),
        None if std::env::var_os("DONSETCH_PROBE_STALE_SECS").is_some() => {
            warnings.push("ignoring DONSETCH_PROBE_STALE_SECS: not a number".into())
        }
        None => {}
    }
    if std::env::var_os("DONSETCH_LOGIN_FORCE").is_some() {
        put(
            &mut m,
            "browser.login_force",
            true.into(),
            "DONSETCH_LOGIN_FORCE",
        );
    }

    // proxy
    if legacy_flag("DONSETCH_NO_ENV_PROXY") {
        put(
            &mut m,
            "proxy.from_environment",
            false.into(),
            "DONSETCH_NO_ENV_PROXY",
        );
    }
    if let Ok(raw) = std::env::var("DONSEEK_PROXIES") {
        let oref = Some(&"DONSEEK_PROXIES".to_string());
        let pool = config::ValueKind::Array(
            raw.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| config::Value::new(oref, s))
                .collect(),
        );
        put(&mut m, "proxy.pool", pool, "DONSEEK_PROXIES");
    }

    // debug
    if std::env::var_os("DONGHOST_DEBUG").is_some() {
        put(&mut m, "debug.ghost", true.into(), "DONGHOST_DEBUG");
    }
    if std::env::var_os("DONSEEK_DEBUG").is_some() {
        put(&mut m, "debug.search", true.into(), "DONSEEK_DEBUG");
    }
    if std::env::var_os("DONSHEET_DEBUG").is_some() {
        put(&mut m, "debug.pdf", true.into(), "DONSHEET_DEBUG");
    }
    if std::env::var_os("DONSHEET_DEBUG_MATRIX").is_some() {
        put(
            &mut m,
            "debug.pdf_matrix",
            true.into(),
            "DONSHEET_DEBUG_MATRIX",
        );
    }
    if std::env::var_os("DONSHEET_DEBUG_CHARS").is_some() {
        put(
            &mut m,
            "debug.pdf_chars",
            true.into(),
            "DONSHEET_DEBUG_CHARS",
        );
    }
    if std::env::var_os("DONSHEET_DEBUG_WORDS").is_some() {
        put(
            &mut m,
            "debug.pdf_words",
            true.into(),
            "DONSHEET_DEBUG_WORDS",
        );
    }
    if std::env::var_os("DONSIFT_DEBUG").is_some() {
        put(&mut m, "debug.extract", true.into(), "DONSIFT_DEBUG");
    }
    if legacy_flag("DONSETCH_DEBUG_ECHO") {
        put(
            &mut m,
            "debug.echo_scorecard",
            true.into(),
            "DONSETCH_DEBUG_ECHO",
        );
    }

    (m, warnings)
}

// ---------------------------------------------------------------------------
// New env names: DONSETCH_<SECTION>__<KEY>
// ---------------------------------------------------------------------------

/// Field book: (section, key, kind, default display, description).
/// Single source of truth for env coercion, `config show` and the
/// generated markdown table.
pub(crate) type Fieldbook = [(
    &'static str,
    &'static str,
    FieldKind,
    &'static str,
    &'static str,
)];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FieldKind {
    Str,
    Bool,
    Int,
    List,
}

pub(crate) fn fieldbook() -> &'static Fieldbook {
    &[
        // transport
        (
            "transport",
            "kind",
            FieldKind::Str,
            "stdio",
            "stdio | http: which transport the daemon serves",
        ),
        (
            "transport",
            "host",
            FieldKind::Str,
            "127.0.0.1",
            "HTTP server bind address",
        ),
        (
            "transport",
            "port",
            FieldKind::Int,
            "8765",
            "HTTP server port",
        ),
        (
            "transport",
            "token",
            FieldKind::Str,
            "(empty)",
            "HTTP bearer token; empty = auth off; modern values use visible ASCII without whitespace",
        ),
        (
            "transport",
            "timeout_secs",
            FieldKind::Int,
            "300",
            "HTTP idle timeout in seconds",
        ),
        (
            "transport",
            "cors",
            FieldKind::Bool,
            "false",
            "send permissive CORS headers",
        ),
        // mcp
        (
            "mcp",
            "text_only",
            FieldKind::Bool,
            "false",
            "strip image blocks from tool output",
        ),
        (
            "mcp",
            "url_handles",
            FieldKind::Bool,
            "true",
            "return compact [H1] url handles in big results",
        ),
        // paths
        (
            "paths",
            "cache_dir",
            FieldKind::Str,
            "(platform default)",
            "override the cache/state root (fallback; env DONSETCH_CACHE_DIR still wins)",
        ),
        // state
        (
            "state",
            "no_disk_state",
            FieldKind::Bool,
            "false",
            "keep every persisted state in memory only (test hook)",
        ),
        (
            "state",
            "route_memory",
            FieldKind::Str,
            "readwrite",
            "off | readwrite | readonly: per-domain stealth route memory",
        ),
        (
            "state",
            "cookie_vault",
            FieldKind::Bool,
            "true",
            "persist tier-1/tier-2 clearance cookies",
        ),
        // proxy
        (
            "proxy",
            "from_environment",
            FieldKind::Bool,
            "true",
            "honor ambient HTTP(S)_PROXY/ALL_PROXY/NO_PROXY",
        ),
        (
            "proxy",
            "http",
            FieldKind::Str,
            "(ambient)",
            "explicit HTTP proxy; beats the ambient var",
        ),
        (
            "proxy",
            "https",
            FieldKind::Str,
            "(ambient)",
            "explicit HTTPS proxy; beats the ambient var",
        ),
        (
            "proxy",
            "all",
            FieldKind::Str,
            "(ambient)",
            "explicit proxy for both schemes",
        ),
        (
            "proxy",
            "no_proxy",
            FieldKind::Str,
            "(ambient)",
            "comma list of hosts to bypass the proxy",
        ),
        (
            "proxy",
            "pool",
            FieldKind::List,
            "(empty)",
            "extra proxy pool, comma-separated",
        ),
        // tls
        (
            "tls",
            "cert_file",
            FieldKind::Str,
            "(ambient)",
            "extra CA cert file (additive on top of the ambient set)",
        ),
        (
            "tls",
            "cert_dir",
            FieldKind::Str,
            "(ambient)",
            "extra CA cert directory (additive on top of the ambient set)",
        ),
        // persona
        (
            "persona",
            "timezone",
            FieldKind::Str,
            "(ambient TZ)",
            "timezone for persona headers",
        ),
        (
            "persona",
            "locale",
            FieldKind::Str,
            "(ambient LANG)",
            "locale for persona headers",
        ),
        // cli
        (
            "cli",
            "color",
            FieldKind::Str,
            "auto",
            "auto | never | always",
        ),
        // fetch
        (
            "fetch",
            "allow_private_egress",
            FieldKind::Bool,
            "false",
            "allow fetch to private/loopback addresses",
        ),
        (
            "fetch",
            "h3",
            FieldKind::Bool,
            "false",
            "opt-in HTTP/3 exit lane",
        ),
        (
            "fetch",
            "alt_svc",
            FieldKind::Bool,
            "true",
            "honor Alt-Svc upgrade hints",
        ),
        (
            "fetch",
            "shadow_fetch",
            FieldKind::Str,
            "auto",
            "auto | always | never: tier-1 shadow fetch lane",
        ),
        (
            "fetch",
            "shadow_deadline_ms",
            FieldKind::Int,
            "3000",
            "shadow-fetch burst deadline in ms",
        ),
        (
            "fetch",
            "adapters",
            FieldKind::Bool,
            "true",
            "enable data-only site adapters",
        ),
        (
            "fetch",
            "adapter_dump_dir",
            FieldKind::Str,
            "(unset)",
            "dump raw adapter payloads here for plugin development",
        ),
        (
            "fetch",
            "crawl_shape",
            FieldKind::Bool,
            "true",
            "crawl result shaping",
        ),
        (
            "fetch",
            "prewarm",
            FieldKind::Bool,
            "true",
            "pre-warm URLs found by search",
        ),
        (
            "fetch",
            "pdf_max_mb",
            FieldKind::Int,
            "100",
            "max PDF download size in MB (1..=4096)",
        ),
        (
            "fetch",
            "ocr",
            FieldKind::Bool,
            "true",
            "OCR arbitration for scanned PDFs",
        ),
        (
            "fetch",
            "ocr_max_pages",
            FieldKind::Int,
            "25",
            "max OCR pages per PDF (1..=500)",
        ),
        // bypass
        (
            "bypass",
            "enabled",
            FieldKind::Bool,
            "true",
            "unlocker bypass lane",
        ),
        (
            "bypass",
            "zone",
            FieldKind::Str,
            "(unset)",
            "Bright Data unlocker zone name",
        ),
        (
            "bypass",
            "max_daily",
            FieldKind::Int,
            "50",
            "max bypass solves per day (1..=10000)",
        ),
        (
            "bypass",
            "timeout_secs",
            FieldKind::Int,
            "120",
            "bypass solve timeout in seconds",
        ),
        (
            "bypass",
            "render",
            FieldKind::Bool,
            "false",
            "render inside bypass solves",
        ),
        (
            "bypass",
            "endpoint",
            FieldKind::Str,
            "(production)",
            "unlocker endpoint override",
        ),
        (
            "bypass",
            "cache",
            FieldKind::Bool,
            "true",
            "persist bypass solutions",
        ),
        (
            "bypass",
            "cache_ttl_secs",
            FieldKind::Int,
            "21600",
            "bypass solution TTL in seconds",
        ),
        (
            "bypass",
            "cache_max_entries",
            FieldKind::Int,
            "200",
            "bypass solution cache size",
        ),
        // search
        (
            "search",
            "google_profile",
            FieldKind::Str,
            "(unset)",
            "Google web-light profile id",
        ),
        (
            "search",
            "ghost_lane",
            FieldKind::Str,
            "auto",
            "auto | always | never: ghost lane for search results",
        ),
        (
            "search",
            "rerank_topup",
            FieldKind::Bool,
            "true",
            "rerank top-up pass",
        ),
        (
            "search",
            "rerank_threads",
            FieldKind::Int,
            "0 (auto)",
            "rerank thread count; 0 = auto, max 64",
        ),
        (
            "search",
            "brightdata_zone",
            FieldKind::Str,
            "(unset)",
            "BYOK Bright Data search zone",
        ),
        // browser
        (
            "browser",
            "backend",
            FieldKind::Str,
            "auto",
            "auto | chromium | cloak",
        ),
        (
            "browser",
            "chromium_path",
            FieldKind::Str,
            "(discovery)",
            "explicit Chromium/Chrome binary path",
        ),
        (
            "browser",
            "no_sandbox",
            FieldKind::Bool,
            "false",
            "run Chromium with --no-sandbox",
        ),
        (
            "browser",
            "cloak_auto_download",
            FieldKind::Bool,
            "false",
            "allow downloading the cloak browser",
        ),
        (
            "browser",
            "cloak_path",
            FieldKind::Str,
            "(discovery)",
            "explicit cloak browser binary",
        ),
        (
            "browser",
            "cloak_version",
            FieldKind::Str,
            "(latest)",
            "pin a cloak browser version",
        ),
        (
            "browser",
            "cloak_cache_dir",
            FieldKind::Str,
            "(platform default)",
            "cloak browser download cache",
        ),
        (
            "browser",
            "playwright_path",
            FieldKind::Str,
            "(ambient)",
            "extra Playwright browsers root",
        ),
        (
            "browser",
            "pool_slots",
            FieldKind::Int,
            "0 (auto)",
            "browser pool size; 1..=16, 0 = auto",
        ),
        (
            "browser",
            "xvfb_display",
            FieldKind::Int,
            "0 (auto)",
            "Xvfb display number; 0 = auto",
        ),
        (
            "browser",
            "route_probes",
            FieldKind::Bool,
            "true",
            "background route probing",
        ),
        (
            "browser",
            "probe_scan_secs",
            FieldKind::Int,
            "120",
            "probe scan interval in seconds",
        ),
        (
            "browser",
            "probe_stale_secs",
            FieldKind::Int,
            "21600",
            "probe result staleness in seconds",
        ),
        (
            "browser",
            "login_force",
            FieldKind::Bool,
            "false",
            "run login flows headless even without a display",
        ),
        // debug
        (
            "debug",
            "ghost",
            FieldKind::Bool,
            "false",
            "ghost/transport debug logging",
        ),
        (
            "debug",
            "search",
            FieldKind::Bool,
            "false",
            "search debug logging",
        ),
        (
            "debug",
            "pdf",
            FieldKind::Bool,
            "false",
            "pdf debug logging",
        ),
        (
            "debug",
            "pdf_matrix",
            FieldKind::Bool,
            "false",
            "pdf matrix debug",
        ),
        (
            "debug",
            "pdf_chars",
            FieldKind::Bool,
            "false",
            "pdf char dump",
        ),
        (
            "debug",
            "pdf_words",
            FieldKind::Bool,
            "false",
            "pdf word dump",
        ),
        (
            "debug",
            "extract",
            FieldKind::Bool,
            "false",
            "extraction debug logging",
        ),
        (
            "debug",
            "echo_scorecard",
            FieldKind::Bool,
            "false",
            "scorecard echo debug",
        ),
    ]
}

const RESERVED_VARS: &[&str] = &[
    "DONSETCH_CACHE_DIR",
    "DONSETCH_CONFIG",
    "DONSETCH_NO_CONFIG_FILE",
    "DONSETCH_PLUGIN",
    "DONSETCH_DEBUG",
    "BLESS_MCP_FIXTURES",
];

const LEGACY_VARS: &[&str] = &[
    "DONSETCH_TRANSPORT",
    "DONSETCH_HTTP_HOST",
    "DONSETCH_HTTP_PORT",
    "DONSETCH_HTTP_TOKEN",
    "DONSETCH_HTTP_TIMEOUT_SECS",
    "DONSETCH_HTTP_CORS",
    "DONSETCH_MCP_TEXT_ONLY",
    "DONSETCH_URL_HANDLES",
    "DONSEEK_NO_DISK_STATE",
    "DONSETCH_NO_ROUTE_MEMORY",
    "DONSETCH_ROUTE_MEMORY_READONLY",
    "DONSETCH_NO_COOKIE_VAULT",
    "DONSETCH_ALLOW_PRIVATE_EGRESS",
    "DONSETCH_H3",
    "DONSETCH_NO_H3",
    "DONSETCH_NO_ALT_SVC",
    "DONSETCH_SHADOW_FETCH",
    "DONSETCH_NO_SHADOW_FETCH",
    "DONSETCH_SHADOW_DEADLINE_MS",
    "DONSETCH_NO_ENV_PROXY",
    "DONSEEK_PROXIES",
    "DONSETCH_NO_ADAPTERS",
    "DONSETCH_ADAPTER_DUMP",
    "DONSETCH_NO_CRAWL_SHAPE",
    "DONSETCH_NO_PREWARM",
    "DONSETCH_PDF_MAX_MB",
    "DONSHEET_OCR",
    "DONSHEET_OCR_MAX_PAGES",
    "DONSETCH_UNLOCKER_ZONE",
    "DONSETCH_BYPASS_MAX_DAILY",
    "DONSETCH_BYPASS_TIMEOUT_SECS",
    "DONSETCH_BYPASS_RENDER",
    "DONSETCH_BYPASS_ENDPOINT",
    "DONSETCH_BYPASS_CACHE",
    "DONSETCH_BYPASS_CACHE_TTL_SECS",
    "DONSETCH_BYPASS_CACHE_MAX_ENTRIES",
    "DONSETCH_GOOGLE_PROFILE",
    "DONSEEK_FORCE_GHOST_LANE",
    "DONSEEK_NO_GHOST_LANES",
    "DONSEEK_NO_TOPUP",
    "DONSEEK_RERANK_THREADS",
    "DONSETCH_BRIGHTDATA_ZONE",
    "DONSETCH_BROWSER_BACKEND",
    "DONGHOST_BROWSER_BACKEND",
    "DONGHOST_CHROME",
    "DONGHOST_NO_SANDBOX",
    "DONSETCH_CLOAK_AUTO_DOWNLOAD",
    "CLOAKBROWSER_BINARY_PATH",
    "CLOAKBROWSER_VERSION",
    "CLOAKBROWSER_CACHE_DIR",
    "DONSETCH_NO_GHOST_POOL",
    "DONSETCH_GHOST_POOL_SLOTS",
    "DONSETCH_XVFB_DISPLAY",
    "DONSETCH_NO_ROUTE_PROBES",
    "DONSETCH_PROBE_SCAN_SECS",
    "DONSETCH_PROBE_STALE_SECS",
    "DONSETCH_LOGIN_FORCE",
    "DONGHOST_DEBUG",
    "DONSEEK_DEBUG",
    "DONSHEET_DEBUG",
    "DONSHEET_DEBUG_MATRIX",
    "DONSHEET_DEBUG_CHARS",
    "DONSHEET_DEBUG_WORDS",
    "DONSIFT_DEBUG",
    "DONSETCH_DEBUG_ECHO",
];

/// Every legacy knob still set in the environment (for doctor + show).
pub(crate) fn legacy_vars_in_env() -> Vec<&'static str> {
    LEGACY_VARS
        .iter()
        .copied()
        .filter(|name| std::env::var_os(name).is_some())
        .collect()
}

fn boolish(value: &str) -> Option<bool> {
    let v = value.trim();
    for on in ["1", "true", "on", "yes", "y"] {
        if v.eq_ignore_ascii_case(on) {
            return Some(true);
        }
    }
    for off in ["0", "false", "off", "no", "n"] {
        if v.eq_ignore_ascii_case(off) {
            return Some(false);
        }
    }
    None
}

fn new_env_layer() -> (VMap, Vec<String>, Vec<String>) {
    let mut map = VMap::new();
    let mut warnings = Vec::new();
    let mut errors = Vec::new();

    for (name, value) in std::env::vars_os() {
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with("DONSETCH_") || name == "DONSETCH_" {
            continue;
        }
        if RESERVED_VARS.contains(&name) || LEGACY_VARS.contains(&name) {
            continue;
        }
        let path = name["DONSETCH_".len()..].to_ascii_lowercase();
        let parts: Vec<&str> = path.split("__").collect();
        let (section, key) = if parts.len() == 2 {
            (parts[0], parts[1])
        } else {
            warnings.push(format!(
                "ignoring {name}: expected DONSETCH_<SECTION>__<KEY>"
            ));
            continue;
        };
        let kind = fieldbook()
            .iter()
            .find(|(s, k, _, _, _)| *s == section && *k == key)
            .map(|entry| entry.2);
        let Some(kind) = kind else {
            warnings.push(format!(
                "ignoring {name}: unknown knob (see `donsetch config show`)"
            ));
            continue;
        };
        let Some(value) = value.to_str() else {
            if (section, key) == ("transport", "token") {
                errors.push(format!("{name}: {MODERN_HTTP_TOKEN_REQUIREMENT}"));
            }
            continue;
        };
        if (section, key) == ("transport", "token")
            && let Err(message) = validate_modern_http_token(value)
        {
            errors.push(format!("{name}: {message}"));
            continue;
        }
        let vkind = match kind {
            FieldKind::Str => config::ValueKind::String(value.to_string()),
            FieldKind::Bool => match boolish(value) {
                Some(b) => b.into(),
                None => {
                    errors.push(format!(
                        "{name}={value:?} is not a boolean (1/true/on/yes/0/false/off/no)"
                    ));
                    continue;
                }
            },
            FieldKind::Int => match value.trim().parse::<i64>() {
                Ok(n) => n.into(),
                Err(_) => {
                    errors.push(format!("{name}={value:?} is not an integer"));
                    continue;
                }
            },
            FieldKind::List => value
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .into(),
        };
        put(&mut map, &format!("{section}.{key}"), vkind, name);
    }

    (map, warnings, errors)
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

fn validate(c: &DonsetchConfig) -> Result<(), ConfigError> {
    let err = |msg: String| Err(ConfigError::Validate(msg));
    if c.transport.port == 0 {
        return err("transport.port = 0 (valid 1..=65535)".into());
    }
    if c.transport.timeout_secs == 0 || c.transport.timeout_secs > 3600 {
        return err("transport.timeout_secs must be 1..=3600".into());
    }
    if c.bypass.max_daily == 0 || c.bypass.max_daily > 10_000 {
        return err("bypass.max_daily must be 1..=10_000".into());
    }
    if c.bypass.timeout_secs == 0 || c.bypass.timeout_secs > 3600 {
        return err("bypass.timeout_secs must be 1..=3600".into());
    }
    if c.bypass.cache_ttl_secs == 0 || c.bypass.cache_ttl_secs > 86_400 {
        return err("bypass.cache_ttl_secs must be 1..=86_400".into());
    }
    if c.bypass.cache_max_entries == 0 || c.bypass.cache_max_entries > 100_000 {
        return err("bypass.cache_max_entries must be 1..=100_000".into());
    }
    if !(1..=4096).contains(&c.fetch.pdf_max_mb) {
        return err("fetch.pdf_max_mb must be 1..=4096".into());
    }
    if c.fetch.ocr_max_pages == 0 || c.fetch.ocr_max_pages > 500 {
        return err("fetch.ocr_max_pages must be 1..=500".into());
    }
    if c.fetch.shadow_deadline_ms == 0 || c.fetch.shadow_deadline_ms > 60_000 {
        return err("fetch.shadow_deadline_ms must be 1..=60_000".into());
    }
    if c.browser.pool_slots > 16 {
        return err("browser.pool_slots must be 0 (auto) or 1..=16".into());
    }
    if c.browser.xvfb_display > 254 {
        return err("browser.xvfb_display must be 0 (auto) or 1..=254".into());
    }
    if c.browser.probe_scan_secs == 0 || c.browser.probe_scan_secs > 3600 {
        return err("browser.probe_scan_secs must be 1..=3600".into());
    }
    if c.browser.probe_stale_secs < 60 || c.browser.probe_stale_secs > 604_800 {
        return err("browser.probe_stale_secs must be 60..=604800".into());
    }
    if c.search.rerank_threads > 64 {
        return err("search.rerank_threads must be 0 (auto) or 1..=64".into());
    }
    Ok(())
}

/// Resolve the exact bearer-token contract used by the HTTP transport.
/// Empty means auth off; every non-empty loaded value remains auth-on and is
/// compared byte-for-byte for backward compatibility.
pub(crate) fn configured_http_token(token: &str) -> Option<&str> {
    (!token.is_empty()).then_some(token)
}

/// Modern sources are strict: an explicit empty string disables auth, while
/// configured tokens must fit safely and unambiguously in an HTTP header.
/// Never include the token itself in the error returned to diagnostics.
const MODERN_HTTP_TOKEN_REQUIREMENT: &str =
    "transport.token must be empty or contain only visible ASCII without whitespace";

fn validate_modern_http_token(token: &str) -> Result<(), &'static str> {
    if configured_http_token(token)
        .is_some_and(|token| !token.bytes().all(|byte| byte.is_ascii_graphic()))
    {
        return Err(MODERN_HTTP_TOKEN_REQUIREMENT);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Show: origins, human table, markdown table
// ---------------------------------------------------------------------------

fn value_of(c: &DonsetchConfig, section: &str, key: &str) -> String {
    let json = serde_json::to_value(c).unwrap_or(serde_json::Value::Null);
    match json.get(section).and_then(|s| s.get(key)) {
        Some(serde_json::Value::Bool(b)) => b.to_string(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        Some(serde_json::Value::String(s)) => format!("\"{s}\""),
        Some(serde_json::Value::Array(a)) => {
            let parts: Vec<String> = a
                .iter()
                .map(|v| match v {
                    serde_json::Value::String(s) => format!("\"{s}\""),
                    other => other.to_string(),
                })
                .collect();
            format!("[{}]", parts.join(", "))
        }
        _ => "(unknown)".into(),
    }
}

/// (section, key, current value, origin) for every documented knob:
/// the origin comes straight off each leaf of the merged tree (the
/// builder stamps which layer won), the value from the typed struct.
pub fn origins(
    merged: &config::Map<String, config::Value>,
    c: &DonsetchConfig,
) -> Vec<(&'static str, &'static str, String, String)> {
    let mut out = Vec::new();
    for (section, key, _, _, _) in fieldbook() {
        let section = *section;
        let key = *key;
        let origin = merged
            .get(section)
            .and_then(|v| match &v.kind {
                config::ValueKind::Table(inner) => inner.get(key),
                _ => None,
            })
            .and_then(config::Value::origin)
            .map(str::to_string)
            .unwrap_or_else(|| "default".to_string());
        out.push((section, key, value_of(c, section, key), origin));
    }
    out
}

/// Knobs whose values can carry credentials. Proxy URLs embed
/// user:password; `config show` masks the secret part so terminal
/// scrollback and pasted bug reports never leak it.
fn is_secret(section: &str, key: &str) -> bool {
    matches!(
        (section, key),
        ("transport", "token") | ("proxy", "http" | "https" | "all" | "pool")
    )
}

/// Mask the userinfo of a URL credential: scheme://user:pass@host
/// becomes scheme://***@host. Values without userinfo are not
/// secrets and show as-is; bare non-URL values (never expected in
/// these fields) mask fully rather than risk a leak.
fn mask_url_secret(url: &str) -> String {
    if let Some(scheme_end) = url.find("://") {
        let rest = &url[scheme_end + 3..];
        if let Some(at) = rest.rfind('@') {
            return format!("{}://***@{}", &url[..scheme_end], &rest[at + 1..]);
        }
        return url.to_string();
    }
    if url.is_empty() {
        String::new()
    } else {
        "***".to_string()
    }
}

/// `config show` display form for one knob: value_of output for
/// everything, credentials masked for the secret proxy fields.
fn masked_value(c: &DonsetchConfig, section: &str, key: &str, shown: String) -> String {
    if !is_secret(section, key) {
        return shown;
    }
    if section == "transport" {
        return if c.transport.token.is_empty() {
            shown
        } else {
            "\"***\"".to_string()
        };
    }
    if key == "pool" {
        let masked = c
            .proxy
            .pool
            .iter()
            .map(|p| format!("\"{}\"", mask_url_secret(p)))
            .collect::<Vec<_>>()
            .join(", ");
        return format!("[{masked}]");
    }
    let raw = match key {
        "http" => c.proxy.http.as_str(),
        "https" => c.proxy.https.as_str(),
        _ => c.proxy.all.as_str(),
    };
    format!("\"{}\"", mask_url_secret(raw))
}

/// Human-readable `config show` output: every knob, value, source.
pub fn show_text(loaded: &Loaded) -> String {
    let mut out = String::new();
    if let Some(path) = &loaded.file {
        out.push_str(&format!("config file: {}\n", path.display()));
    } else {
        out.push_str("config file: (none, using env + defaults)\n");
    }
    let legacy = legacy_vars_in_env();
    if !legacy.is_empty() {
        out.push_str(&format!(
            "legacy env vars detected: {} (still honored; migrate to the keys in `donsetch config show --legacy`)\n",
            legacy.join(", ")
        ));
    }
    out.push('\n');
    let rows = origins(&loaded.merged, &loaded.config);
    let mut last_section = "";
    for (section, key, value, origin) in rows {
        if section != last_section {
            if !last_section.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!("[{section}]\n"));
            last_section = section;
        }
        out.push_str(&format!(
            "  {key:<18} = {} ({origin})\n",
            masked_value(&loaded.config, section, key, value)
        ));
    }
    out
}

/// The knob reference table as markdown (single source: this file).
pub fn show_markdown() -> String {
    let mut out = String::new();
    out.push_str("| Key | Type | Default | What it does |\n|---|---|---|---|\n");
    let mut last_section = "";
    for (section, key, kind, default, doc) in fieldbook() {
        let section = *section;
        let kind = *kind;
        let type_str = match kind {
            FieldKind::Str => "string",
            FieldKind::Bool => "bool",
            FieldKind::Int => "integer",
            FieldKind::List => "list",
        };
        if section != last_section {
            out.push_str(&format!("\n**`[{section}]`**\n\n"));
            last_section = section;
        }
        let esc = |s: &str| s.replace('|', "\\|");
        out.push_str(&format!(
            "| `{key}` | {type_str} | {} | {} |\n",
            esc(default),
            esc(doc)
        ));
    }
    out
}

/// The legacy -> new mapping as markdown.
pub fn show_legacy_markdown() -> String {
    let mut out = String::new();
    out.push_str("| Legacy env var | New key | Still honored? |\n|---|---|---|\n");
    for name in LEGACY_VARS {
        let (section, key) = legacy_target_of(name);
        out.push_str(&format!(
            "| `{name}` | `{section}.{key}` | yes (deprecated, cut at v4) |\n"
        ));
    }
    out
}

/// Best-effort mapping of a legacy var name to its config key (for
/// doctor output and `--legacy` docs; mirrors legacy_layer()).
pub(crate) fn legacy_target_of(name: &str) -> (&'static str, &'static str) {
    match name {
        "DONSETCH_TRANSPORT" => ("transport", "kind"),
        "DONSETCH_HTTP_HOST" => ("transport", "host"),
        "DONSETCH_HTTP_PORT" => ("transport", "port"),
        "DONSETCH_HTTP_TOKEN" => ("transport", "token"),
        "DONSETCH_HTTP_TIMEOUT_SECS" => ("transport", "timeout_secs"),
        "DONSETCH_HTTP_CORS" => ("transport", "cors"),
        "DONSETCH_MCP_TEXT_ONLY" => ("mcp", "text_only"),
        "DONSETCH_URL_HANDLES" => ("mcp", "url_handles"),
        "DONSEEK_NO_DISK_STATE" => ("state", "no_disk_state"),
        "DONSETCH_NO_ROUTE_MEMORY" => ("state", "route_memory"),
        "DONSETCH_ROUTE_MEMORY_READONLY" => ("state", "route_memory"),
        "DONSETCH_NO_COOKIE_VAULT" => ("state", "cookie_vault"),
        "DONSETCH_ALLOW_PRIVATE_EGRESS" => ("fetch", "allow_private_egress"),
        "DONSETCH_H3" => ("fetch", "h3"),
        "DONSETCH_NO_H3" => ("fetch", "h3"),
        "DONSETCH_NO_ALT_SVC" => ("fetch", "alt_svc"),
        "DONSETCH_SHADOW_FETCH" => ("fetch", "shadow_fetch"),
        "DONSETCH_NO_SHADOW_FETCH" => ("fetch", "shadow_fetch"),
        "DONSETCH_SHADOW_DEADLINE_MS" => ("fetch", "shadow_deadline_ms"),
        "DONSETCH_NO_ENV_PROXY" => ("proxy", "from_environment"),
        "DONSEEK_PROXIES" => ("proxy", "pool"),
        "DONSETCH_NO_ADAPTERS" => ("fetch", "adapters"),
        "DONSETCH_ADAPTER_DUMP" => ("fetch", "adapter_dump_dir"),
        "DONSETCH_NO_CRAWL_SHAPE" => ("fetch", "crawl_shape"),
        "DONSETCH_NO_PREWARM" => ("fetch", "prewarm"),
        "DONSETCH_PDF_MAX_MB" => ("fetch", "pdf_max_mb"),
        "DONSHEET_OCR" => ("fetch", "ocr"),
        "DONSHEET_OCR_MAX_PAGES" => ("fetch", "ocr_max_pages"),
        "DONSETCH_UNLOCKER_ZONE" => ("bypass", "zone"),
        "DONSETCH_BYPASS_MAX_DAILY" => ("bypass", "max_daily"),
        "DONSETCH_BYPASS_TIMEOUT_SECS" => ("bypass", "timeout_secs"),
        "DONSETCH_BYPASS_RENDER" => ("bypass", "render"),
        "DONSETCH_BYPASS_ENDPOINT" => ("bypass", "endpoint"),
        "DONSETCH_BYPASS_CACHE" => ("bypass", "cache"),
        "DONSETCH_BYPASS_CACHE_TTL_SECS" => ("bypass", "cache_ttl_secs"),
        "DONSETCH_BYPASS_CACHE_MAX_ENTRIES" => ("bypass", "cache_max_entries"),
        "DONSETCH_GOOGLE_PROFILE" => ("search", "google_profile"),
        "DONSEEK_FORCE_GHOST_LANE" => ("search", "ghost_lane"),
        "DONSEEK_NO_GHOST_LANES" => ("search", "ghost_lane"),
        "DONSEEK_NO_TOPUP" => ("search", "rerank_topup"),
        "DONSEEK_RERANK_THREADS" => ("search", "rerank_threads"),
        "DONSETCH_BRIGHTDATA_ZONE" => ("search", "brightdata_zone"),
        "DONSETCH_BROWSER_BACKEND" | "DONGHOST_BROWSER_BACKEND" => ("browser", "backend"),
        "DONGHOST_CHROME" => ("browser", "chromium_path"),
        "DONGHOST_NO_SANDBOX" => ("browser", "no_sandbox"),
        "DONSETCH_CLOAK_AUTO_DOWNLOAD" => ("browser", "cloak_auto_download"),
        "CLOAKBROWSER_BINARY_PATH" => ("browser", "cloak_path"),
        "CLOAKBROWSER_VERSION" => ("browser", "cloak_version"),
        "CLOAKBROWSER_CACHE_DIR" => ("browser", "cloak_cache_dir"),
        "DONSETCH_NO_GHOST_POOL" => ("browser", "pool_slots"),
        "DONSETCH_GHOST_POOL_SLOTS" => ("browser", "pool_slots"),
        "DONSETCH_XVFB_DISPLAY" => ("browser", "xvfb_display"),
        "DONSETCH_NO_ROUTE_PROBES" => ("browser", "route_probes"),
        "DONSETCH_PROBE_SCAN_SECS" => ("browser", "probe_scan_secs"),
        "DONSETCH_PROBE_STALE_SECS" => ("browser", "probe_stale_secs"),
        "DONSETCH_LOGIN_FORCE" => ("browser", "login_force"),
        "DONGHOST_DEBUG" => ("debug", "ghost"),
        "DONSEEK_DEBUG" => ("debug", "search"),
        "DONSHEET_DEBUG" => ("debug", "pdf"),
        "DONSHEET_DEBUG_MATRIX" => ("debug", "pdf_matrix"),
        "DONSHEET_DEBUG_CHARS" => ("debug", "pdf_chars"),
        "DONSHEET_DEBUG_WORDS" => ("debug", "pdf_words"),
        "DONSIFT_DEBUG" => ("debug", "extract"),
        "DONSETCH_DEBUG_ECHO" => ("debug", "echo_scorecard"),
        _ => ("", ""),
    }
}

// ---------------------------------------------------------------------------
// The global
// ---------------------------------------------------------------------------

static CONFIG: OnceLock<DonsetchConfig> = OnceLock::new();

/// Install a validated config (the CLI/MCP entry point does this after
/// merging CLI flags). Errors are fatal and reported to the caller.
/// Installing twice in one process is an error, not a silent no-op:
/// the first install is the contract the whole daemon reads.
pub fn install(cfg: DonsetchConfig) -> Result<(), ConfigError> {
    validate(&cfg)?;
    CONFIG
        .set(cfg)
        .map_err(|_| ConfigError::Env("config is already installed in this process".into()))
}

/// The process config, initializing lazily from defaults + env + file.
///
/// Errors at this path cannot be fatal (no caller context), so the error
/// and the default config are reported to stderr and the default is used;
/// the binary entry points call `load()` + `install()` first so real users
/// always get the loud path.
pub fn cfg() -> &'static DonsetchConfig {
    CONFIG.get_or_init(|| match load() {
        Ok(loaded) => {
            for w in &loaded.warnings {
                eprintln!("[donsetch config] {w}");
            }
            loaded.config
        }
        Err(e) => {
            eprintln!("[donsetch config] {e}; using defaults");
            DonsetchConfig::default()
        }
    })
}

// ---------------------------------------------------------------------------
// Shared parsers (kept for tests + non-config uses)
// ---------------------------------------------------------------------------

/// Strict opt-in flag parser: only `1`, `true`, `on` count.
pub(crate) fn env_flag_value(value: Option<&std::ffi::OsStr>) -> bool {
    value
        .and_then(|v| v.to_str())
        .map(str::trim)
        .is_some_and(|value| {
            value.eq_ignore_ascii_case("1")
                || value.eq_ignore_ascii_case("true")
                || value.eq_ignore_ascii_case("on")
        })
}

/// Write a file that holds credentials (API keys, proxy passwords)
/// so that it is owner-only from the moment it exists.
///
/// `fs::write` followed by `set_permissions` creates the file at the
/// umask default (0644 on most systems) and only tightens it after
/// the secret has already landed: a world-readable window, and a
/// world-readable file for good if anything fails in between. This
/// opens with mode 0600 up front, then also re-applies 0600 for the
/// case where the file already existed with looser permissions
/// (`mode` on open only affects creation). Non-Unix: plain write.
pub(crate) fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::fs::PermissionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        f.write_all(bytes)?;
        f.flush()
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_env(name: &str, value: impl AsRef<std::ffi::OsStr>) {
        unsafe { std::env::set_var(name, value) }
    }

    fn unset_env(name: &str) {
        unsafe { std::env::remove_var(name) }
    }

    /// Snapshot-and-clear every donsetch env var (process-per-test makes
    /// this safe; the guard restores the shell on drop so the dev's own
    /// exports never leak into a sibling test process image).
    struct EnvGuard {
        saved: Vec<(String, Option<std::ffi::OsString>)>,
    }

    fn clean_env() -> EnvGuard {
        let mut saved = Vec::new();
        let prefixes = [
            "DONSETCH_",
            "DONSEEK_",
            "DONGHOST_",
            "DONSHEET_",
            "DONSIFT_",
            "CLOAKBROWSER_",
        ];
        // Snapshot first: mutating the env while iterating vars_os
        // deadlocks on the env lock.
        let snapshot: Vec<(String, std::ffi::OsString)> = std::env::vars_os()
            .filter_map(|(k, v)| {
                let name = k.to_str()?.to_string();
                prefixes
                    .iter()
                    .any(|p| name.starts_with(p))
                    .then_some((name, v))
            })
            .collect();
        for (name, v) in snapshot {
            unset_env(&name);
            saved.push((name, Some(v)));
        }
        EnvGuard { saved }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (name, value) in &self.saved {
                match value {
                    Some(v) => set_env(name, v),
                    None => unset_env(name),
                }
            }
        }
    }

    fn write_cfg(tmp: &std::path::Path, text: &str) -> std::path::PathBuf {
        let dir = tmp.join(format!("cfg-{}-{}", std::process::id(), rand_suffix()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("donsetch.toml");
        std::fs::write(&path, text).unwrap();
        path
    }

    fn rand_suffix() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            .to_string()
    }

    /// Presence beats default-equality: a top layer that sets a field to
    /// its own default is still a setting and must win (the old value-
    /// difference fold dropped it and lied about the origin).
    #[test]
    fn layer_fold_is_by_presence_not_value() {
        let guard = clean_env();
        let path = write_cfg(&std::env::temp_dir(), "[fetch]\nprewarm = false\n");
        set_env("DONSETCH_CONFIG", &path);
        set_env("DONSETCH_FETCH__PREWARM", "true");
        let loaded = load().expect("load");
        assert!(
            loaded.config.fetch.prewarm,
            "env set it true explicitly; presence must beat default-equality"
        );
        let rows = origins(&loaded.merged, &loaded.config);
        let origin = rows
            .iter()
            .find(|(s, k, _, _)| *s == "fetch" && *k == "prewarm")
            .expect("prewarm row")
            .3
            .clone();
        assert_eq!(origin, "DONSETCH_FETCH__PREWARM");
        drop(guard);
    }

    /// The documented order is defaults < legacy < file < new env; the
    /// implementation folded legacy AFTER the file, so a legacy value
    /// beat the TOML. Pinned now.
    #[test]
    fn legacy_layer_loses_to_the_file() {
        let guard = clean_env();
        let path = write_cfg(&std::env::temp_dir(), "[fetch]\npdf_max_mb = 44\n");
        set_env("DONSETCH_CONFIG", &path);
        set_env("DONSETCH_PDF_MAX_MB", "7");
        let loaded = load().expect("load");
        assert_eq!(
            loaded.config.fetch.pdf_max_mb, 44,
            "the file layer must beat the legacy env name"
        );
        drop(guard);
    }

    /// Legacy names keep their historical soft semantics: a value
    /// outside the sane range warns and keeps the default, never a hard
    /// failure (review finding: both caps used to exit 1).
    #[test]
    fn legacy_out_of_range_warns_and_keeps_the_default() {
        let guard = clean_env();
        set_env("DONSETCH_NO_CONFIG_FILE", "1");
        set_env("DONSETCH_PDF_MAX_MB", "-1");
        set_env("DONSETCH_HTTP_PORT", "99999");
        let loaded = load().expect("legacy values must never fail hard");
        assert_eq!(loaded.config.fetch.pdf_max_mb, 100);
        assert_eq!(loaded.config.transport.port, 8765);
        assert!(
            loaded
                .warnings
                .iter()
                .any(|w| w.contains("DONSETCH_HTTP_PORT=99999"))
        );
        assert!(
            loaded
                .warnings
                .iter()
                .any(|w| w.contains("DONSETCH_PDF_MAX_MB=-1"))
        );
        drop(guard);
    }

    /// proxy.from_environment gates the AMBIENT proxy convention, not
    /// the config-file slots: an explicit TOML proxy must survive the
    /// kill switch (the old caller-side gate starved it).
    ///
    /// Isolation note: relies on nextest's process-per-test runner
    /// (env mutation + the process-wide cfg() OnceLock: the first
    /// cfg() call freezes the config layer, so keep this out of
    /// shared-process runners).
    #[test]
    fn from_environment_off_keeps_toml_proxy_slots_alive() {
        let guard = clean_env();
        let path = write_cfg(
            &std::env::temp_dir(),
            "[proxy]\nhttp = \"http://unlocker.local:3128\"\nfrom_environment = false\n",
        );
        set_env("DONSETCH_CONFIG", &path);
        let loaded = load().expect("load");
        assert!(!loaded.config.proxy.from_environment);
        assert_eq!(loaded.config.proxy.http, "http://unlocker.local:3128");
        // The resolver must see the slot even with the ambient gate off.
        let picked = crate::transport::proxy::from_env_for("http://example.com");
        assert!(
            picked.is_some(),
            "an explicit TOML proxy must survive from_environment=false"
        );
        drop(guard);
    }

    /// A file that merely exists is not a setter for every key: only the
    /// keys present in it carry the file origin (the old code stamped
    /// 72/74 rows as file:).
    #[test]
    fn file_origin_reports_only_present_keys() {
        let guard = clean_env();
        let path = write_cfg(&std::env::temp_dir(), "[fetch]\nh3 = true\n");
        set_env("DONSETCH_CONFIG", &path);
        let loaded = load().expect("load");
        let rows = origins(&loaded.merged, &loaded.config);
        let file_rows: Vec<&(_, _, _, _)> =
            rows.iter().filter(|r| r.3.starts_with("file:")).collect();
        assert_eq!(file_rows.len(), 1, "exactly one key came from the file");
        assert_eq!(file_rows[0].0, "fetch");
        assert_eq!(file_rows[0].1, "h3");
        let defaulted = rows.iter().filter(|r| !r.3.starts_with("file:")).count();
        assert!(defaulted >= 70, "everything else stays default-origin");
        drop(guard);
    }

    /// The legacy backend mapper dropped the headless variant; it must
    /// map to BrowserBackend::Headless like every historical spelling.
    #[test]
    fn legacy_headless_backend_still_maps() {
        let guard = clean_env();
        set_env("DONSETCH_NO_CONFIG_FILE", "1");
        set_env("DONSETCH_BROWSER_BACKEND", "headless");
        let loaded = load().expect("load");
        assert_eq!(loaded.config.browser.backend, BrowserBackend::Headless);
        assert!(
            loaded
                .warnings
                .iter()
                .all(|w| !w.contains("unknown browser backend")),
            "headless is a known backend: {warnings:?}",
            warnings = loaded.warnings
        );
        drop(guard);
    }

    /// Kill switch wins: DONSETCH_NO_GHOST_POOL must override a slot
    /// count, exactly like the pre-config behavior.
    #[test]
    fn ghost_pool_kill_switch_wins_over_slot_count() {
        let guard = clean_env();
        set_env("DONSETCH_NO_CONFIG_FILE", "1");
        set_env("DONSETCH_NO_GHOST_POOL", "1");
        set_env("DONSETCH_GHOST_POOL_SLOTS", "4");
        let loaded = load().expect("load");
        assert_eq!(
            loaded.config.browser.pool_slots, 1,
            "the kill switch must beat the count"
        );
        drop(guard);
    }

    /// Falsy spellings of DONSETCH_NO_CONFIG_FILE mean "the file layer
    /// is on": only truthy values disable it.
    #[test]
    fn no_config_file_falsy_words_still_load_the_file() {
        for falsy in ["0", "false", "off", "no"] {
            let guard = clean_env();
            let path = write_cfg(&std::env::temp_dir(), "[fetch]\nh3 = true\n");
            set_env("DONSETCH_CONFIG", &path);
            set_env("DONSETCH_NO_CONFIG_FILE", falsy);
            let loaded = load().expect("load");
            assert!(
                loaded.config.fetch.h3,
                "DONSETCH_NO_CONFIG_FILE={falsy} must not disable the file layer"
            );
            drop(guard);
        }
    }

    #[test]
    fn modern_http_tokens_are_strict_and_errors_never_echo_them() {
        let guard = clean_env();
        let file_secret = "file token sentinel";
        let path = write_cfg(
            &std::env::temp_dir(),
            &format!("[transport]\ntoken = {file_secret:?}\n"),
        );
        set_env("DONSETCH_CONFIG", &path);
        let error = load()
            .err()
            .expect("whitespace token must fail")
            .to_string();
        assert!(error.contains("transport.token"), "{error}");
        assert!(!error.contains(file_secret), "file token leaked: {error}");

        unset_env("DONSETCH_CONFIG");
        set_env("DONSETCH_NO_CONFIG_FILE", "1");
        let env_secret = "environment token sentinel";
        set_env("DONSETCH_TRANSPORT__TOKEN", env_secret);
        let error = load()
            .err()
            .expect("whitespace token must fail")
            .to_string();
        assert!(error.contains("DONSETCH_TRANSPORT__TOKEN"), "{error}");
        assert!(!error.contains(env_secret), "env token leaked: {error}");

        set_env("DONSETCH_TRANSPORT__TOKEN", "   ");
        assert!(
            load().is_err(),
            "whitespace-only token must not disable auth"
        );

        set_env("DONSETCH_TRANSPORT__TOKEN", "Abc-._~+/=:");
        let loaded = load().expect("visible ASCII token");
        assert_eq!(loaded.config.transport.token, "Abc-._~+/=:");

        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;

            set_env(
                "DONSETCH_TRANSPORT__TOKEN",
                std::ffi::OsString::from_vec(b"modern-\xff-token".to_vec()),
            );
            let error = load().err().expect("non-UTF-8 token must fail").to_string();
            assert!(error.contains("DONSETCH_TRANSPORT__TOKEN"), "{error}");
            assert!(error.contains("visible ASCII"), "{error}");
        }
        drop(guard);
    }

    #[test]
    fn legacy_http_token_value_survives_and_modern_empty_can_disable_auth() {
        let guard = clean_env();
        set_env("DONSETCH_NO_CONFIG_FILE", "1");
        set_env("DONSETCH_HTTP_TOKEN", " padded legacy token ");
        let loaded = load().expect("legacy token");
        assert_eq!(
            configured_http_token(&loaded.config.transport.token),
            Some(" padded legacy token ")
        );

        set_env("DONSETCH_TRANSPORT__TOKEN", "");
        let loaded = load().expect("modern empty override");
        assert_eq!(loaded.config.transport.token, "");
        assert_eq!(configured_http_token(&loaded.config.transport.token), None);

        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;

            unset_env("DONSETCH_TRANSPORT__TOKEN");
            set_env(
                "DONSETCH_HTTP_TOKEN",
                std::ffi::OsString::from_vec(b"legacy-\xff-token".to_vec()),
            );
            let loaded = load().expect("legacy non-UTF-8 keeps its historical fallback");
            assert_eq!(configured_http_token(&loaded.config.transport.token), None);
            assert!(
                loaded
                    .warnings
                    .iter()
                    .any(|warning| warning == "ignoring DONSETCH_HTTP_TOKEN: not valid UTF-8")
            );
        }
        drop(guard);
    }

    /// Markdown tables must never split a row: raw pipes in defaults
    /// and descriptions are escaped ("\\|"), so a data row has the
    /// same " | " separator count as the header (4 = 4 columns).
    #[test]
    fn show_markdown_keeps_one_row_per_knob() {
        let md = super::show_markdown();
        let header_cols = md.lines().next().unwrap().matches(" | ").count();
        let data_rows: Vec<&str> = md.lines().filter(|l| l.starts_with("| `")).collect();
        assert!(data_rows.len() > 50, "fieldbook shrunk?");
        for row in &data_rows {
            assert_eq!(
                row.matches(" | ").count(),
                header_cols,
                "row split by an unescaped pipe: {row}"
            );
        }
        // The transport.kind description does contain a pipe; the escape
        // must have fired on it.
        let kind_row = data_rows
            .iter()
            .find(|l| l.contains("`kind`"))
            .expect("transport.kind not in the table");
        assert!(kind_row.contains("\\|"), "kind row lost its escaped pipe");
    }

    #[test]
    fn show_text_redacts_transport_and_proxy_secrets() {
        let mut c = DonsetchConfig::default();
        c.transport.token = "transport-secret-sentinel".into();
        c.proxy.http = "http://plain.proxy.example:8080".into();
        c.proxy.https = "http://alice:pa@ss@gw.example:2334".into();
        c.proxy.all = "socks5://bob:hunter2@other.example:1080".into();
        c.proxy.pool = vec![
            "http://carol:p@ss@a.example:8080".into(),
            "bare-user:bare-secret@b.example:8080".into(),
        ];
        c.proxy.no_proxy = "localhost,.internal.test".into();
        let loaded = Loaded {
            config: c,
            merged: config::Map::new(),
            warnings: Vec::new(),
            file: None,
        };
        let text = super::show_text(&loaded);
        for leak in [
            "transport-secret-sentinel",
            "pa@ss",
            "hunter2",
            "p@ss",
            "bare-user",
            "bare-secret",
        ] {
            assert!(!text.contains(leak), "config show leaked {leak:?}");
        }
        for masked in [
            "token              = \"***\"",
            "http://plain.proxy.example:8080",
            "http://***@gw.example:2334",
            "socks5://***@other.example:1080",
            "http://***@a.example:8080",
            "localhost,.internal.test",
            "[\"http://***@a.example:8080\", \"***\"]",
        ] {
            assert!(text.contains(masked), "missing masked form {masked:?}");
        }
    }

    #[test]
    fn show_text_preserves_empty_secret_fields_as_unset() {
        let config = DonsetchConfig::default();
        for (section, key) in [
            ("transport", "token"),
            ("proxy", "http"),
            ("proxy", "https"),
            ("proxy", "all"),
        ] {
            let shown = value_of(&config, section, key);
            assert_eq!(masked_value(&config, section, key, shown), "\"\"");
        }
        let shown = value_of(&config, "proxy", "pool");
        assert_eq!(masked_value(&config, "proxy", "pool", shown), "[]");
    }

    #[test]
    fn opt_in_flags_accept_only_explicit_true_values() {
        for value in ["1", "true", "TRUE", "on", "ON", " true ", "\t1\n"] {
            assert!(
                env_flag_value(Some(std::ffi::OsStr::new(value))),
                "{value:?}"
            );
        }

        for value in [
            "",
            "0",
            "false",
            "FALSE",
            "off",
            "OFF",
            "yes",
            "no",
            "enabled",
            "anything",
            " true-ish ",
        ] {
            assert!(
                !env_flag_value(Some(std::ffi::OsStr::new(value))),
                "{value:?}"
            );
        }

        assert!(!env_flag_value(None));
    }

    #[cfg(unix)]
    #[test]
    fn opt_in_flags_reject_non_unicode_values() {
        use std::os::unix::ffi::OsStrExt;

        assert!(!env_flag_value(Some(std::ffi::OsStr::from_bytes(
            b"true\xff"
        ))));
    }

    #[test]
    fn boolish_covers_both_truth_and_falsity_vocabularies() {
        for on in ["1", "true", "TRUE", "On", "YES", "y"] {
            assert_eq!(boolish(on), Some(true), "{on:?}");
        }
        for off in ["0", "false", "OFF", "No", "n"] {
            assert_eq!(boolish(off), Some(false), "{off:?}");
        }
        assert_eq!(boolish("maybe"), None);
        assert_eq!(boolish(""), None);
    }

    #[test]
    fn every_fieldbook_entry_has_a_known_section() {
        for (section, key, _, _, _) in fieldbook() {
            match *section {
                "transport" | "mcp" | "paths" | "state" | "proxy" | "tls" | "persona" | "cli"
                | "fetch" | "bypass" | "search" | "browser" | "debug" => {}
                other => panic!("unknown section {other} for {section}.{key}"),
            }
        }
    }

    #[test]
    fn defaults_are_valid() {
        validate(&DonsetchConfig::default()).expect("defaults must pass validation");
    }

    #[test]
    fn put_builds_nested_tables_and_overwrites_leaves() {
        let mut m = VMap::new();
        put(&mut m, "fetch.h3", true.into(), "A");
        put(&mut m, "fetch.h3", false.into(), "B");
        put(&mut m, "fetch.pdf_max_mb", 5i64.into(), "C");
        let fetch = m.get("fetch").expect("fetch section in map");
        let fetch_str = fetch.to_string();
        assert!(fetch_str.contains("h3"), "{fetch_str}");
        assert!(fetch_str.contains("false"), "{fetch_str}");
        assert!(fetch_str.contains("pdf_max_mb"), "{fetch_str}");
        assert!(fetch_str.contains(" => 5"), "{fetch_str}");
    }

    #[cfg(unix)]
    #[test]
    fn write_private_creates_owner_only() {
        let dir =
            std::env::temp_dir().join(format!("donsetch-write-private-{}-new", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("secret.json");

        write_private(&p, b"{\"key\":\"s3cr3t\"}").unwrap();

        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(std::fs::read(&p).unwrap(), b"{\"key\":\"s3cr3t\"}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn write_private_tightens_an_existing_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "donsetch-write-private-{}-existing",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("secret.json");
        std::fs::write(&p, "old and longer content").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();

        write_private(&p, b"new").unwrap();

        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(std::fs::read(&p).unwrap(), b"new");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
