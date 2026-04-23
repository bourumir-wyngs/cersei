//! Audit tool: scan packages for known security vulnerabilities using OSV.dev.
//!
//! Accepts either explicit package lists or a `path` for automatic lockfile discovery.
//! Supports npm (web), crates.io (rust), and PyPI (python) ecosystems, plus `auto`
//! to detect all ecosystems in a directory.

use super::*;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use walkdir::WalkDir;

pub struct AuditTool;

// ── Package representation ────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Pkg {
    name: String,
    /// Exact resolved version. None when only a range is available.
    version: Option<String>,
    /// Human-readable constraint for approximate mode (e.g. ">=0.129.0").
    constraint: Option<String>,
    ecosystem: &'static str,
    /// Relative path to the lockfile this package came from.
    source: String,
}

impl Pkg {
    fn exact(
        name: impl Into<String>,
        version: impl Into<String>,
        ecosystem: &'static str,
        source: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            version: Some(version.into()),
            constraint: None,
            ecosystem,
            source: source.into(),
        }
    }

    fn approximate(
        name: impl Into<String>,
        constraint: impl Into<String>,
        ecosystem: &'static str,
        source: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            version: None,
            constraint: Some(constraint.into()),
            ecosystem,
            source: source.into(),
        }
    }
}

// ── TOML array-of-tables parser ───────────────────────────────────────────────
// Used for: Cargo.lock, poetry.lock, uv.lock.
// Reads `[[package]]` entries and collects their key = "value" fields.

fn parse_toml_package_list(content: &str) -> Vec<HashMap<String, String>> {
    let mut result: Vec<HashMap<String, String>> = Vec::new();
    let mut current: Option<HashMap<String, String>> = None;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed == "[[package]]" {
            if let Some(pkg) = current.take() {
                result.push(pkg);
            }
            current = Some(HashMap::new());
        } else if let Some(ref mut pkg) = current {
            if let Some((key, val)) = trimmed.split_once(" = ") {
                let val = val.trim().trim_matches('"').to_string();
                pkg.insert(key.trim().to_string(), val);
            }
        }
    }
    if let Some(pkg) = current {
        result.push(pkg);
    }
    result
}

// ── Lockfile parsers ──────────────────────────────────────────────────────────

fn parse_package_lock_json(content: &str, source: &str, include_dev: bool) -> Vec<Pkg> {
    let Ok(json) = serde_json::from_str::<Value>(content) else {
        return vec![];
    };
    let mut pkgs = Vec::new();

    // v2/v3: top-level "packages" object with "node_modules/name" keys
    if let Some(packages) = json["packages"].as_object() {
        for (key, val) in packages {
            if key.is_empty() {
                continue; // root manifest
            }
            // v2/v3: skip workspace/local entries that are not under node_modules/
            // (e.g. "packages/app" from npm workspaces — not a registry package name)
            if !key.contains("node_modules/") {
                continue;
            }
            let is_dev = val["dev"].as_bool().unwrap_or(false);
            if !include_dev && is_dev {
                continue;
            }
            // Strip leading "node_modules/" and any nested "node_modules/" prefix
            let name = key
                .split("node_modules/")
                .filter(|s| !s.is_empty())
                .last()
                .unwrap_or(key.as_str());
            if let Some(version) = val["version"].as_str() {
                pkgs.push(Pkg::exact(name, version, "npm", source));
            }
        }
        return pkgs;
    }

    // v1: "dependencies" object (may nest)
    if let Some(deps) = json["dependencies"].as_object() {
        fn collect_v1(
            deps: &serde_json::Map<String, Value>,
            pkgs: &mut Vec<Pkg>,
            source: &str,
            include_dev: bool,
        ) {
            for (name, val) in deps {
                if !include_dev && val["dev"].as_bool().unwrap_or(false) {
                    continue;
                }
                if let Some(version) = val["version"].as_str() {
                    pkgs.push(Pkg::exact(name.clone(), version, "npm", source));
                }
                if let Some(nested) = val["dependencies"].as_object() {
                    collect_v1(nested, pkgs, source, include_dev);
                }
            }
        }
        collect_v1(deps, &mut pkgs, source, include_dev);
    }
    pkgs
}

fn parse_yarn_lock(content: &str, source: &str) -> Vec<Pkg> {
    // Supports yarn v1 classic lockfile format.
    let mut pkgs = Vec::new();
    let mut pending_names: Vec<String> = Vec::new();
    let mut pending_version: Option<String> = None;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // Non-indented line ending with ':' → package header
        if !line.starts_with(' ') && !line.starts_with('\t') && line.ends_with(':') {
            // Flush previous entry
            if let Some(ver) = pending_version.take() {
                for n in &pending_names {
                    pkgs.push(Pkg::exact(n.clone(), &ver, "npm", source));
                }
            }
            pending_names.clear();

            // Header may look like: `lodash@^4.17.11, lodash@^4.17.21:` or
            // `"@scope/pkg@^1.0.0":` — strip outer quotes and trailing colon.
            let header = line.trim_end_matches(':').trim_matches('"');
            for spec in header.split(',') {
                let spec = spec.trim().trim_matches('"');
                let name = extract_yarn_name(spec);
                if !name.is_empty() && !pending_names.contains(&name) {
                    pending_names.push(name);
                }
            }
        } else if let Some(rest) = trimmed.strip_prefix("version") {
            // v1 classic: `version "4.17.21"` (quoted, space-separated)
            // v2 berry:   `version: 4.17.21`  (YAML, colon-separated, unquoted)
            let ver = rest
                .trim()
                .trim_start_matches(':')
                .trim()
                .trim_matches('"')
                .to_string();
            if !ver.is_empty() {
                pending_version = Some(ver);
            }
        }
    }
    if let Some(ver) = pending_version {
        for n in &pending_names {
            pkgs.push(Pkg::exact(n.clone(), &ver, "npm", source));
        }
    }

    dedup_pkgs(pkgs)
}

/// Extract package name from a yarn specifier like `lodash@^4.17.11` or `@scope/pkg@^1.0.0`.
fn extract_yarn_name(spec: &str) -> String {
    if spec.starts_with('@') {
        // Scoped package: @scope/name@range → @scope/name
        let without_at = &spec[1..];
        match without_at.rfind('@') {
            Some(pos) => format!("@{}", &without_at[..pos]),
            None => spec.to_string(),
        }
    } else {
        match spec.rfind('@') {
            Some(pos) => spec[..pos].to_string(),
            None => String::new(), // no `@` → not a valid package specifier
        }
    }
}

fn parse_pnpm_lock_yaml(content: &str, source: &str) -> Vec<Pkg> {
    // Parse pnpm-lock.yaml by scanning for `packages:` / `snapshots:` sections.
    // v6 keys: `  /lodash@4.17.21:` (with leading slash)
    // v9 keys: `  lodash@4.17.21:`
    let mut pkgs = Vec::new();
    let mut in_section = false;

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        // Top-level section headers (not indented)
        if !line.starts_with(' ') && !line.starts_with('\t') {
            in_section = matches!(line.trim_end_matches(':').trim(), "packages" | "snapshots");
            continue;
        }
        if !in_section {
            continue;
        }

        let trimmed = line.trim();
        if !trimmed.ends_with(':') || !trimmed.contains('@') || trimmed.starts_with('#') {
            continue;
        }

        // Strip trailing colon, leading slash (v6), and outer quotes.
        // pnpm uses single quotes (`'@babel/core@7.0.0':`) or double quotes
        // (`"@scope/pkg@1.0.0":`) around scoped-package keys.
        let spec = trimmed
            .trim_end_matches(':')
            .trim_matches(|c| c == '\'' || c == '"')
            .trim_start_matches('/');

        let (name, version) = if spec.starts_with('@') {
            let rest = &spec[1..];
            match rest.rfind('@') {
                Some(pos) => (format!("@{}", &rest[..pos]), &rest[pos + 1..]),
                None => continue,
            }
        } else {
            match spec.rfind('@') {
                Some(pos) => (spec[..pos].to_string(), &spec[pos + 1..]),
                None => continue,
            }
        };

        // Strip pnpm suffix like `_hash` or `(peer...)` after the semver
        let version = version.split('_').next().unwrap_or(version);
        let version = version.split('(').next().unwrap_or(version).trim();
        if !version.is_empty() && !name.is_empty() {
            pkgs.push(Pkg::exact(name, version, "npm", source));
        }
    }
    dedup_pkgs(pkgs)
}

fn parse_cargo_lock(content: &str, source: &str) -> Vec<Pkg> {
    parse_toml_package_list(content)
        .into_iter()
        .filter_map(|mut pkg| {
            let name = pkg.remove("name")?;
            let version = pkg.remove("version")?;
            Some(Pkg::exact(name, version, "crates.io", source))
        })
        .collect()
}

