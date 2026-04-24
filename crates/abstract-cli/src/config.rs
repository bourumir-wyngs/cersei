//! Configuration with layered loading.
//!
//! Priority (lowest -> highest):
//! 1. Hardcoded defaults
//! 2. ~/.abstract/config.toml           (legacy global)
//! 3. .abstract/config.toml             (legacy project local)
//! 4. ~/.abstract/config_{project}.yaml (scoped project settings)
//! 5. Environment variables             (ABSTRACT_MODEL, etc.)
//! 6. CLI flags

use serde::de;
use serde::{Deserialize, Deserializer, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, RwLock};

static PERMISSIONS_PROJECT_NAME: LazyLock<RwLock<Option<String>>> =
    LazyLock::new(|| RwLock::new(None));

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
    #[serde(skip_serializing, skip_deserializing, default = "current_dir_fallback")]
    pub working_dir: PathBuf,
    #[serde(default)]
    pub fallback_models: Vec<String>,
    #[serde(default)]
    pub model_tools: Vec<String>,
    #[serde(default)]
    pub reviewer_tools: Vec<String>,
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
            working_dir: current_dir_fallback(),
            fallback_models: Vec::new(),
            model_tools: default_model_tools(),
            reviewer_tools: default_reviewer_tools(),
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

fn default_model_tools() -> Vec<String> {
    vec![
        // Files/code
        "Read".to_string(),
        "MultiRead".to_string(),
        "Write".to_string(),
        "Edit".to_string(),
        "File".to_string(),
        "Revert".to_string(),
        "Glob".to_string(),
        "Grep".to_string(),
        "MultiGrep".to_string(),
        "ListDirectory".to_string(),
        "FileHistory".to_string(),
        "Structure".to_string(),
        // Review/planning
        "Review".to_string(),
        "EnterPlanMode".to_string(),
        "ExitPlanMode".to_string(),
        "TodoWrite".to_string(),
        // Shell/process
        "Bash".to_string(),
        "PowerShell".to_string(),
        "Process".to_string(),
        // Build/test
        "Npm".to_string(),
        "Npx".to_string(),
        "Cargo".to_string(),
        "Pytest".to_string(),
        "Web_tests".to_string(),
        "Wasm_tests".to_string(),
        // Web/browser
        "WebFetch".to_string(),
        "WebSearch".to_string(),
        "Browser".to_string(),
        // Docs/data
        "SpreadSheet".to_string(),
        "PdfRead".to_string(),
        // Security/audit
        "Audit".to_string(),
        // Databases
        "MySql".to_string(),
        "PostgreSql".to_string(),
        // Git
        "Git".to_string(),
        // Config/user interaction
        "AskUserQuestion".to_string(),
        "Config".to_string(),
        // Docker
        "DockerCompose".to_string(),
        "DockerAssistant".to_string(),
        "DockerExec".to_string(),
        // Math/CAS
        "CAS".to_string(),
        // Memory
        "MemoryRecall".to_string(),
        "MemoryStore".to_string(),
    ]
}

fn default_reviewer_tools() -> Vec<String> {
    vec![
        // Files/code
        "Read".to_string(),
        "MultiRead".to_string(),
        "Glob".to_string(),
        "Grep".to_string(),
        "ListDirectory".to_string(),
        "FileHistory".to_string(),
        "Structure".to_string(),
        // Git
        "Git".to_string(),
        // Memory
        "MemoryRecall".to_string(),
    ]
}

fn current_dir_fallback() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// ~/.abstract/
pub fn global_config_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".abstract")
}

fn project_config_dir_in(start_dir: &Path) -> PathBuf {
    start_dir.join(".abstract")
}

/// .abstract/ in the current project
pub fn project_config_dir() -> PathBuf {
    project_config_dir_in(&current_dir_fallback())
}

/// ~/.abstract/config.toml
pub fn global_config_path() -> PathBuf {
    global_config_dir().join("config.toml")
}

/// .abstract/config.toml in the given project
pub fn legacy_project_config_path(start_dir: &Path) -> PathBuf {
    project_config_dir_in(start_dir).join("config.toml")
}

/// ~/.abstract/config_{project}.yaml
pub fn project_config_path() -> PathBuf {
    global_config_dir().join(format!("config_{}.yaml", permissions_project_name()))
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

    derive_permissions_project_name(&current_dir_fallback(), None)
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

/// Load config for a specific project root.
pub fn load_for_dir(start_dir: &Path) -> AppConfig {
    let global_path = global_config_path();
    let legacy_project_path = legacy_project_config_path(start_dir);
    let scoped_project_path = project_config_path();

    load_from_paths(
        &[
            global_path.as_path(),
            legacy_project_path.as_path(),
            scoped_project_path.as_path(),
        ],
        true,
    )
}

pub fn ensure_project_config_exists(start_dir: &Path) -> anyhow::Result<()> {
    let path = project_config_path();
    if path.exists() {
        return Ok(());
    }

    let global_path = global_config_path();
    let legacy_project_path = legacy_project_config_path(start_dir);
    let config = load_from_paths(
        &[
            global_path.as_path(),
            legacy_project_path.as_path(),
            path.as_path(),
        ],
        false,
    );

    save_to(&config, &path)
}

fn load_from_paths(paths: &[&Path], apply_env_overrides: bool) -> AppConfig {
    let mut config = AppConfig::default();

    for path in paths {
        if let Some(loaded) = load_config_file(path) {
            merge(&mut config, loaded);
        }
    }

    if apply_env_overrides {
        apply_env(&mut config);
    }
    apply_derived_values(&mut config);
    config
}

fn load_config_file(path: &Path) -> Option<AppConfig> {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("toml") => load_toml_file(path),
        Some("yaml") | Some("yml") => load_yaml_file(path),
        _ => None,
    }
}

