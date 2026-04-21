//! TOML configuration with layered loading.
//!
//! Priority (lowest → highest):
//! 1. Hardcoded defaults
//! 2. ~/.abstract/config.toml  (user global)
//! 3. .abstract/config.toml    (project local)
//! 4. Environment variables     (ABSTRACT_MODEL, etc.)
//! 5. CLI flags

use serde::de;
use serde::{Deserialize, Deserializer, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, RwLock};

static PERMISSIONS_PROJECT_NAME: LazyLock<RwLock<Option<String>>> =
    LazyLock::new(|| RwLock::new(None));

// ─── Config structs ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub model: String,
    pub reviewer_model: String,
    pub provider: String,
    pub max_turns: u32,
    pub max_tokens: u32,
    #[serde(
        default = "default_effort_budget",
        deserialize_with = "deserialize_effort"
    )]
    pub effort: u32,
    pub output_style: String,
    pub theme: String,
    pub auto_compact: bool,
    pub graph_memory: bool,
    pub permissions_mode: String,
    pub working_dir: PathBuf,
    #[serde(default)]
    pub fallback_models: Vec<String>,
    #[serde(default)]
    pub mcp_servers: Vec<McpServerEntry>,
    #[serde(default)]
    pub hooks: Vec<HookEntry>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            model: "gpt-5.4".into(),
            reviewer_model: "google/gemini-3.1-pro-preview".into(),
            provider: "openai".into(),
            max_turns: 50,
            max_tokens: max_tokens_for_effort(default_effort_budget()),
            effort: default_effort_budget(),
            output_style: "default".into(),
            theme: "dark".into(),
            auto_compact: true,
            graph_memory: true,
            permissions_mode: "interactive".into(),
            working_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            fallback_models: Vec::new(),
            mcp_servers: Vec::new(),
            hooks: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerEntry {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookEntry {
    pub event: String,
    pub command: String,
}

// ─── Config directories ────────────────────────────────────────────────────

/// ~/.abstract/
pub fn global_config_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".abstract")
}

/// .abstract/ in the current project
pub fn project_config_dir() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".abstract")
}

/// ~/.abstract/config.toml
pub fn global_config_path() -> PathBuf {
    global_config_dir().join("config.toml")
}

/// .abstract/config.toml
pub fn project_config_path() -> PathBuf {
    project_config_dir().join("config.toml")
}

/// ~/.abstract/history
pub fn history_path() -> PathBuf {
    global_config_dir().join("history")
}

/// ~/.abstract/{project}_graph.db
pub fn graph_db_path() -> PathBuf {
    let project_name = permissions_project_name();
    global_config_dir().join(format!("{project_name}_graph.db"))
}

pub fn initialize_permissions_project_name(start_dir: &Path, explicit_name: Option<&str>) {
    let project_name = derive_permissions_project_name(start_dir, explicit_name);
    let mut guard = PERMISSIONS_PROJECT_NAME.write().unwrap();
    *guard = Some(project_name);
}

fn permissions_project_name() -> String {
    if let Some(project_name) = PERMISSIONS_PROJECT_NAME.read().unwrap().clone() {
        return project_name;
    }

    let start_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    derive_permissions_project_name(&start_dir, None)
}

fn derive_permissions_project_name(start_dir: &Path, explicit_name: Option<&str>) -> String {
    if let Some(name) = explicit_name.map(str::trim).filter(|name| !name.is_empty()) {
        return sanitize_permissions_project_name(name);
    }

    if let Some(name) = start_dir.file_name().and_then(|name| name.to_str()) {
        let sanitized = sanitize_permissions_project_name(name);
        if !sanitized.is_empty() {
            return sanitized;
        }
    }

    "project".into()
}

fn sanitize_permissions_project_name(name: &str) -> String {
    let sanitized = cersei_memory::memdir::sanitize_path_component(name);
    if sanitized.is_empty() {
        "project".into()
    } else {
        sanitized
    }
}