/// Return true if a poetry.lock package entry is dev-only.
/// Handles both the legacy `category = "dev"` field (Poetry <1.2) and
/// the modern `groups = ["dev", ...]` array (Poetry >=1.2).
fn is_poetry_dev_package(pkg: &HashMap<String, String>) -> bool {
    // Legacy: category = "dev"
    if pkg
        .get("category")
        .map(|c| c.as_str() == "dev")
        .unwrap_or(false)
    {
        return true;
    }
    // Modern: groups array — stored as raw TOML array string e.g. `["dev"]`.
    // A package can appear in both main and dev groups; keep it if it is a runtime dep.
    if let Some(groups) = pkg.get("groups") {
        let groups = extract_toml_array_strings(groups);
        let has_runtime_group = groups
            .iter()
            .any(|g| matches!(g.as_str(), "main" | "default"));
        let has_dev_group = groups
            .iter()
            .any(|g| matches!(g.as_str(), "dev" | "test" | "tests"));
        if has_dev_group && !has_runtime_group {
            return true;
        }
    }
    false
}

fn parse_poetry_lock(content: &str, source: &str, include_dev: bool) -> Vec<Pkg> {
    parse_toml_package_list(content)
        .into_iter()
        .filter_map(|mut pkg| {
            let name = pkg.remove("name")?;
            let version = pkg.remove("version")?;
            if !include_dev && is_poetry_dev_package(&pkg) {
                return None;
            }
            Some(Pkg::exact(name.to_lowercase(), version, "PyPI", source))
        })
        .collect()
}

fn parse_uv_lock(content: &str, source: &str) -> Vec<Pkg> {
    // uv.lock uses [[package]] sections like poetry but may include local workspace members.
    // Local packages have `source = {editable = "..."}` inline table on the same line — skip them.
    let mut pkgs = Vec::new();
    let mut in_pkg = false;
    let mut current: HashMap<String, String> = HashMap::new();
    let mut skip_current = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed == "[[package]]" {
            if in_pkg && !skip_current {
                if let (Some(n), Some(v)) = (current.get("name"), current.get("version")) {
                    pkgs.push(Pkg::exact(n.to_lowercase(), v.as_str(), "PyPI", source));
                }
            }
            in_pkg = true;
            skip_current = false;
            current.clear();
        } else if in_pkg {
            if trimmed.starts_with("source = {") && !trimmed.contains("registry") {
                // Skip local/editable/path packages. Registry sources are remote and kept.
                skip_current = true;
            } else if let Some((key, val)) = trimmed.split_once(" = ") {
                let val = val.trim().trim_matches('"').to_string();
                current.insert(key.trim().to_string(), val);
            }
        }
    }
    if in_pkg && !skip_current {
        if let (Some(n), Some(v)) = (current.get("name"), current.get("version")) {
            pkgs.push(Pkg::exact(n.to_lowercase(), v.as_str(), "PyPI", source));
        }
    }
    pkgs
}

fn parse_pipfile_lock(content: &str, source: &str, include_dev: bool) -> Vec<Pkg> {
    let Ok(json) = serde_json::from_str::<Value>(content) else {
        return vec![];
    };
    let mut pkgs = Vec::new();

    let sections = if include_dev {
        vec!["default", "develop"]
    } else {
        vec!["default"]
    };

    for section in sections {
        if let Some(deps) = json[section].as_object() {
            for (name, val) in deps {
                if let Some(ver_str) = val["version"].as_str() {
                    // Pipfile.lock stores versions as "==1.2.3"
                    let ver = ver_str.trim_start_matches("==");
                    if !ver.is_empty() {
                        pkgs.push(Pkg::exact(name.to_lowercase(), ver, "PyPI", source));
                    }
                }
            }
        }
    }
    pkgs
}

/// Parse `requirements.txt` (and `-dev.txt` variants).
/// Returns exact packages for `==` pins and approximate for other specifiers.
fn parse_requirements_txt(content: &str, source: &str) -> Vec<Pkg> {
    let mut pkgs = Vec::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('-') {
            continue;
        }
        // Strip inline comments
        let line = line.split('#').next().unwrap_or(line).trim();
        // Strip PEP 508 environment markers (e.g. "; python_version < '3.10'")
        let line = line.split(';').next().unwrap_or(line).trim();

        // Split into package name and version specifier, handling extras,
        // markers, and direct references.
        let (name_raw, specifier) = split_dep_spec(line);

        let pkg_name = name_raw.trim().to_lowercase();
        if pkg_name.is_empty() {
            continue;
        }

        if specifier.is_empty() {
            pkgs.push(Pkg::approximate(pkg_name, "(any)", "PyPI", source));
        } else if specifier.starts_with("==") {
            let ver = clean_exact_version(&specifier[2..]);
            if !ver.is_empty() {
                pkgs.push(Pkg::exact(pkg_name, &ver, "PyPI", source));
            } else {
                pkgs.push(Pkg::approximate(pkg_name, specifier, "PyPI", source));
            }
        } else {
            pkgs.push(Pkg::approximate(pkg_name, specifier, "PyPI", source));
        }
    }
    pkgs
}

/// Parse `pyproject.toml` for declared dependencies (approximate mode).
/// Only used when no exact lockfile is present in the same directory.
/// Handles [project] dependencies array and [tool.poetry.*] key=value tables.
fn parse_pyproject_toml(content: &str, source: &str, include_dev: bool) -> Vec<Pkg> {
    let mut pkgs = Vec::new();

    #[derive(PartialEq, Clone, Copy)]
    enum Section {
        None,
        ProjectMeta,         // inside [project], watching for `dependencies = [...]`
        ProjectDepsArray,    // inside a multi-line `dependencies = [...]`
        ProjectOptDeps,      // inside [project.optional-dependencies]
        ProjectOptDepsArray, // inside a multi-line `key = [...]` under [project.optional-dependencies]
        PoetryDeps,          // [tool.poetry.dependencies] or dev/group variants
    }

    let mut section = Section::None;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // TOML section header (single brackets, not [[...]])
        if trimmed.starts_with('[') && !trimmed.starts_with("[[") {
            let header = &trimmed[1..trimmed.len().saturating_sub(1)];
            section = match header {
                "project" => Section::ProjectMeta,
                "project.optional-dependencies" => Section::ProjectOptDeps,
                "tool.poetry.dependencies" => Section::PoetryDeps,
                "tool.poetry.dev-dependencies" => {
                    if include_dev {
                        Section::PoetryDeps
                    } else {
                        Section::None
                    }
                }
                h if h.starts_with("tool.poetry.group.") && h.ends_with(".dependencies") => {
                    if include_dev {
                        Section::PoetryDeps
                    } else {
                        Section::None
                    }
                }
                _ => Section::None,
            };
            continue;
        }

        match section {
            Section::None => {}

            Section::ProjectMeta => {
                // Watch specifically for the `dependencies = [...]` key.
                // Ignore all other keys (name, version, requires-python, description, etc.)
                if !trimmed.starts_with("dependencies") {
                    continue;
                }
                if let Some(bracket_pos) = trimmed.find('[') {
                    let after = &trimmed[bracket_pos + 1..];
                    let close_pos = find_unquoted_close_bracket(after);
                    let items_str = close_pos.map(|p| &after[..p]).unwrap_or(after);
                    for item in extract_toml_array_strings(items_str) {
                        add_pep508_dep(&item, source, &mut pkgs);
                    }
                    if close_pos.is_none() {
                        section = Section::ProjectDepsArray;
                    }
                    // If closed on same line, remain ProjectMeta (there may be more keys)
                }
            }

            Section::ProjectDepsArray => {
                if trimmed.starts_with(']') {
                    section = Section::ProjectMeta;
                    continue;
                }
                parse_pep508_array_line(trimmed, source, &mut pkgs);
            }

            Section::ProjectOptDeps => {
                if let Some(bracket_pos) = trimmed.find('[') {
                    let after = &trimmed[bracket_pos + 1..];
                    let close_pos = find_unquoted_close_bracket(after);
                    let items_str = close_pos.map(|p| &after[..p]).unwrap_or(after);
                    for item in extract_toml_array_strings(items_str) {
                        add_pep508_dep(&item, source, &mut pkgs);
                    }
                    if close_pos.is_none() {
                        section = Section::ProjectOptDepsArray;
                    }
                }
            }

            Section::ProjectOptDepsArray => {
                if trimmed.starts_with(']') {
                    section = Section::ProjectOptDeps;
                    continue;
                }
                parse_pep508_array_line(trimmed, source, &mut pkgs);
            }

            Section::PoetryDeps => {
                if trimmed.starts_with('#') {
                    continue;
                }
                if let Some((key, val)) = trimmed.split_once(" = ") {
                    let key = key.trim().to_lowercase();
                    if key == "python" {
                        continue;
                    }
                    let val = val.trim();
                    let constraint = if val.starts_with('{') {
                        extract_poetry_inline_version(val)
                            .unwrap_or_else(|| "(complex)".to_string())
                    } else if val.starts_with('[') {
                        "(complex)".to_string()
                    } else {
                        val.trim_matches('"').trim_matches('\'').to_string()
                    };
                    if constraint.starts_with("==") {
                        let ver = constraint[2..].split(',').next().unwrap_or("").trim();
                        if !ver.is_empty() {
                            pkgs.push(Pkg::exact(key, ver, "PyPI", source));
                            continue;
                        }
                    }
                    if !constraint.is_empty() {
                        pkgs.push(Pkg::approximate(key, &constraint, "PyPI", source));
                    }
                }
            }
        }
    }
    pkgs
}