fn load_toml_file(path: &Path) -> Option<AppConfig> {
    let content = std::fs::read_to_string(path).ok()?;
    toml::from_str(&content).ok()
}

fn load_yaml_file(path: &Path) -> Option<AppConfig> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_saphyr::from_str(&content).ok()
}

fn merge(base: &mut AppConfig, overlay: AppConfig) {
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
    if overlay.model_tools != AppConfig::default().model_tools {
        base.model_tools = overlay.model_tools;
    }
    if overlay.reviewer_tools != AppConfig::default().reviewer_tools {
        base.reviewer_tools = overlay.reviewer_tools;
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

fn storage_value(config: &AppConfig) -> anyhow::Result<serde_json::Value> {
    let mut value = serde_json::to_value(config)?;
    if let Some(map) = value.as_object_mut() {
        map.remove("working_dir");
    }
    Ok(value)
}

/// Save config to a TOML or YAML file based on its extension.
pub fn save_to(config: &AppConfig, path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut config = config.clone();
    apply_derived_values(&mut config);
    let value = storage_value(&config)?;
    let content = match path.extension().and_then(|ext| ext.to_str()) {
        Some("toml") => toml::to_string_pretty(&value)?,
        Some("yaml") | Some("yml") => serde_saphyr::to_string(&value)?,
        Some(other) => anyhow::bail!("Unsupported config format: {other}"),
        None => anyhow::bail!("Config path must include a file extension"),
    };
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

    #[test]
    fn load_from_paths_merges_legacy_and_scoped_configs_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let global = tmp.path().join("global.toml");
        let project = tmp.path().join("project.toml");
        let scoped = tmp.path().join("scoped.yaml");

        std::fs::write(
            &global,
            r#"
model = "global-model"
theme = "light"
max_turns = 11
"#,
        )
        .unwrap();
        std::fs::write(
            &project,
            r#"
model = "project-model"
provider = "anthropic"
"#,
        )
        .unwrap();
        std::fs::write(
            &scoped,
            r#"
model: scoped-model
fallback_models:
  - google/gemini-3-flash-preview
"#,
        )
        .unwrap();

        let config = load_from_paths(
            &[global.as_path(), project.as_path(), scoped.as_path()],
            false,
        );

        assert_eq!(config.model, "scoped-model");
        assert_eq!(config.theme, "light");
        assert_eq!(config.provider, "anthropic");
        assert_eq!(
            config.fallback_models,
            vec!["google/gemini-3-flash-preview".to_string()]
        );
        assert_eq!(config.max_turns, 11);
    }

    #[test]
    fn load_from_paths_ignores_persisted_working_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");

        std::fs::write(
            &path,
            r#"
model = "override"
working_dir = "/definitely/not/the/runtime/workdir"
"#,
        )
        .unwrap();

        let config = load_from_paths(&[path.as_path()], false);
        assert_eq!(config.model, "override");
        assert_ne!(
            config.working_dir,
            PathBuf::from("/definitely/not/the/runtime/workdir")
        );
    }

    #[test]
    fn save_to_yaml_omits_working_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.yaml");
        let mut config = AppConfig::default();
        config.working_dir = PathBuf::from("/tmp/private-workdir");

        save_to(&config, &path).unwrap();

        let content = std::fs::read_to_string(path).unwrap();
        assert!(!content.contains("working_dir"));
        assert!(content.contains("model: gpt-5.4"));
        assert!(content.contains("model_tools:"));
        assert!(content.contains("reviewer_tools:"));
        assert!(content.contains("MemoryRecall"));
        assert!(content.contains("MemoryStore"));
    }

    #[test]
    fn save_to_toml_omits_working_dir_and_keeps_toml_format() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        let mut config = AppConfig::default();
        config.working_dir = PathBuf::from("/tmp/private-workdir");

        save_to(&config, &path).unwrap();

        let content = std::fs::read_to_string(path).unwrap();
        assert!(!content.contains("working_dir"));
        assert!(content.contains("model = \"gpt-5.4\""));
        assert!(content.contains("effort = 4096"));
        assert!(content.contains("model_tools = ["));
        assert!(content.contains("reviewer_tools = ["));
    }

    #[test]
    fn ensure_project_config_exists_creates_scoped_yaml_from_merged_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(project.join(".abstract")).unwrap();

        let global_dir = home.join(".abstract");
        std::fs::create_dir_all(&global_dir).unwrap();
        std::fs::write(
            global_dir.join("config.toml"),
            "model = \"global-model\"\npermissions_mode = \"accept-edits\"\n",
        )
        .unwrap();
        std::fs::write(
            project.join(".abstract").join("config.toml"),
            "theme = \"light\"\n",
        )
        .unwrap();

        let previous_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &home);
        initialize_permissions_project_name(&project, None);

        ensure_project_config_exists(&project).unwrap();

        let scoped = global_config_dir().join("config_project.yaml");
        assert!(scoped.exists());
        let content = std::fs::read_to_string(scoped).unwrap();
        assert!(content.contains("model: global-model"));
        assert!(content.contains("theme: light"));
        assert!(content.contains("permissions_mode: accept-edits"));
        assert!(content.contains("model_tools:"));
        assert!(content.contains("reviewer_tools:"));
        assert!(!content.contains("working_dir"));

        match previous_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
    }
}