pub const LOW_EFFORT_BUDGET: u32 = 1024;
pub const MEDIUM_EFFORT_BUDGET: u32 = 4096;
pub const HIGH_EFFORT_BUDGET: u32 = 8192;
pub const MAX_EFFORT_BUDGET: u32 = 32768;

fn default_effort_budget() -> u32 {
    MEDIUM_EFFORT_BUDGET
}

pub fn max_tokens_for_effort(effort: u32) -> u32 {
    effort.saturating_mul(4)
}

pub fn set_effort_budget(config: &mut AppConfig, effort: u32) {
    config.effort = effort;
    config.max_tokens = max_tokens_for_effort(effort);
}

fn apply_derived_values(config: &mut AppConfig) {
    config.max_tokens = max_tokens_for_effort(config.effort);
}

pub fn parse_effort_budget(value: &str) -> Option<u32> {
    match value.trim().to_lowercase().as_str() {
        "low" | "min" => Some(LOW_EFFORT_BUDGET),
        "medium" | "med" | "default" => Some(MEDIUM_EFFORT_BUDGET),
        "high" => Some(HIGH_EFFORT_BUDGET),
        "max" | "maximum" => Some(MAX_EFFORT_BUDGET),
        other => other
            .parse::<u32>()
            .ok()
            .filter(|budget| *budget > 0 && budget.checked_mul(4).is_some()),
    }
}

pub fn effort_temperature(budget: u32) -> Option<f32> {
    match budget {
        LOW_EFFORT_BUDGET => Some(0.3),
        MAX_EFFORT_BUDGET => Some(1.0),
        _ => None,
    }
}

fn deserialize_effort<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum EffortValue {
        Number(u32),
        String(String),
    }

    match EffortValue::deserialize(deserializer)? {
        EffortValue::Number(n) if n > 0 => Ok(n),
        EffortValue::Number(_) => Err(de::Error::custom("effort must be greater than zero")),
        EffortValue::String(value) => parse_effort_budget(&value)
            .ok_or_else(|| de::Error::custom(format!("invalid effort value '{value}'"))),
    }
}

/// ~/.abstract/permissions_{project}.yaml
pub fn permissions_path() -> PathBuf {
    let project_name = permissions_project_name();
    global_config_dir().join(format!("permissions_{project_name}.yaml"))
}

// ─── Loading ───────────────────────────────────────────────────────────────

/// Load config with layered merging.
pub fn load() -> AppConfig {
    let mut config = AppConfig::default();

    // Layer 2: global config
    if let Some(loaded) = load_toml_file(&global_config_path()) {
        merge(&mut config, loaded);
    }

    // Layer 3: project config
    if let Some(loaded) = load_toml_file(&project_config_path()) {
        merge(&mut config, loaded);
    }

    // Layer 4: environment variables
    apply_env(&mut config);
    apply_derived_values(&mut config);

    config
}

fn load_toml_file(path: &Path) -> Option<AppConfig> {
    let content = std::fs::read_to_string(path).ok()?;
    toml::from_str(&content).ok()
}

fn merge(base: &mut AppConfig, overlay: AppConfig) {
    // Only override non-default values
    if overlay.model != AppConfig::default().model {
        base.model = overlay.model;
    }
    if overlay.reviewer_model != AppConfig::default().reviewer_model {
        base.reviewer_model = overlay.reviewer_model;
    }
    if overlay.provider != AppConfig::default().provider {
        base.provider = overlay.provider;
    }
    if overlay.max_turns != AppConfig::default().max_turns {
        base.max_turns = overlay.max_turns;
    }
    if overlay.max_tokens != AppConfig::default().max_tokens {
        base.max_tokens = overlay.max_tokens;
    }
    if overlay.effort != AppConfig::default().effort {
        base.effort = overlay.effort;
    }
    if overlay.output_style != AppConfig::default().output_style {
        base.output_style = overlay.output_style;
    }
    if overlay.theme != AppConfig::default().theme {
        base.theme = overlay.theme;
    }
    if !overlay.auto_compact && AppConfig::default().auto_compact {
        base.auto_compact = false;
    }
    if !overlay.graph_memory && AppConfig::default().graph_memory {
        base.graph_memory = false;
    }
    if overlay.permissions_mode != AppConfig::default().permissions_mode {
        base.permissions_mode = overlay.permissions_mode;
    }
    if !overlay.fallback_models.is_empty() {
        base.fallback_models = overlay.fallback_models;
    }
    if !overlay.mcp_servers.is_empty() {
        base.mcp_servers = overlay.mcp_servers;
    }
    if !overlay.hooks.is_empty() {
        base.hooks = overlay.hooks;
    }
}