/// Strip environment markers, multiple constraints, and hash specs from a raw exact
/// version token so that OSV receives a plain semver string like `2.31.0`.
///
/// Examples:
///   `2.31.0; python_version < "3.12"` → `2.31.0`
///   `2.31.0,<3.0`                     → `2.31.0`
///   `2.31.0 --hash=sha256:abc`        → `2.31.0`
fn clean_exact_version(raw: &str) -> String {
    // Split at `,` (additional constraint), `;` (env marker), or whitespace (hash/option)
    raw.splitn(2, |c| c == ',' || c == ';')
        .next()
        .unwrap_or(raw)
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Parse one line from a multi-line TOML dep array. Strips comments, trailing commas,
/// and surrounding quotes before dispatching to `add_pep508_dep`.
fn parse_pep508_array_line(trimmed: &str, source: &str, pkgs: &mut Vec<Pkg>) {
    let trimmed = strip_unquoted_comment(trimmed).trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return;
    }
    let dep = trimmed
        .trim_matches(|c| c == '"' || c == '\'' || c == ',')
        .trim();
    if !dep.is_empty() {
        add_pep508_dep(dep, source, pkgs);
    }
}

/// Add a single PEP 508 dependency string to `pkgs`.
fn add_pep508_dep(dep: &str, source: &str, pkgs: &mut Vec<Pkg>) {
    let (name, constraint) = split_dep_spec(dep);
    let name = name.trim().to_lowercase();
    if name.is_empty() {
        return;
    }
    if constraint.starts_with("==") {
        let ver = clean_exact_version(&constraint[2..]);
        if !ver.is_empty() {
            pkgs.push(Pkg::exact(name, &ver, "PyPI", source));
            return;
        }
    }
    let c = if constraint.is_empty() {
        "(any)"
    } else {
        constraint
    };
    pkgs.push(Pkg::approximate(name, c, "PyPI", source));
}

/// Find the position of the first `]` that is not inside a quoted string.
/// Used to locate the closing bracket of a TOML inline array without being
/// confused by extras notation like `fastapi[standard]`.
fn find_unquoted_close_bracket(s: &str) -> Option<usize> {
    let mut in_quote = false;
    let mut quote_char = '"';
    for (i, c) in s.char_indices() {
        if in_quote {
            if c == quote_char {
                in_quote = false;
            }
        } else if c == '"' || c == '\'' {
            in_quote = true;
            quote_char = c;
        } else if c == ']' {
            return Some(i);
        }
    }
    None
}

/// Remove a TOML comment, preserving `#` characters inside quoted strings.
fn strip_unquoted_comment(s: &str) -> &str {
    let mut in_quote = false;
    let mut quote_char = '"';
    let mut escaped = false;

    for (i, c) in s.char_indices() {
        if in_quote {
            if escaped {
                escaped = false;
            } else if c == '\\' && quote_char == '"' {
                escaped = true;
            } else if c == quote_char {
                in_quote = false;
            }
        } else if c == '"' || c == '\'' {
            in_quote = true;
            quote_char = c;
        } else if c == '#' {
            return &s[..i];
        }
    }

    s
}

/// Extract quoted string items from a TOML inline array fragment.
/// e.g. `"fastapi>=0.129.0", "requests==2.31.0"` → vec!["fastapi>=0.129.0", "requests==2.31.0"]
fn extract_toml_array_strings(s: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut in_quote = false;
    let mut quote_char = '"';
    let mut current = String::new();
    for c in s.chars() {
        if in_quote {
            if c == quote_char {
                in_quote = false;
                if !current.is_empty() {
                    result.push(std::mem::take(&mut current));
                }
            } else {
                current.push(c);
            }
        } else if c == '"' || c == '\'' {
            in_quote = true;
            quote_char = c;
        }
    }
    result
}

/// Extract `version` from a Poetry inline table like `{version = "^1.0.0", extras = [...]}`.
fn extract_poetry_inline_version(s: &str) -> Option<String> {
    for part in s.split(',') {
        let part = part
            .trim()
            .trim_start_matches('{')
            .trim_end_matches('}')
            .trim();
        if let Some((key, val)) = part.split_once(" = ") {
            if key.trim() == "version" {
                return Some(val.trim().trim_matches('"').trim_matches('\'').to_string());
            }
        }
    }
    None
}

/// Split `fastapi[standard]>=0.129.0` → ("fastapi", ">=0.129.0").
fn split_dep_spec(dep: &str) -> (&str, &str) {
    // Strip PEP 508 environment markers: everything after the first unquoted ';'
    // e.g. `importlib-metadata; python_version < "3.10"` → `importlib-metadata`
    let dep = dep.split(';').next().unwrap_or(dep).trim();

    // PEP 508 direct reference: `name[extra] @ https://...`.
    // OSV cannot query the URL, but it can still query the package name.
    if let Some((name_part, _url)) = dep.split_once(" @ ") {
        if let Some(pos) = name_part.find('[') {
            return (name_part[..pos].trim(), "");
        }
        return (name_part.trim(), "");
    }

    if let Some(pos) = dep.find('[') {
        let end = dep.find(']').map(|p| p + 1).unwrap_or(dep.len());
        let name = dep[..pos].trim();
        let rest = dep[end..].trim();
        return (name, rest);
    }

    let specifiers = ['>', '<', '=', '~', '!'];
    if let Some(pos) = dep.find(|c| specifiers.contains(&c)) {
        (dep[..pos].trim(), dep[pos..].trim())
    } else {
        (dep.trim(), "")
    }
}

fn dedup_pkgs(mut pkgs: Vec<Pkg>) -> Vec<Pkg> {
    let mut seen: HashSet<(String, Option<String>, &'static str)> = HashSet::new();
    pkgs.retain(|p| seen.insert((p.name.clone(), p.version.clone(), p.ecosystem)));
    pkgs
}

// ── Lockfile discovery ────────────────────────────────────────────────────────

/// Files recognised as lockfiles, grouped by ecosystem.
/// Priority order: earlier entries beat later ones per ecosystem when deduplicating.
const LOCKFILE_SPECS: &[(&str, &str)] = &[
    // npm
    ("package-lock.json", "npm"),
    ("npm-shrinkwrap.json", "npm"),
    ("yarn.lock", "npm"),
    ("pnpm-lock.yaml", "npm"),
    // rust
    ("Cargo.lock", "crates.io"),
    // python
    ("poetry.lock", "PyPI"),
    ("uv.lock", "PyPI"),
    ("Pipfile.lock", "PyPI"),
    ("requirements.txt", "PyPI"),
    ("requirements-dev.txt", "PyPI"),
    ("requirements_dev.txt", "PyPI"),
    ("requirements-test.txt", "PyPI"),
    ("pyproject.toml", "PyPI"), // fallback / approximate
];

fn is_dev_requirements_file(name: &str) -> bool {
    matches!(
        name,
        "requirements-dev.txt" | "requirements_dev.txt" | "requirements-test.txt"
    )
}

fn ecosystem_for_project(project: &str) -> Option<&'static str> {
    match project {
        "web" => Some("npm"),
        "rust" => Some("crates.io"),
        "python" => Some("PyPI"),
        "auto" => None,
        _ => None,
    }
}

struct DiscoveredSource {
    lockfile_path: String,
    packages: Vec<Pkg>,
    approximate: bool,
}

fn discover_packages(
    base: &Path,
    project: &str,
    exclude: &[String],
    include_dev: bool,
) -> (Vec<DiscoveredSource>, Vec<String>) {
    let target_ecosystem = ecosystem_for_project(project);
    let mut warnings: Vec<String> = Vec::new();

    // Pre-compile exclude patterns once. Strip trailing `/**` or `/*` so patterns
    // are matched against individual path component names, not full paths.
    let exclude_patterns: Vec<glob::Pattern> = exclude
        .iter()
        .filter_map(|p| {
            let pat = p.trim_end_matches("/**").trim_end_matches("/*");
            glob::Pattern::new(pat).ok()
        })
        .collect();

    let mut found: Vec<(std::path::PathBuf, &str)> = Vec::new();

    for entry in WalkDir::new(base)
        .max_depth(5)
        .into_iter()
        .filter_entry(|e| {
            if e.path() == base {
                return true;
            }
            let components: Vec<&str> = e
                .path()
                .strip_prefix(base)
                .unwrap_or(e.path())
                .components()
                .filter_map(|c| c.as_os_str().to_str())
                .collect();

            for pat in &exclude_patterns {
                for comp in &components {
                    if pat.matches(comp) {
                        return false;
                    }
                }
            }
            true
        })
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let file_name = entry.file_name().to_string_lossy();
        for &(lf_name, eco) in LOCKFILE_SPECS {
            if file_name.as_ref() == lf_name {
                // Filter by ecosystem when project is not "auto"
                if let Some(target) = target_ecosystem {
                    if eco != target {
                        continue;
                    }
                }
                // Skip dev/test requirement files when include_dev is false
                if !include_dev && is_dev_requirements_file(lf_name) {
                    break;
                }
                found.push((entry.path().to_path_buf(), eco));
                break;
            }
        }
    }

    // For npm: keep only the highest-priority lockfile per directory.
    // For python: drop pyproject.toml when an exact lockfile is present.
    let found = deduplicate_npm_per_dir(found);
    let found = deduplicate_python_per_dir(found);

    // Parse each lockfile
    let mut sources: Vec<DiscoveredSource> = Vec::new();

    for (path, _eco) in found {
        let rel = path
            .strip_prefix(base)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| path.to_string_lossy().into_owned());

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                warnings.push(format!("Could not read {}: {}", rel, e));
                continue;
            }
        };

        let file_name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();

        let (pkgs, approx) = parse_lockfile(&file_name, &content, &rel, include_dev);

        if pkgs.is_empty() {
            // Only warn for approximate files (pyproject.toml with no deps is normal)
            if !approx && !content.trim().is_empty() {
                warnings.push(format!("No packages parsed from {}", rel));
            }
            continue;
        }

        sources.push(DiscoveredSource {
            lockfile_path: rel,
            packages: pkgs,
            approximate: approx,
        });
    }

    (sources, warnings)
}

