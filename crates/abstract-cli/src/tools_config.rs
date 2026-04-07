use cersei_tools::{Extensions, ToolsConfig};

pub const TOOLS_CONFIG_FILE: &str = "tools.yaml";

pub fn load_extensions_from_start_dir() -> anyhow::Result<Extensions> {
    let start_dir = std::env::current_dir()
        .map_err(|e| anyhow::anyhow!("Failed to determine start directory: {e}"))?;
    let path = start_dir.join(TOOLS_CONFIG_FILE);

    let extensions = Extensions::default();
    if !path.exists() {
        return Ok(extensions);
    }

    let content = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("Failed to read {}: {e}", path.display()))?;
    let config: ToolsConfig = serde_saphyr::from_str(&content)
        .map_err(|e| anyhow::anyhow!("Failed to parse {}: {e}", path.display()))?;
    cersei_tools::set_global_tools_config(config.clone());
    extensions.insert(config);
    Ok(extensions)
}