fn apply_env(config: &mut AppConfig) {
    if let Ok(v) = std::env::var("ABSTRACT_MODEL") {
        config.model = v;
    }
    if let Ok(v) = std::env::var("ABSTRACT_REVIEWER_MODEL") {
        config.reviewer_model = v;
    }
    if let Ok(v) = std::env::var("ABSTRACT_PROVIDER") {
        config.provider = v;
    }
    if let Ok(v) = std::env::var("ABSTRACT_EFFORT") {
        if let Some(effort) = parse_effort_budget(&v) {
            set_effort_budget(config, effort);
        }
    }
    if let Ok(v) = std::env::var("ABSTRACT_THEME") {
        config.theme = v;
    }
    if let Ok(v) = std::env::var("ABSTRACT_FALLBACK_MODELS") {
        config.fallback_models = v
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }
    if let Ok(v) = std::env::var("ABSTRACT_MAX_TURNS") {
        if let Ok(n) = v.parse() {
            config.max_turns = n;
        }
    }
}

/// Save config to a TOML file.
pub fn save_to(config: &AppConfig, path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut config = config.clone();
    apply_derived_values(&mut config);
    let content = toml::to_string_pretty(&config)?;
    std::fs::write(path, content)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_permissions_project_name_uses_start_dir_folder_name() {
        let path = PathBuf::from("/tmp/my-project");
        assert_eq!(derive_permissions_project_name(&path, None), "my-project");
    }

    #[test]
    fn derive_permissions_project_name_uses_explicit_override() {
        let path = PathBuf::from("/tmp/my-project");
        assert_eq!(
            derive_permissions_project_name(&path, Some("custom/name")),
            "custom_name"
        );
    }

    #[test]
    fn permissions_path_uses_project_file_name() {
        initialize_permissions_project_name(Path::new("/tmp/cersei"), None);
        assert_eq!(
            permissions_path(),
            global_config_dir().join("permissions_cersei.yaml")
        );
    }

    #[test]
    fn graph_db_path_uses_project_file_name() {
        initialize_permissions_project_name(Path::new("/tmp/cersei"), None);
        assert_eq!(graph_db_path(), global_config_dir().join("cersei_graph.db"));
    }

    #[test]
    fn deserializes_numeric_effort() {
        let config: AppConfig = toml::from_str("effort = 12345").unwrap();
        assert_eq!(config.effort, 12345);
    }

    #[test]
    fn deserializes_legacy_string_effort() {
        let config: AppConfig = toml::from_str("effort = \"high\"").unwrap();
        assert_eq!(config.effort, HIGH_EFFORT_BUDGET);
    }

    #[test]
    fn serializes_effort_as_number() {
        let config = AppConfig::default();
        let toml = toml::to_string(&config).unwrap();
        assert!(toml.contains("effort = 4096"));
    }

    #[test]
    fn derives_max_tokens_from_effort() {
        let mut config: AppConfig = toml::from_str("effort = 8192\nmax_tokens = 123").unwrap();
        apply_derived_values(&mut config);
        assert_eq!(config.max_tokens, 32768);
    }

    #[test]
    fn set_effort_budget_updates_max_tokens() {
        let mut config = AppConfig::default();
        set_effort_budget(&mut config, 12000);
        assert_eq!(config.effort, 12000);
        assert_eq!(config.max_tokens, 48000);
    }
}