/// For npm, when multiple lockfiles exist in the same directory, keep only the
/// highest-priority one (package-lock.json > yarn.lock > pnpm-lock.yaml).
fn deduplicate_npm_per_dir(
    found: Vec<(std::path::PathBuf, &'static str)>,
) -> Vec<(std::path::PathBuf, &'static str)> {
    let npm_priority = |name: &str| match name {
        "package-lock.json" | "npm-shrinkwrap.json" => 0u8,
        "yarn.lock" => 1,
        "pnpm-lock.yaml" => 2,
        _ => 3,
    };

    let mut dir_npm_best: HashMap<std::path::PathBuf, (u8, std::path::PathBuf)> = HashMap::new();
    let mut non_npm: Vec<(std::path::PathBuf, &'static str)> = Vec::new();

    for (path, eco) in found {
        if eco == "npm" {
            let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
            let prio = npm_priority(
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .as_ref(),
            );
            dir_npm_best
                .entry(dir)
                .and_modify(|e| {
                    if prio < e.0 {
                        *e = (prio, path.clone());
                    }
                })
                .or_insert((prio, path));
        } else {
            non_npm.push((path, eco));
        }
    }

    let npm_entries: Vec<(std::path::PathBuf, &'static str)> = dir_npm_best
        .into_values()
        .map(|(_, path)| (path, "npm"))
        .collect();

    let mut result = non_npm;
    result.extend(npm_entries);
    result
}

/// If a directory already has an exact Python lockfile (poetry.lock, uv.lock, Pipfile.lock),
/// remove pyproject.toml from that same directory to avoid mixing approximate range results
/// with accurate locked results.
fn deduplicate_python_per_dir(
    found: Vec<(std::path::PathBuf, &'static str)>,
) -> Vec<(std::path::PathBuf, &'static str)> {
    // Only exact lockfiles suppress pyproject.toml. requirements.txt is excluded because
    // it is often partial, target-specific, or unpinned — dropping pyproject.toml could
    // cause false negatives for deps only declared there.
    const EXACT_PYTHON: &[&str] = &["poetry.lock", "uv.lock", "Pipfile.lock"];

    let dirs_with_exact: HashSet<std::path::PathBuf> = found
        .iter()
        .filter(|(path, _)| {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            EXACT_PYTHON.contains(&name.as_ref())
        })
        .filter_map(|(path, _)| path.parent().map(|p| p.to_path_buf()))
        .collect();

    if dirs_with_exact.is_empty() {
        return found;
    }

    found
        .into_iter()
        .filter(|(path, _)| {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if name.as_ref() == "pyproject.toml" {
                let dir = path.parent().unwrap_or(Path::new("."));
                !dirs_with_exact.contains(dir)
            } else {
                true
            }
        })
        .collect()
}

fn parse_lockfile(
    file_name: &str,
    content: &str,
    rel: &str,
    include_dev: bool,
) -> (Vec<Pkg>, bool) {
    match file_name {
        "package-lock.json" | "npm-shrinkwrap.json" => {
            (parse_package_lock_json(content, rel, include_dev), false)
        }
        "yarn.lock" => (parse_yarn_lock(content, rel), false),
        "pnpm-lock.yaml" => (parse_pnpm_lock_yaml(content, rel), false),
        "Cargo.lock" => (parse_cargo_lock(content, rel), false),
        "poetry.lock" => (parse_poetry_lock(content, rel, include_dev), false),
        "uv.lock" => (parse_uv_lock(content, rel), false),
        "Pipfile.lock" => (parse_pipfile_lock(content, rel, include_dev), false),
        n if n.starts_with("requirements") && n.ends_with(".txt") => {
            (parse_requirements_txt(content, rel), false)
        }
        "pyproject.toml" => (parse_pyproject_toml(content, rel, include_dev), true),
        _ => (vec![], false),
    }
}

// ── OSV.dev helpers ───────────────────────────────────────────────────────────

fn extract_fix(affected: &Value, pkg: &Pkg) -> Option<String> {
    for entry in affected.as_array()? {
        if !affected_entry_matches_pkg(entry, pkg) {
            continue;
        }
        for range in entry["ranges"].as_array()? {
            for event in range["events"].as_array()? {
                if let Some(fixed) = event["fixed"].as_str() {
                    return Some(fixed.to_string());
                }
            }
        }
    }
    None
}

fn affected_entry_matches_pkg(entry: &Value, pkg: &Pkg) -> bool {
    let package = &entry["package"];
    let Some(name) = package["name"].as_str() else {
        return false;
    };
    let Some(ecosystem) = package["ecosystem"].as_str() else {
        return false;
    };

    package_name_matches(name, &pkg.name, pkg.ecosystem)
        && ecosystem.eq_ignore_ascii_case(pkg.ecosystem)
}

fn package_name_matches(osv_name: &str, pkg_name: &str, ecosystem: &str) -> bool {
    if ecosystem == "PyPI" {
        let normalize = |s: &str| s.replace(['_', '.'], "-").to_lowercase();
        normalize(osv_name) == normalize(pkg_name)
    } else {
        osv_name == pkg_name
    }
}

fn vuln_severity(vuln: &Value) -> &'static str {
    // 1. GitHub advisory database_specific.severity (plain label)
    if let Some(s) = vuln["database_specific"]["severity"].as_str() {
        return normalize_severity_label(s);
    }
    // 2. CVSS score/vector in severity array
    if let Some(arr) = vuln["severity"].as_array() {
        for item in arr {
            if let Some(score_str) = item["score"].as_str() {
                let label = cvss_label(score_str);
                if label != "UNKNOWN" {
                    return label;
                }
            }
        }
    }
    "UNKNOWN"
}

/// Normalise a raw advisory severity label to CRITICAL/HIGH/MEDIUM/LOW/NONE.
/// GitHub uses "MODERATE" instead of "MEDIUM".
fn normalize_severity_label(s: &str) -> &'static str {
    if s.eq_ignore_ascii_case("MODERATE") || s.eq_ignore_ascii_case("MEDIUM") {
        "MEDIUM"
    } else if s.eq_ignore_ascii_case("CRITICAL") {
        "CRITICAL"
    } else if s.eq_ignore_ascii_case("HIGH") {
        "HIGH"
    } else if s.eq_ignore_ascii_case("LOW") {
        "LOW"
    } else if s.eq_ignore_ascii_case("NONE") {
        "NONE"
    } else {
        "UNKNOWN"
    }
}

fn cvss_label(vector: &str) -> &'static str {
    // Try numeric float first (some providers embed the score as a leading token)
    if let Some(s) = vector
        .split_whitespace()
        .next()
        .and_then(|s| s.parse::<f64>().ok())
    {
        return numeric_severity(s);
    }
    // Try computing from a CVSS 3.x vector string
    if vector.starts_with("CVSS:3") {
        if let Some(score) = parse_cvss3_score(vector) {
            return numeric_severity(score);
        }
    }
    "UNKNOWN"
}

/// Compute the CVSS 3.x base score from a vector string such as
/// `CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H`.
/// Returns `None` if the vector is malformed or contains unrecognised values.
fn parse_cvss3_score(vector: &str) -> Option<f64> {
    // Drop the "CVSS:3.x/" prefix and collect key:value pairs.
    let metrics: Vec<(&str, &str)> = vector
        .splitn(2, '/')
        .nth(1)?
        .split('/')
        .filter_map(|c| c.split_once(':'))
        .collect();

    let get =
        |key: &str| -> Option<&str> { metrics.iter().find(|(k, _)| *k == key).map(|(_, v)| *v) };

    let scope_changed = get("S")? == "C";

    let av: f64 = match get("AV")? {
        "N" => 0.85,
        "A" => 0.62,
        "L" => 0.55,
        "P" => 0.20,
        _ => return None,
    };
    let ac: f64 = match get("AC")? {
        "L" => 0.77,
        "H" => 0.44,
        _ => return None,
    };
    let pr: f64 = match (get("PR")?, scope_changed) {
        ("N", _) => 0.85,
        ("L", false) => 0.62,
        ("L", true) => 0.68,
        ("H", false) => 0.27,
        ("H", true) => 0.50,
        _ => return None,
    };
    let ui: f64 = match get("UI")? {
        "N" => 0.85,
        "R" => 0.62,
        _ => return None,
    };
    let c_imp: f64 = match get("C")? {
        "N" => 0.00,
        "L" => 0.22,
        "H" => 0.56,
        _ => return None,
    };
    let i_imp: f64 = match get("I")? {
        "N" => 0.00,
        "L" => 0.22,
        "H" => 0.56,
        _ => return None,
    };
    let a_imp: f64 = match get("A")? {
        "N" => 0.00,
        "L" => 0.22,
        "H" => 0.56,
        _ => return None,
    };

    let iss = 1.0 - (1.0 - c_imp) * (1.0 - i_imp) * (1.0 - a_imp);

    let impact = if scope_changed {
        7.52 * (iss - 0.029) - 3.25 * (iss - 0.02_f64).powi(15)
    } else {
        6.42 * iss
    };

    if impact <= 0.0 {
        return Some(0.0);
    }

    let exploitability = 8.22 * av * ac * pr * ui;

    let raw = if scope_changed {
        f64::min(1.08 * (impact + exploitability), 10.0)
    } else {
        f64::min(impact + exploitability, 10.0)
    };

    // CVSS spec: round up to one decimal place (ceiling)
    Some((raw * 10.0).ceil() / 10.0)
}

fn numeric_severity(score: f64) -> &'static str {
    if score >= 9.0 {
        "CRITICAL"
    } else if score >= 7.0 {
        "HIGH"
    } else if score >= 4.0 {
        "MEDIUM"
    } else if score > 0.0 {
        "LOW"
    } else {
        "NONE"
    }
}

fn severity_rank(s: &str) -> u8 {
    match s {
        "CRITICAL" => 4,
        "HIGH" => 3,
        "MEDIUM" | "MODERATE" => 2,
        "LOW" => 1,
        _ => 0,
    }
}

// ── Tool implementation ───────────────────────────────────────────────────────

#[async_trait]
impl Tool for AuditTool {
    fn name(&self) -> &str {
        "Audit"
    }

    fn description(&self) -> &str {
        "Scan packages for known security vulnerabilities using OSV.dev (no API key required). \
         Two modes: (1) pass a `packages` list of {name,version} pairs, or (2) pass a `path` to \
         a project directory and let the tool discover lockfiles automatically. \
         `project` selects ecosystem: 'web'=npm, 'rust'=crates.io, 'python'=PyPI, \
         'auto'=detect all ecosystems from lockfiles. \
         Output is structured for AI consumption: severity labels (CRITICAL/HIGH/MEDIUM/LOW), \
         CVE aliases, fixed versions, and advisory IDs at the top level."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Web
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "project": {
                    "type": "string",
                    "enum": ["web", "rust", "python", "auto"],
                    "description": "'web'=npm only, 'rust'=crates.io only, 'python'=PyPI only, 'auto'=detect all from lockfiles. Required when using 'packages'; optional with 'path' (defaults to 'auto')."
                },
                "packages": {
                    "type": "array",
                    "description": "Explicit list of packages to audit. Alternative to `path`.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": {"type": "string", "description": "Package name as in the registry."},
                            "version": {"type": "string", "description": "Exact version, e.g. '1.2.3'."},
                            "ecosystem": {"type": "string", "description": "Optional override (e.g. 'npm', 'crates.io', 'PyPI'). Defaults to ecosystem implied by `project`."}
                        },
                        "required": ["name", "version"]
                    },
                    "minItems": 1
                },
                "path": {
                    "type": "string",
                    "description": "Directory to scan for lockfiles. Supported: package-lock.json, npm-shrinkwrap.json, yarn.lock, pnpm-lock.yaml (npm); Cargo.lock (rust); poetry.lock, uv.lock, Pipfile.lock, requirements*.txt, pyproject.toml (python). Scans up to 5 directory levels deep."
                },
                "exclude": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Path component names to exclude during discovery (e.g. ['.venv', 'node_modules', 'vendor']). Defaults to ['.venv', 'node_modules', '.git']."
                },
                "include_dev": {
                    "type": "boolean",
                    "description": "Include dev/test dependencies (default: false). Applies to: package-lock.json dev flag, Pipfile.lock develop section, requirements-dev.txt."
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        #[derive(Deserialize)]
        struct PackageInput {
            name: String,
            version: String,
            ecosystem: Option<String>,
        }

        #[derive(Deserialize)]
        struct Input {
            project: Option<String>,
            packages: Option<Vec<PackageInput>>,
            path: Option<String>,
            exclude: Option<Vec<String>>,
            include_dev: Option<bool>,
        }

        let input: Input = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        let project = input.project.as_deref().unwrap_or("auto");
        let include_dev = input.include_dev.unwrap_or(false);
        let exclude: Vec<String> = input
            .exclude
            .unwrap_or_else(|| vec![".venv".into(), "node_modules".into(), ".git".into()]);

        // Validate project type
        if !matches!(project, "web" | "rust" | "python" | "auto") {
            return ToolResult::error(format!(
                "Unknown project type '{}'; expected 'web', 'rust', 'python', or 'auto'.",
                project
            ));
        }

        // ── Collect packages ──────────────────────────────────────────────────

        let mut all_sources: Vec<DiscoveredSource> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();

        // Mode 1: path-based discovery
        if let Some(ref path_str) = input.path {
            let base = if std::path::Path::new(path_str).is_absolute() {
                std::path::PathBuf::from(path_str)
            } else {
                ctx.working_dir.join(path_str)
            };

            if !base.exists() {
                return ToolResult::error(format!("Path does not exist: {}", base.display()));
            }

            let (sources, w) = discover_packages(&base, project, &exclude, include_dev);
            all_sources.extend(sources);
            warnings.extend(w);
        }

        // Mode 2: explicit packages
        if let Some(pkg_list) = input.packages {
            let default_eco: Option<&'static str> = match project {
                "web" => Some("npm"),
                "rust" => Some("crates.io"),
                "python" => Some("PyPI"),
                _ => None,
            };

            let mut pkgs: Vec<Pkg> = Vec::new();
            let mut skipped_wrong_eco: Vec<String> = Vec::new();

            for p in pkg_list {
                let eco: &str = match p.ecosystem.as_deref().or(default_eco) {
                    Some(e) => e,
                    None => {
                        return ToolResult::error(format!(
                            "Package '{}@{}' has no ecosystem. \
                             Set `project` to 'web', 'rust', or 'python', \
                             or add an `ecosystem` field to each package.",
                            p.name, p.version
                        ));
                    }
                };

                // Validate ecosystem matches project (unless auto)
                if project != "auto" {
                    if let Some(target) = default_eco {
                        if eco != target {
                            skipped_wrong_eco
                                .push(format!("{}@{} (ecosystem {})", p.name, p.version, eco));
                            continue;
                        }
                    }
                }

                let eco_static: &'static str = match eco {
                    "npm" => "npm",
                    "crates.io" => "crates.io",
                    "PyPI" | "pypi" => "PyPI",
                    "go" | "Go" => "Go",
                    "Maven" | "maven" => "Maven",
                    other => {
                        warnings.push(format!(
                            "Unknown ecosystem '{}' for {}@{} — skipping.",
                            other, p.name, p.version
                        ));
                        continue;
                    }
                };

                pkgs.push(Pkg::exact(p.name, p.version, eco_static, "(explicit)"));
            }

            if !skipped_wrong_eco.is_empty() {
                warnings.push(format!(
                    "Skipped (wrong ecosystem for project '{}'): {}",
                    project,
                    skipped_wrong_eco.join(", ")
                ));
            }

            if !pkgs.is_empty() {
                all_sources.push(DiscoveredSource {
                    lockfile_path: "(explicit)".into(),
                    packages: pkgs,
                    approximate: false,
                });
            }
        }

        if all_sources.is_empty() {
            return ToolResult::error(
                "No packages found to audit. Provide either `packages` or a `path` containing \
                 supported lockfiles (package-lock.json, Cargo.lock, poetry.lock, uv.lock, \
                 Pipfile.lock, requirements*.txt, pnpm-lock.yaml, yarn.lock, pyproject.toml)."
                    .to_string(),
            );
        }

        // Merge packages from all sources into a flat list for batch querying.
        let all_pkgs: Vec<&Pkg> = all_sources
            .iter()
            .flat_map(|src| src.packages.iter())
            .collect();

        // ── Build OSV batch query ─────────────────────────────────────────────

        let queries: Vec<Value> = all_pkgs
            .iter()
            .map(|pkg| {
                let mut q = serde_json::json!({
                    "package": { "name": pkg.name, "ecosystem": pkg.ecosystem }
                });
                if let Some(ver) = pkg.version.as_deref() {
                    q["version"] = Value::String(ver.to_owned());
                }
                q
            })
            .collect();

        let client = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent("Cersei-Agent/0.1")
            .build()
        {
            Ok(c) => c,
            Err(e) => return ToolResult::error(format!("Failed to build HTTP client: {}", e)),
        };

        // OSV.dev querybatch supports up to 1000 queries per request.
        // Chunk large package lists and merge the results in order.
        const OSV_BATCH_LIMIT: usize = 1000;
        let mut all_results: Vec<Value> = Vec::with_capacity(queries.len());

        for chunk in queries.chunks(OSV_BATCH_LIMIT) {
            let response = match client
                .post("https://api.osv.dev/v1/querybatch")
                .json(&serde_json::json!({ "queries": chunk }))
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => return ToolResult::error(format!("OSV.dev request failed: {}", e)),
            };

            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                return ToolResult::error(format!(
                    "OSV.dev API returned HTTP {}: {}",
                    status.as_u16(),
                    body
                ));
            }

            let data: Value = match response.json().await {
                Ok(j) => j,
                Err(e) => {
                    return ToolResult::error(format!("Failed to parse OSV.dev response: {}", e))
                }
            };

            match data["results"].as_array() {
                Some(r) => all_results.extend(r.iter().cloned()),
                None => {
                    return ToolResult::error(
                        "Unexpected OSV.dev response: missing 'results' array.".to_string(),
                    )
                }
            }
        }

        let results = &all_results;

        // ── Correlate ─────────────────────────────────────────────────────────

        struct Finding<'a> {
            pkg: &'a Pkg,
            vulns: &'a [Value],
            max_severity: u8,
        }

        let mut findings: Vec<Finding> = Vec::new();
        let mut clean_count = 0usize;

        for (i, result) in results.iter().enumerate() {
            let pkg = all_pkgs[i];
            let vulns = result["vulns"].as_array();
            match vulns {
                Some(v) if !v.is_empty() => {
                    let max_severity = v
                        .iter()
                        .map(|vuln| severity_rank(vuln_severity(vuln)))
                        .max()
                        .unwrap_or(0);
                    findings.push(Finding {
                        pkg,
                        vulns: v.as_slice(),
                        max_severity,
                    });
                }
                _ => clean_count += 1,
            }
        }

        // ── Format report ─────────────────────────────────────────────────────

        let total_scanned = all_pkgs.len();
        let total_vulns: usize = findings.iter().map(|f| f.vulns.len()).sum();
        let (mut crit, mut high, mut med, mut low, mut unk) = (0, 0, 0, 0, 0);
        for f in &findings {
            for v in f.vulns {
                match vuln_severity(v) {
                    "CRITICAL" => crit += 1,
                    "HIGH" => high += 1,
                    "MEDIUM" => med += 1,
                    "LOW" => low += 1,
                    _ => unk += 1,
                }
            }
        }

        let mut out = String::new();
        out.push_str("# Vulnerability Audit Report\n\n");

        // Sources section
        if all_sources.len() > 1
            || all_sources
                .first()
                .map(|s| s.lockfile_path != "(explicit)")
                .unwrap_or(false)
        {
            out.push_str("## Sources\n");
            for src in &all_sources {
                let note = if src.approximate {
                    " [APPROXIMATE — no lockfile, version ranges only]"
                } else {
                    ""
                };
                out.push_str(&format!(
                    "  - {} ({} packages{})\n",
                    src.lockfile_path,
                    src.packages.len(),
                    note
                ));
            }
            out.push('\n');
        }

        out.push_str(&format!("Scanned   : {} packages\n", total_scanned));
        out.push_str(&format!(
            "Vulnerable: {} packages, {} vulnerabilities\n",
            findings.len(),
            total_vulns
        ));
        out.push_str(&format!("Clean     : {} packages\n", clean_count));

        if total_vulns > 0 {
            out.push_str(&format!(
                "Severity  : CRITICAL={} HIGH={} MEDIUM={} LOW={} UNKNOWN={}\n",
                crit, high, med, low, unk
            ));
        }

        for w in &warnings {
            out.push_str(&format!("Warning   : {}\n", w));
        }

        if findings.is_empty() {
            out.push_str("\nResult: NO KNOWN VULNERABILITIES FOUND\n");
            return ToolResult::success(out);
        }

        out.push_str("\n---\n\n");

        // Sort findings: highest severity first (uses cached max_severity).
        let mut sorted: Vec<&Finding> = findings.iter().collect();
        sorted.sort_by_key(|f| std::cmp::Reverse(f.max_severity));

        for f in sorted {
            let ver_display = f
                .pkg
                .version
                .as_deref()
                .map(|v| format!("@{v}"))
                .unwrap_or_else(|| {
                    f.pkg
                        .constraint
                        .as_deref()
                        .map(|c| format!(" ({c}) [APPROXIMATE]"))
                        .unwrap_or_default()
                });

            out.push_str(&format!(
                "## {}:{}{} — {}\n\n",
                f.pkg.ecosystem, f.pkg.name, ver_display, f.pkg.source
            ));

            // Precompute severity once per vuln to avoid recomputing in sort + display.
            let mut vulns_with_sev: Vec<(&Value, &'static str)> =
                f.vulns.iter().map(|v| (v, vuln_severity(v))).collect();
            vulns_with_sev.sort_by_key(|(_, s)| std::cmp::Reverse(severity_rank(s)));

            for (vuln, severity) in &vulns_with_sev {
                let id = vuln["id"].as_str().unwrap_or("UNKNOWN");
                let summary = vuln["summary"].as_str().unwrap_or("(no summary)");
                let fix = extract_fix(&vuln["affected"], f.pkg)
                    .map(|v| format!("upgrade to >= {}", v))
                    .unwrap_or_else(|| "no fix version recorded".to_string());
                let published = vuln["published"]
                    .as_str()
                    .and_then(|s| s.get(..10))
                    .unwrap_or("unknown");
                let aliases: Vec<&str> = vuln["aliases"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
                    .unwrap_or_default();
                let cwes: Vec<&str> = vuln["database_specific"]["cwe_ids"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
                    .unwrap_or_default();
                let ref_url = vuln["references"]
                    .as_array()
                    .and_then(|refs| refs.first())
                    .and_then(|r| r["url"].as_str())
                    .unwrap_or("");

                out.push_str(&format!("### [{severity}] {id} — {summary}\n"));
                if !aliases.is_empty() {
                    out.push_str(&format!("- Aliases  : {}\n", aliases.join(", ")));
                }
                out.push_str(&format!("- Severity : {severity}\n"));
                out.push_str(&format!("- Fix      : {fix}\n"));
                out.push_str(&format!("- Published: {published}\n"));
                if !cwes.is_empty() {
                    out.push_str(&format!("- CWE      : {}\n", cwes.join(", ")));
                }
                if !ref_url.is_empty() {
                    out.push_str(&format!("- Advisory : {ref_url}\n"));
                }
                if f.pkg.version.is_none() {
                    out.push_str("- NOTE     : Version range query — confirm whether your installed version is affected.\n");
                }
                out.push('\n');
            }
        }

        ToolResult::success(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::AllowAll;
    use std::sync::Arc;

    fn test_ctx() -> ToolContext {
        ToolContext {
            working_dir: std::env::temp_dir(),
            session_id: "test".into(),
            permissions: Arc::new(AllowAll),
            cost_tracker: Arc::new(Default::default()),
            mcp_manager: None,
            extensions: Default::default(),
            network_policy: None,
        }
    }

    // ── Schema tests ──────────────────────────────────────────────────────────

    #[test]
    fn test_permission_and_category() {
        assert_eq!(AuditTool.permission_level(), PermissionLevel::ReadOnly);
        assert_eq!(AuditTool.category(), ToolCategory::Web);
    }

    // ── Parser unit tests ─────────────────────────────────────────────────────

    #[test]
    fn test_parse_package_lock_v3() {
        let content = r#"{
            "lockfileVersion": 3,
            "packages": {
                "": {"version": "1.0.0"},
                "node_modules/lodash": {"version": "4.17.21"},
                "node_modules/@scope/pkg": {"version": "2.0.0"},
                "node_modules/jest": {"version": "29.0.0", "dev": true},
                "packages/app": {"version": "0.1.0"},
                "apps/website": {"version": "1.0.0"}
            }
        }"#;
        let pkgs = parse_package_lock_json(content, "package-lock.json", false);
        assert_eq!(pkgs.len(), 2);
        assert!(pkgs
            .iter()
            .any(|p| p.name == "lodash" && p.version.as_deref() == Some("4.17.21")));
        assert!(pkgs.iter().any(|p| p.name == "@scope/pkg"));
        assert!(pkgs.iter().all(|p| p.name != "jest")); // dev excluded
        assert!(pkgs.iter().all(|p| p.name != "packages/app")); // workspace skipped
        assert!(pkgs.iter().all(|p| p.name != "apps/website")); // workspace skipped
    }

    #[test]
    fn test_parse_package_lock_v3_include_dev() {
        let content = r#"{
            "lockfileVersion": 3,
            "packages": {
                "": {},
                "node_modules/lodash": {"version": "4.17.21"},
                "node_modules/jest": {"version": "29.0.0", "dev": true}
            }
        }"#;
        let pkgs = parse_package_lock_json(content, "package-lock.json", true);
        assert_eq!(pkgs.len(), 2);
        assert!(pkgs.iter().any(|p| p.name == "jest"));
    }

    #[test]
    fn test_parse_cargo_lock() {
        let content = r#"
[[package]]
name = "serde"
version = "1.0.130"
source = "registry+https://..."

[[package]]
name = "tokio"
version = "1.20.0"
"#;
        let pkgs = parse_cargo_lock(content, "Cargo.lock");
        assert_eq!(pkgs.len(), 2);
        assert!(pkgs
            .iter()
            .any(|p| p.name == "serde" && p.version.as_deref() == Some("1.0.130")));
        assert!(pkgs.iter().any(|p| p.name == "tokio"));
    }

    #[test]
    fn test_parse_poetry_lock() {
        let content = r#"
[[package]]
name = "requests"
version = "2.28.1"
description = "Python HTTP library"

[[package]]
name = "Flask"
version = "2.3.0"
"#;
        let pkgs = parse_poetry_lock(content, "poetry.lock", false);
        assert_eq!(pkgs.len(), 2);
        // Names should be lowercased
        assert!(pkgs.iter().any(|p| p.name == "flask"));
        assert!(pkgs.iter().any(|p| p.name == "requests"));
    }

    #[test]
    fn test_parse_poetry_lock_dev_filter() {
        let content = r#"
[[package]]
name = "requests"
version = "2.28.1"
category = "main"

[[package]]
name = "pytest"
version = "7.0.0"
category = "dev"

[[package]]
name = "coverage"
version = "7.3.0"
groups = ["dev"]

[[package]]
name = "flask"
version = "2.3.0"
groups = ["main"]

[[package]]
name = "shared-lib"
version = "1.0.0"
groups = ["main", "dev"]
"#;
        let pkgs_no_dev = parse_poetry_lock(content, "poetry.lock", false);
        let names_no_dev: Vec<&str> = pkgs_no_dev.iter().map(|p| p.name.as_str()).collect();
        assert!(
            names_no_dev.contains(&"requests"),
            "requests (main) should be included"
        );
        assert!(
            names_no_dev.contains(&"flask"),
            "flask (main group) should be included"
        );
        assert!(
            names_no_dev.contains(&"shared-lib"),
            "packages in main+dev should stay included"
        );
        assert!(
            !names_no_dev.contains(&"pytest"),
            "pytest (category=dev) should be excluded"
        );
        assert!(
            !names_no_dev.contains(&"coverage"),
            "coverage (groups=[dev]) should be excluded"
        );

        let pkgs_with_dev = parse_poetry_lock(content, "poetry.lock", true);
        assert_eq!(
            pkgs_with_dev.len(),
            5,
            "all packages included with include_dev=true"
        );
    }

    #[test]
    fn test_parse_requirements_txt() {
        let content = r#"
# production deps
requests==2.28.1
flask>=2.0.0,<3.0
pytest  # no version
Django==4.2.0
"#;
        let pkgs = parse_requirements_txt(content, "requirements.txt");
        let exact: Vec<_> = pkgs.iter().filter(|p| p.version.is_some()).collect();
        let approx: Vec<_> = pkgs.iter().filter(|p| p.version.is_none()).collect();
        assert_eq!(exact.len(), 2); // requests, Django
        assert_eq!(approx.len(), 2); // flask (range), pytest (no ver)
        assert!(exact
            .iter()
            .any(|p| p.name == "requests" && p.version.as_deref() == Some("2.28.1")));
        assert!(exact.iter().any(|p| p.name == "django"));
    }

    #[test]
    fn test_parse_requirements_txt_markers() {
        // PEP 508 environment markers must not leak into the package name
        let content = r#"
importlib-metadata; python_version < "3.10"
requests==2.31.0; python_version >= "3.8"
typing-extensions>=4.0.0; python_version < "3.11"
"#;
        let pkgs = parse_requirements_txt(content, "requirements.txt");
        assert_eq!(pkgs.len(), 3);
        let im = pkgs
            .iter()
            .find(|p| p.name == "importlib-metadata")
            .expect("importlib-metadata missing");
        assert!(
            im.version.is_none(),
            "marker-only dep should be approximate"
        );
        let req = pkgs
            .iter()
            .find(|p| p.name == "requests")
            .expect("requests missing");
        assert_eq!(
            req.version.as_deref(),
            Some("2.31.0"),
            "exact version should survive marker strip"
        );
        let te = pkgs
            .iter()
            .find(|p| p.name == "typing-extensions")
            .expect("typing-extensions missing");
        assert!(te.version.is_none());
        // Marker fragments must not appear as names
        assert!(pkgs.iter().all(|p| !p.name.contains("python_version")));
    }

    #[test]
    fn test_parse_requirements_txt_extras() {
        let content = r#"
requests[socks]==2.31.0
uvicorn[standard]>=0.20.0
cryptography[ssh]
"#;
        let pkgs = parse_requirements_txt(content, "requirements.txt");
        // requests[socks]==2.31.0 → exact
        let r = pkgs.iter().find(|p| p.name == "requests").unwrap();
        assert_eq!(
            r.version.as_deref(),
            Some("2.31.0"),
            "extras should not break exact version"
        );
        // uvicorn[standard]>=0.20.0 → approximate
        let u = pkgs.iter().find(|p| p.name == "uvicorn").unwrap();
        assert!(u.version.is_none());
        // cryptography[ssh] → approximate (no version)
        let c = pkgs.iter().find(|p| p.name == "cryptography").unwrap();
        assert!(c.version.is_none());
    }

    #[test]
    fn test_parse_requirements_txt_direct_references() {
        let content = r#"
requests @ https://example.com/requests-2.31.0.tar.gz
fastapi[standard] @ file:///tmp/fastapi
"#;
        let pkgs = parse_requirements_txt(content, "requirements.txt");
        let requests = pkgs.iter().find(|p| p.name == "requests").unwrap();
        assert!(requests.version.is_none());
        let fastapi = pkgs.iter().find(|p| p.name == "fastapi").unwrap();
        assert!(fastapi.version.is_none());
        assert!(pkgs.iter().all(|p| !p.name.contains("://")));
    }

    #[test]
    fn test_parse_pipfile_lock() {
        let content = r#"{
            "default": {
                "requests": {"version": "==2.28.1"},
                "flask": {"version": "==2.3.0"}
            },
            "develop": {
                "pytest": {"version": "==7.0.0"}
            }
        }"#;
        let pkgs_no_dev = parse_pipfile_lock(content, "Pipfile.lock", false);
        assert_eq!(pkgs_no_dev.len(), 2);

        let pkgs_with_dev = parse_pipfile_lock(content, "Pipfile.lock", true);
        assert_eq!(pkgs_with_dev.len(), 3);
        assert!(pkgs_with_dev.iter().any(|p| p.name == "pytest"));
    }

    #[test]
    fn test_parse_yarn_lock_v1() {
        let content = r#"# yarn lockfile v1

lodash@^4.17.11, lodash@^4.17.21:
  version "4.17.21"
  resolved "https://..."
  integrity sha512-...

"@scope/pkg@^1.0.0":
  version "1.2.3"
  resolved "https://..."
"#;
        let pkgs = parse_yarn_lock(content, "yarn.lock");
        assert!(pkgs
            .iter()
            .any(|p| p.name == "lodash" && p.version.as_deref() == Some("4.17.21")));
        assert!(pkgs
            .iter()
            .any(|p| p.name == "@scope/pkg" && p.version.as_deref() == Some("1.2.3")));
    }

    #[test]
    fn test_parse_yarn_lock_v2_berry() {
        // Yarn 2+ (berry) uses YAML format with `version:` (no quotes)
        let content = r#"__metadata:
  version: 6
  cacheKey: 8

"lodash@npm:^4.17.21":
  version: 4.17.21
  resolution: "lodash@npm:4.17.21"
  checksum: abc123
  languageName: node
  linkType: hard

"@babel/core@npm:^7.0.0":
  version: 7.24.0
  resolution: "@babel/core@npm:7.24.0"
  languageName: node
  linkType: hard
"#;
        let pkgs = parse_yarn_lock(content, "yarn.lock");
        assert!(
            pkgs.iter()
                .any(|p| p.name == "lodash" && p.version.as_deref() == Some("4.17.21")),
            "lodash not found in {:?}",
            pkgs.iter()
                .map(|p| (&p.name, &p.version))
                .collect::<Vec<_>>()
        );
        assert!(
            pkgs.iter()
                .any(|p| p.name == "@babel/core" && p.version.as_deref() == Some("7.24.0")),
            "@babel/core not found"
        );
        // __metadata version: 6 must NOT appear as a package
        assert!(pkgs.iter().all(|p| p.name != "__metadata"));
    }

    #[test]
    fn test_uv_lock_skips_local_keeps_registry() {
        let content = r#"
[[package]]
name = "my-app"
version = "0.1.0"
source = { editable = "." }

[[package]]
name = "fastapi"
version = "0.115.0"
source = { registry = "https://pypi.org/simple" }

[[package]]
name = "local-lib"
version = "0.2.0"
source = { path = "../local-lib" }
"#;
        let pkgs = parse_uv_lock(content, "uv.lock");
        // Only fastapi (registry source) should be included
        assert_eq!(
            pkgs.len(),
            1,
            "got: {:?}",
            pkgs.iter().map(|p| &p.name).collect::<Vec<_>>()
        );
        assert_eq!(pkgs[0].name, "fastapi");
    }

    #[test]
    fn test_parse_pyproject_toml_only_deps() {
        // Metadata keys like name/version/requires-python must NOT appear as packages
        let content = r#"
[project]
name = "my-app"
version = "1.0.0"
requires-python = ">=3.11"
description = "A test app"
dependencies = [
    "fastapi>=0.129.0",
    "requests==2.31.0",
    "httpx==0.27.0", # pinned exact dep
    "localpkg @ https://example.com/localpkg-1.0.0.tar.gz", # direct reference
]

[project.optional-dependencies]
security = ["cryptography>=41.0.0"]
"#;
        let pkgs = parse_pyproject_toml(content, "pyproject.toml", false);
        let names: Vec<&str> = pkgs.iter().map(|p| p.name.as_str()).collect();
        assert!(
            !names.contains(&"my-app"),
            "project name should not be a dep"
        );
        assert!(!names.contains(&"name"), "metadata key should not be a dep");
        assert!(
            !names.contains(&"version"),
            "metadata key should not be a dep"
        );
        assert!(
            !names.contains(&"requires-python"),
            "metadata key should not be a dep"
        );
        assert!(names.contains(&"fastapi"), "fastapi should be a dep");
        assert!(names.contains(&"requests"), "requests should be a dep");
        assert!(
            names.contains(&"httpx"),
            "inline comments should not break deps"
        );
        assert!(
            names.contains(&"localpkg"),
            "direct references should keep package name"
        );
        let requests = pkgs.iter().find(|p| p.name == "requests").unwrap();
        assert_eq!(
            requests.version.as_deref(),
            Some("2.31.0"),
            "requests should have exact version"
        );
        let httpx = pkgs.iter().find(|p| p.name == "httpx").unwrap();
        assert_eq!(httpx.version.as_deref(), Some("0.27.0"));
        let localpkg = pkgs.iter().find(|p| p.name == "localpkg").unwrap();
        assert!(localpkg.version.is_none());
        assert!(
            names.contains(&"cryptography"),
            "optional dep should be included"
        );
    }

    #[test]
    fn test_parse_pyproject_toml_poetry_style() {
        let content = r#"
[tool.poetry.dependencies]
python = "^3.11"
fastapi = "^0.129.0"
requests = {version = "^2.31.0", extras = ["security"]}
"#;
        let pkgs = parse_pyproject_toml(content, "pyproject.toml", false);
        let names: Vec<&str> = pkgs.iter().map(|p| p.name.as_str()).collect();
        assert!(!names.contains(&"python"), "python should be skipped");
        assert!(names.contains(&"fastapi"));
        assert!(names.contains(&"requests"));
    }

    #[test]
    fn test_dev_requirements_filtered_by_include_dev() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("requirements.txt"), b"flask==2.3.0\n").unwrap();
        std::fs::write(dir.path().join("requirements-dev.txt"), b"pytest==7.0.0\n").unwrap();

        // Without include_dev: only requirements.txt
        let (sources_no_dev, _) = discover_packages(dir.path(), "python", &[], false);
        let all_names: Vec<&str> = sources_no_dev
            .iter()
            .flat_map(|s| s.packages.iter().map(|p| p.name.as_str()))
            .collect();
        assert!(all_names.contains(&"flask"));
        assert!(
            !all_names.contains(&"pytest"),
            "pytest should be excluded when include_dev=false"
        );

        // With include_dev: both files
        let (sources_with_dev, _) = discover_packages(dir.path(), "python", &[], true);
        let all_names_dev: Vec<&str> = sources_with_dev
            .iter()
            .flat_map(|s| s.packages.iter().map(|p| p.name.as_str()))
            .collect();
        assert!(all_names_dev.contains(&"flask"));
        assert!(
            all_names_dev.contains(&"pytest"),
            "pytest should be included when include_dev=true"
        );
    }

    #[test]
    fn test_pyproject_toml_excluded_when_lockfile_present() {
        let dir = tempfile::tempdir().unwrap();
        // Both poetry.lock and pyproject.toml in the same directory
        std::fs::write(
            dir.path().join("pyproject.toml"),
            b"[project]\nname = \"app\"\ndependencies = [\"requests>=2.0\"]\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("poetry.lock"),
            b"[[package]]\nname = \"requests\"\nversion = \"2.31.0\"\n",
        )
        .unwrap();

        let (sources, _) = discover_packages(dir.path(), "python", &[], false);
        let source_files: Vec<&str> = sources.iter().map(|s| s.lockfile_path.as_str()).collect();
        assert!(
            source_files.iter().any(|f| f.contains("poetry.lock")),
            "poetry.lock should be used"
        );
        assert!(
            !source_files.iter().any(|f| f.contains("pyproject.toml")),
            "pyproject.toml should be excluded when poetry.lock exists"
        );
    }

    #[test]
    fn test_exclude_patterns() {
        fn matches(pattern: &str, component: &str) -> bool {
            let pat = pattern.trim_end_matches("/**").trim_end_matches("/*");
            glob::Pattern::new(pat)
                .map(|p| p.matches(component))
                .unwrap_or(false)
        }
        assert!(matches(".venv", ".venv"));
        assert!(matches(".venv/**", ".venv"));
        assert!(matches("node_modules", "node_modules"));
        assert!(!matches(".venv", "src"));
        // Multi-wildcard pattern that the old hand-rolled impl silently ignored
        assert!(matches("vendor-*", "vendor-foo"));
        assert!(!matches("vendor-*", "src"));
    }

    #[test]
    fn test_extract_fix_filters_affected_package() {
        let pkg = Pkg::exact("target", "1.0.0", "npm", "package-lock.json");
        let affected = serde_json::json!([
            {
                "package": {"name": "other", "ecosystem": "npm"},
                "ranges": [{"events": [{"fixed": "9.9.9"}]}]
            },
            {
                "package": {"name": "target", "ecosystem": "npm"},
                "ranges": [{"events": [{"fixed": "2.0.0"}]}]
            }
        ]);
        assert_eq!(extract_fix(&affected, &pkg).as_deref(), Some("2.0.0"));

        let missing = Pkg::exact("missing", "1.0.0", "npm", "package-lock.json");
        assert_eq!(extract_fix(&affected, &missing), None);
    }

    // ── Integration tests (network) ───────────────────────────────────────────

    #[tokio::test]
    async fn test_invalid_project_type() {
        let r = AuditTool
            .execute(
                serde_json::json!({"project": "java", "packages": [{"name": "foo", "version": "1.0.0"}]}),
                &test_ctx(),
            )
            .await;
        assert!(r.is_error);
    }

    #[tokio::test]
    async fn test_ecosystem_mismatch_skipped() {
        let r = AuditTool
            .execute(
                serde_json::json!({"project": "rust", "packages": [{"name": "lodash", "version": "4.17.20", "ecosystem": "npm"}]}),
                &test_ctx(),
            )
            .await;
        assert!(r.is_error); // all packages skipped → error
    }

    #[tokio::test]
    async fn test_explicit_packages_require_ecosystem_when_auto() {
        // project=auto with no per-package ecosystem must error, not silently use npm
        let r = AuditTool
            .execute(
                serde_json::json!({"project": "auto", "packages": [{"name": "serde", "version": "1.0.0"}]}),
                &test_ctx(),
            )
            .await;
        assert!(r.is_error);
        assert!(
            r.content.contains("ecosystem"),
            "error should mention ecosystem"
        );
    }

    #[tokio::test]
    #[ignore = "requires live OSV.dev network access"]
    async fn test_rust_vuln_lookup() {
        let r = AuditTool
            .execute(
                serde_json::json!({"project": "rust", "packages": [{"name": "rand", "version": "0.3.22"}]}),
                &test_ctx(),
            )
            .await;
        assert!(!r.is_error, "error: {}", r.content);
        assert!(r.content.contains("Vulnerability Audit Report"));
    }

    #[tokio::test]
    async fn test_path_nonexistent() {
        let r = AuditTool
            .execute(
                serde_json::json!({"project": "auto", "path": "/nonexistent/path/xyz"}),
                &test_ctx(),
            )
            .await;
        assert!(r.is_error);
        assert!(r.content.contains("does not exist"));
    }

    #[tokio::test]
    #[ignore = "requires live OSV.dev network access"]
    async fn test_path_discovery_with_cargo_lock() {
        // Write a temporary Cargo.lock and test discovery
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let lockfile = dir.path().join("Cargo.lock");
        let mut f = std::fs::File::create(&lockfile).unwrap();
        writeln!(f, "[[package]]\nname = \"rand\"\nversion = \"0.3.22\"\n").unwrap();

        let r = AuditTool
            .execute(
                serde_json::json!({
                    "project": "rust",
                    "path": dir.path().to_str().unwrap()
                }),
                &test_ctx(),
            )
            .await;
        assert!(!r.is_error, "error: {}", r.content);
        assert!(r.content.contains("Vulnerability Audit Report"));
        assert!(r.content.contains("Cargo.lock"));
    }
}
