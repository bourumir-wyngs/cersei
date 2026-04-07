//! Browser tools built around chromiumoxide for local web app development.

use super::*;
use chromiumoxide::cdp::browser_protocol::log::EventEntryAdded;
use chromiumoxide::cdp::browser_protocol::network::{
    self as cdp_network, EventLoadingFailed, EventLoadingFinished, EventRequestWillBeSent,
    EventResponseReceived,
};
use chromiumoxide::cdp::js_protocol::runtime::{EventConsoleApiCalled, RemoteObject};
use chromiumoxide::{Browser, BrowserConfig, Element, Page};
use std::collections::HashSet;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tempfile::TempDir;
use url::Url;

type BrowserResult<T> = std::result::Result<T, String>;
const BROWSER_OPEN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
const MAX_BROWSER_LOG_ENTRIES: usize = 1000;
const MAX_BROWSER_NETWORK_ENTRIES: usize = 2000;
const DEFAULT_COMPUTED_STYLE_PROPERTIES: &[&str] = &[
    "font-size",
    "font-weight",
    "font-family",
    "line-height",
    "color",
    "display",
    "margin",
    "padding",
];

pub struct BrowserWindowTool;
pub struct BrowserNavigateTool;
pub struct BrowserConsoleTool;
pub struct BrowserDomTool;
pub struct BrowserClickTool;
pub struct BrowserInputTool;
pub struct BrowserNetworkTool;
pub struct BrowserCssTool;
pub struct BrowserStorageTool;

#[derive(Default)]
struct BrowserSession {
    launch_lock: tokio::sync::Mutex<()>,
    browser: tokio::sync::Mutex<Option<Browser>>,
    page: parking_lot::RwLock<Option<Page>>,
    log_entries: parking_lot::Mutex<Vec<BrowserLogEntry>>,
    network_entries: parking_lot::Mutex<Vec<NetworkEntry>>,
    next_network_sequence: std::sync::atomic::AtomicU64,
    active_request_ids: parking_lot::Mutex<HashSet<String>>,
    last_network_completion_timestamp: parking_lot::Mutex<Option<f64>>,
    tasks: parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    profile_dir: parking_lot::Mutex<Option<TempDir>>,
}

#[derive(Debug, Clone, Serialize)]
struct BrowserLogEntry {
    source: String,
    level: String,
    text: String,
    timestamp: f64,
    url: Option<String>,
    line_number: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
struct NetworkEntry {
    request_id: String,
    sequence: u64,
    url: String,
    method: String,
    resource_type: Option<String>,
    status: Option<i64>,
    status_text: Option<String>,
    mime_type: Option<String>,
    response_headers: Option<Value>,
    error_text: Option<String>,
    timestamp: f64,
    encoded_data_length: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct BrowserWindowInput {
    action: Option<BrowserWindowAction>,
    url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BrowserNavigateInput {
    url: String,
}

#[derive(Debug, Deserialize, Default)]
struct BrowserConsoleInput {
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct BrowserClickInput {
    selector: String,
}

#[derive(Debug, Deserialize)]
struct BrowserInputInput {
    selector: String,
    text: Option<String>,
    key: Option<String>,
    clear_first: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct BrowserDomInput {
    selector: String,
    field: Option<DomField>,
    property: Option<String>,
    properties: Option<Vec<String>>,
    all: Option<bool>,
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct BrowserNetworkInput {
    url_filter: Option<String>,
    resource_type: Option<String>,
    status_filter: Option<String>,
    limit: Option<usize>,
    failed_only: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct BrowserCssInput {
    selector: String,
}


#[derive(Debug, Deserialize)]
struct BrowserStorageInput {
    storage: StorageType,
    name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum StorageType {
    Cookies,
    LocalStorage,
    SessionStorage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum DomField {
    InnerText,
    InnerHtml,
    OuterHtml,
    Attributes,
    Value,
    Property,
    ComputedStyle,
}

impl Default for DomField {
    fn default() -> Self {
        Self::InnerText
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum BrowserWindowAction {
    Open,
    Status,
    Close,
}

impl Default for BrowserWindowAction {
    fn default() -> Self {
        Self::Status
    }
}

static BROWSER_SESSION_REGISTRY: once_cell::sync::Lazy<dashmap::DashMap<String, Arc<BrowserSession>>> =
    once_cell::sync::Lazy::new(dashmap::DashMap::new);

fn trim_vec_to_limit<T>(items: &mut Vec<T>, max_entries: usize) {
    if items.len() > max_entries {
        let overflow = items.len() - max_entries;
        items.drain(0..overflow);
    }
}

fn push_log_entry(session: &BrowserSession, entry: BrowserLogEntry) {
    let mut entries = session.log_entries.lock();
    entries.push(entry);
    trim_vec_to_limit(&mut entries, MAX_BROWSER_LOG_ENTRIES);
}

fn push_network_entry(session: &BrowserSession, entry: NetworkEntry) {
    let mut entries = session.network_entries.lock();
    entries.push(entry);
    trim_vec_to_limit(&mut entries, MAX_BROWSER_NETWORK_ENTRIES);
}

fn mark_network_completion(session: &BrowserSession, request_id: &str, timestamp: f64) {
    session.active_request_ids.lock().remove(request_id);
    *session.last_network_completion_timestamp.lock() = Some(timestamp);
}

fn clear_network_tracking(session: &BrowserSession) {
    session.network_entries.lock().clear();
    session.active_request_ids.lock().clear();
    *session.last_network_completion_timestamp.lock() = None;
}

fn should_track_request_as_active(url: &str) -> bool {
    let trimmed = url.trim();
    !trimmed.is_empty() && !trimmed.starts_with("data:")
}

fn browser_session(session_id: &str) -> Arc<BrowserSession> {
    BROWSER_SESSION_REGISTRY
        .entry(session_id.to_string())
        .or_insert_with(|| Arc::new(BrowserSession::default()))
        .clone()
}

#[async_trait]
impl Tool for BrowserWindowTool {
    fn name(&self) -> &str {
        "BrowserWindow"
    }

    fn description(&self) -> &str {
        "Manage the retained visible browser window for this agent session. Use action=open to launch or reuse it, action=status to inspect it, and action=close to close it. Other browser tools use this same retained window. Only localhost URLs are allowed."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Execute
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Web
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["open", "status", "close"],
                    "description": "Browser window action. Defaults to status."
                },
                "url": {
                    "type": "string",
                    "description": "Optional localhost URL to open when action is open. If omitted, BrowserWindow uses browser.url from tools.yaml when configured, otherwise about:blank."
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let input: BrowserWindowInput = match serde_json::from_value(input) {
            Ok(value) => value,
            Err(err) => return ToolResult::error(format!("Invalid input: {err}")),
        };
        let browser_config = browser_config_from_context(ctx);

        match input.action.unwrap_or_default() {
            BrowserWindowAction::Open => {
                let requested_url = input.url.as_deref();
                match tokio::time::timeout(
                    BROWSER_OPEN_TIMEOUT,
                    ensure_browser_page(ctx, &browser_config, requested_url),
                )
                .await
                {
                    Ok(Ok((session, page))) => success_json(
                        browser_window_open_value(&session, &page, &browser_config, requested_url)
                            .await,
                    ),
                    Ok(Err(err)) => ToolResult::error(err),
                    Err(_) => ToolResult::error(
                        "BrowserWindow action=open timed out while opening the visible browser window. Try again."
                    ),
                }
            }
            BrowserWindowAction::Status => match current_browser_page(ctx).await {
                Some((session, page)) => {
                    success_json(browser_status_value(&session, &page, &browser_config).await)
                }
                None => success_json(browser_closed_value(&browser_config)),
            },
            BrowserWindowAction::Close => match close_browser_session(&ctx.session_id).await {
                Ok(message) => ToolResult::success(message),
                Err(err) => ToolResult::error(err),
            },
        }
    }
}

#[async_trait]
impl Tool for BrowserNavigateTool {
    fn name(&self) -> &str {
        "BrowserNavigate"
    }

    fn description(&self) -> &str {
        "Navigate the current retained browser window to a localhost URL."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Execute
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Web
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "Localhost URL to open in the retained browser window. about:blank is also allowed."
                }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let input: BrowserNavigateInput = match serde_json::from_value(input) {
            Ok(value) => value,
            Err(err) => return ToolResult::error(format!("Invalid input: {err}")),
        };

        let (session, page) = match require_browser_page(ctx).await {
            Ok(state) => state,
            Err(err) => return ToolResult::error(err),
        };
        let browser_config = browser_config_from_context(ctx);

        match navigate_page(&page, &input.url).await {
            Ok(()) => success_json(browser_status_value(&session, &page, &browser_config).await),
            Err(err) => ToolResult::error(err),
        }
    }
}


#[async_trait]
impl Tool for BrowserConsoleTool {
    fn name(&self) -> &str {
        "BrowserConsole"
    }

    fn description(&self) -> &str {
        "Return collected console and log entries from the retained browser window in this session."
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
                "limit": {
                    "type": "integer",
                    "description": "Optional number of most recent entries to return. If omitted, return all collected entries."
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let input: BrowserConsoleInput = match serde_json::from_value(input) {
            Ok(value) => value,
            Err(err) => return ToolResult::error(format!("Invalid input: {err}")),
        };

        let (session, _) = match require_browser_page(ctx).await {
            Ok(state) => state,
            Err(err) => return ToolResult::error(err),
        };

        let mut entries = session.log_entries.lock().clone();
        if let Some(limit) = input.limit {
            if entries.len() > limit {
                let start = entries.len() - limit;
                entries = entries.split_off(start);
            }
        }

        success_json(serde_json::json!({
            "entries": entries,
            "total_entries": session.log_entries.lock().len(),
        }))
    }
}

#[async_trait]
impl Tool for BrowserDomTool {
    fn name(&self) -> &str {
        "BrowserDom"
    }

    fn description(&self) -> &str {
        concat!(
            "Inspect the current page DOM without interacting with the page. ",
            "Use this after BrowserWindow action=open or BrowserNavigate when you need to read text, HTML, ",
            "attributes, form values, or a specific DOM property for an element like ",
            "`main h1`, `[aria-live='polite']`, or `form input[name='email']`. ",
            "By default it reads the first matching element and returns inner_text. ",
            "Set field=computed_style to read computed CSS such as font-size, color, ",
            "font-weight, display, margin, or padding. ",
            "Set all=true to inspect every matching element. ",
            "Set field=property and provide property to read a named DOM property such as ",
            "checked, disabled, value, textContent, or ariaLabel."
        )
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
                "selector": {
                    "type": "string",
                    "description": "Element to read from, for example `main h1`, `.toast-error`, or `table tbody tr`."
                },
                "field": {
                    "type": "string",
                    "enum": ["inner_text", "inner_html", "outer_html", "attributes", "value", "property", "computed_style"],
                    "description": "Which DOM field to return. Defaults to inner_text."
                },
                "property": {
                    "type": "string",
                    "description": "Required when field is property. The DOM property name to read. If field is computed_style, this can also be used for one CSS property like `font-size` or `color`."
                },
                "properties": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional CSS property names to read when field is computed_style, for example [`font-size`, `color`, `display`]. If omitted, BrowserDom returns a useful default set."
                },
                "all": {
                    "type": "boolean",
                    "description": "If true, return the selected field for every matching element."
                },
                "limit": {
                    "type": "integer",
                    "description": "Optional maximum number of matching elements to return when all is true."
                }
            },
            "required": ["selector"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let input: BrowserDomInput = match serde_json::from_value(input) {
            Ok(value) => value,
            Err(err) => return ToolResult::error(format!("Invalid input: {err}")),
        };

        let (_, page) = match require_browser_page(ctx).await {
            Ok(state) => state,
            Err(err) => return ToolResult::error(err),
        };

        let field = input.field.unwrap_or_default();
        let all = input.all.unwrap_or(false);
        if field == DomField::ComputedStyle {
            match computed_style_value(
                &page,
                &input.selector,
                input.property.as_deref(),
                input.properties.as_deref(),
                all,
                input.limit,
            )
            .await
            {
                Ok(value) => {
                    return success_json(serde_json::json!({
                        "selector": input.selector,
                        "field": field,
                        "value": value,
                    }));
                }
                Err(err) => return ToolResult::error(err),
            }
        }

        if all {
            let elements = match page.find_elements(input.selector.clone()).await {
                Ok(elements) => elements,
                Err(err) => return ToolResult::error(format!("Failed to query DOM: {err}")),
            };

            let mut items = Vec::new();
            for element in elements.into_iter().take(input.limit.unwrap_or(usize::MAX)) {
                match dom_field_value(&element, field, input.property.as_deref()).await {
                    Ok(value) => items.push(value),
                    Err(err) => return ToolResult::error(err),
                }
            }

            success_json(serde_json::json!({
                "selector": input.selector,
                "field": field,
                "matches": items.len(),
                "items": items,
            }))
        } else {
            let element = match page.find_element(input.selector.clone()).await {
                Ok(element) => element,
                Err(err) => return ToolResult::error(format!("Failed to query DOM: {err}")),
            };

            match dom_field_value(&element, field, input.property.as_deref()).await {
                Ok(value) => success_json(serde_json::json!({
                    "selector": input.selector,
                    "field": field,
                    "value": value,
                })),
                Err(err) => ToolResult::error(err),
            }
        }
    }
}

#[async_trait]
impl Tool for BrowserClickTool {
    fn name(&self) -> &str {
        "BrowserClick"
    }

    fn description(&self) -> &str {
        "Click an element in the retained browser window, for example `button[type='submit']` or `nav a[href='/settings']`."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Dangerous
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Web
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "selector": {
                    "type": "string",
                    "description": "Element to click, for example `button[type='submit']` or `[data-testid='save']`."
                }
            },
            "required": ["selector"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let input: BrowserClickInput = match serde_json::from_value(input) {
            Ok(value) => value,
            Err(err) => return ToolResult::error(format!("Invalid input: {err}")),
        };

        let (session, page) = match require_browser_page(ctx).await {
            Ok(state) => state,
            Err(err) => return ToolResult::error(err),
        };
        let browser_config = browser_config_from_context(ctx);

        let element = match page.find_element(input.selector).await {
            Ok(element) => element,
            Err(err) => return ToolResult::error(format!("Failed to query DOM: {err}")),
        };

        match element.click().await {
            Ok(_) => success_json(browser_status_value(&session, &page, &browser_config).await),
            Err(err) => ToolResult::error(format!("Browser click failed: {err}")),
        }
    }
}

#[async_trait]
impl Tool for BrowserInputTool {
    fn name(&self) -> &str {
        "BrowserInput"
    }

    fn description(&self) -> &str {
        "Send keyboard input to an element in the retained browser window. This can type text, press a key, or do both in order, for example type into `input[name='email']` or press Enter on `input[type='search']`."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Dangerous
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Web
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "selector": {
                    "type": "string",
                    "description": "Element to focus before sending keyboard input, for example `input[name='email']`, `textarea[name='message']`, or `input[type='search']`."
                },
                "text": {
                    "type": "string",
                    "description": "Optional text to type after focusing the element. Provide text, key, or both."
                },
                "key": {
                    "type": "string",
                    "description": "Optional key to press after typing, for example Enter, Escape, ArrowDown, or Tab. Provide text, key, or both."
                },
                "clear_first": {
                    "type": "boolean",
                    "description": "If true, clear the element's value before typing text."
                }
            },
            "required": ["selector"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let input: BrowserInputInput = match serde_json::from_value(input) {
            Ok(value) => value,
            Err(err) => return ToolResult::error(format!("Invalid input: {err}")),
        };
        if let Err(err) = validate_browser_input(&input) {
            return ToolResult::error(err);
        }

        let (session, page) = match require_browser_page(ctx).await {
            Ok(state) => state,
            Err(err) => return ToolResult::error(err),
        };
        let browser_config = browser_config_from_context(ctx);

        if input.clear_first.unwrap_or(false) {
            if let Err(err) = clear_element_value(&page, &input.selector).await {
                return ToolResult::error(err);
            }
        }

        let element = match page.find_element(input.selector).await {
            Ok(element) => element,
            Err(err) => return ToolResult::error(format!("Failed to query DOM: {err}")),
        };

        if let Err(err) = element.click().await {
            return ToolResult::error(format!(
                "Browser input failed while focusing the element: {err}"
            ));
        }

        if let Some(text) = input.text.as_deref().filter(|text| !text.is_empty()) {
            if let Err(err) = element.type_str(text).await {
                return ToolResult::error(format!("Browser input failed while typing text: {err}"));
            }
        }

        if let Some(key) = input.key.as_deref().filter(|key| !key.trim().is_empty()) {
            if let Err(err) = element.press_key(key).await {
                return ToolResult::error(format!("Browser input failed while pressing a key: {err}"));
            }
        }

        success_json(browser_status_value(&session, &page, &browser_config).await)
    }
}

// ─── BrowserNetworkTool ─────────────────────────────────────────────────────

#[async_trait]
impl Tool for BrowserNetworkTool {
    fn name(&self) -> &str {
        "BrowserNetwork"
    }

    fn description(&self) -> &str {
        concat!(
            "Inspect collected network requests and responses from the retained browser window. ",
            "Shows URL, method, status code, headers, resource type, errors, and timing. ",
            "Filter by URL substring, resource type (Document, Stylesheet, Script, XHR, Fetch, Image, etc.), ",
            "status code range (e.g. '4xx', '5xx', '200'), or failed requests only."
        )
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
                "url_filter": {
                    "type": "string",
                    "description": "Optional substring to filter request URLs, e.g. '/api/' or '.css'."
                },
                "resource_type": {
                    "type": "string",
                    "description": "Optional resource type filter: Document, Stylesheet, Script, XHR, Fetch, Image, Font, Media, WebSocket, Other."
                },
                "status_filter": {
                    "type": "string",
                    "description": "Optional status code filter: an exact code like '404', or a range like '4xx' or '5xx'."
                },
                "failed_only": {
                    "type": "boolean",
                    "description": "If true, only return requests that failed (network error, not HTTP error codes)."
                },
                "limit": {
                    "type": "integer",
                    "description": "Optional maximum number of entries to return. Most recent entries are returned first."
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let input: BrowserNetworkInput = match serde_json::from_value(input) {
            Ok(value) => value,
            Err(err) => return ToolResult::error(format!("Invalid input: {err}")),
        };

        let (session, _) = match require_browser_page(ctx).await {
            Ok(state) => state,
            Err(err) => return ToolResult::error(err),
        };

        let all_entries = session.network_entries.lock().clone();
        let mut entries: Vec<&NetworkEntry> = all_entries.iter().collect();

        // Apply filters
        if let Some(ref url_filter) = input.url_filter {
            entries.retain(|e| e.url.contains(url_filter.as_str()));
        }
        if let Some(ref resource_type) = input.resource_type {
            let rt = resource_type.to_ascii_lowercase();
            entries.retain(|e| {
                e.resource_type
                    .as_ref()
                    .map(|t| t.to_ascii_lowercase() == rt)
                    .unwrap_or(false)
            });
        }
        if let Some(ref status_filter) = input.status_filter {
            entries.retain(|e| matches_status_filter(e.status, status_filter));
        }
        if input.failed_only.unwrap_or(false) {
            entries.retain(|e| e.error_text.is_some());
        }

        // Most recent first
        entries.reverse();

        let total = entries.len();
        if let Some(limit) = input.limit {
            entries.truncate(limit);
        }

        success_json(serde_json::json!({
            "entries": entries.into_iter().cloned().collect::<Vec<_>>(),
            "returned": std::cmp::min(total, input.limit.unwrap_or(total)),
            "total_matching": total,
            "total_collected": all_entries.len(),
        }))
    }
}

// ─── BrowserCssTool ─────────────────────────────────────────────────────────

#[async_trait]
impl Tool for BrowserCssTool {
    fn name(&self) -> &str {
        "BrowserCss"
    }

    fn description(&self) -> &str {
        concat!(
            "Inspect the CSS rules that match an element, including the stylesheet source ",
            "(file URL and rule text). This reveals which CSS rules apply and where they come from, ",
            "going beyond computed styles to show rule provenance. ",
            "Returns matched rules sorted by specificity (most specific last)."
        )
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
                "selector": {
                    "type": "string",
                    "description": "CSS selector for the element to inspect, e.g. `.box-content label` or `#main h1`."
                }
            },
            "required": ["selector"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let input: BrowserCssInput = match serde_json::from_value(input) {
            Ok(value) => value,
            Err(err) => return ToolResult::error(format!("Invalid input: {err}")),
        };

        let (_, page) = match require_browser_page(ctx).await {
            Ok(state) => state,
            Err(err) => return ToolResult::error(err),
        };

        match css_matched_rules(&page, &input.selector).await {
            Ok(value) => success_json(serde_json::json!({
                "selector": input.selector,
                "matched_rules": value,
            })),
            Err(err) => ToolResult::error(err),
        }
    }
}

// ─── BrowserStorageTool ─────────────────────────────────────────────────────

#[async_trait]
impl Tool for BrowserStorageTool {
    fn name(&self) -> &str {
        "BrowserStorage"
    }

    fn description(&self) -> &str {
        concat!(
            "Inspect browser storage for the current page: cookies, localStorage, or sessionStorage. ",
            "For cookies, returns all cookies matching the current URL. ",
            "For localStorage/sessionStorage, returns all key-value pairs. ",
            "Optionally filter by name to return a single entry."
        )
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
                "storage": {
                    "type": "string",
                    "enum": ["cookies", "local_storage", "session_storage"],
                    "description": "Which storage to inspect."
                },
                "name": {
                    "type": "string",
                    "description": "Optional: filter by cookie name or storage key."
                }
            },
            "required": ["storage"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let input: BrowserStorageInput = match serde_json::from_value(input) {
            Ok(value) => value,
            Err(err) => return ToolResult::error(format!("Invalid input: {err}")),
        };

        let (_, page) = match require_browser_page(ctx).await {
            Ok(state) => state,
            Err(err) => return ToolResult::error(err),
        };

        match input.storage {
            StorageType::Cookies => {
                match page.get_cookies().await {
                    Ok(cookies) => {
                        let items: Vec<Value> = cookies
                            .iter()
                            .filter(|c| {
                                input.name.as_ref().map(|n| c.name == *n).unwrap_or(true)
                            })
                            .map(|c| {
                                serde_json::json!({
                                    "name": c.name,
                                    "value": c.value,
                                    "domain": c.domain,
                                    "path": c.path,
                                    "expires": c.expires,
                                    "http_only": c.http_only,
                                    "secure": c.secure,
                                    "session": c.session,
                                    "same_site": c.same_site.as_ref().map(|s| s.as_ref().to_string()),
                                })
                            })
                            .collect();
                        success_json(serde_json::json!({
                            "storage": "cookies",
                            "count": items.len(),
                            "items": items,
                        }))
                    }
                    Err(err) => ToolResult::error(format!("Failed to get cookies: {err}")),
                }
            }
            StorageType::LocalStorage | StorageType::SessionStorage => {
                let storage_obj = match input.storage {
                    StorageType::LocalStorage => "localStorage",
                    StorageType::SessionStorage => "sessionStorage",
                    _ => unreachable!(),
                };

                let script = if let Some(ref name) = input.name {
                    let key = serde_json::to_string(name)
                        .map_err(|e| format!("Failed to encode key: {e}"))
                        .unwrap_or_default();
                    format!(
                        r#"() => {{
                            const val = {storage_obj}.getItem({key});
                            return val === null ? null : {{ key: {key}, value: val }};
                        }}"#
                    )
                } else {
                    format!(
                        r#"() => {{
                            const items = [];
                            for (let i = 0; i < {storage_obj}.length; i++) {{
                                const key = {storage_obj}.key(i);
                                items.push({{ key, value: {storage_obj}.getItem(key) }});
                            }}
                            return items;
                        }}"#
                    )
                };

                match page.evaluate(script.as_str()).await {
                    Ok(result) => match result.into_value::<Value>() {
                        Ok(value) => success_json(serde_json::json!({
                            "storage": storage_obj,
                            "data": value,
                        })),
                        Err(err) => {
                            ToolResult::error(format!("Failed to decode storage result: {err}"))
                        }
                    },
                    Err(err) => ToolResult::error(format!("Failed to read {storage_obj}: {err}")),
                }
            }
        }
    }
}

// ─── Helper functions for new tools ─────────────────────────────────────────

fn matches_status_filter(status: Option<i64>, filter: &str) -> bool {
    let filter = filter.trim();
    if filter.ends_with("xx") || filter.ends_with("XX") {
        if let Ok(prefix) = filter[..1].parse::<i64>() {
            return status.map(|s| s / 100 == prefix).unwrap_or(false);
        }
    }
    if let Ok(code) = filter.parse::<i64>() {
        return status == Some(code);
    }
    false
}

async fn css_matched_rules(page: &Page, selector: &str) -> BrowserResult<Value> {
    let selector_json = serde_json::to_string(selector)
        .map_err(|err| format!("Failed to encode CSS selector: {err}"))?;
    let script = format!(
        r#"() => {{
            const el = document.querySelector({selector_json});
            if (!el) throw new Error("Element not found for selector: " + {selector_json});

            const rules = [];
            try {{
                for (const sheet of document.styleSheets) {{
                    let href = null;
                    try {{ href = sheet.href || (sheet.ownerNode && sheet.ownerNode.tagName === 'STYLE' ? 'inline:<style>' : null); }} catch(_) {{}}
                    let cssRules;
                    try {{ cssRules = sheet.cssRules; }} catch(_) {{ continue; }}
                    for (let i = 0; i < cssRules.length; i++) {{
                        const rule = cssRules[i];
                        if (rule.type !== CSSRule.STYLE_RULE) continue;
                        try {{
                            if (!el.matches(rule.selectorText)) continue;
                        }} catch(_) {{ continue; }}
                        const props = {{}};
                        for (let j = 0; j < rule.style.length; j++) {{
                            const prop = rule.style[j];
                            props[prop] = rule.style.getPropertyValue(prop);
                        }}
                        rules.push({{
                            selector: rule.selectorText,
                            stylesheet: href,
                            properties: props,
                        }});
                    }}
                }}
            }} catch(e) {{
                // Some cross-origin stylesheets may throw; we skip them
            }}

            // Also include inline styles
            const inlineStyle = el.getAttribute('style');
            if (inlineStyle) {{
                const props = {{}};
                for (let j = 0; j < el.style.length; j++) {{
                    const prop = el.style[j];
                    props[prop] = el.style.getPropertyValue(prop);
                }}
                rules.push({{
                    selector: "(inline style)",
                    stylesheet: null,
                    properties: props,
                }});
            }}

            return rules;
        }}"#
    );

    page.evaluate(script.as_str())
        .await
        .map_err(|err| format!("Failed to get matched CSS rules: {err}"))?
        .into_value::<Value>()
        .map_err(|err| format!("Failed to decode CSS rules result: {err}"))
}

async fn ensure_browser_page(
    ctx: &ToolContext,
    browser_config: &BrowserToolConfig,
    requested_url: Option<&str>,
) -> BrowserResult<(Arc<BrowserSession>, Page)> {
    let session = browser_session(&ctx.session_id);
    let guard = session.launch_lock.lock().await;

    let existing_page = session.page.read().clone();
    if let Some(page) = existing_page {
        if let Some(url) = requested_url {
            navigate_page(&page, url).await?;
        }
        drop(guard);
        return Ok((session, page));
    }

    let profile_dir =
        TempDir::new().map_err(|err| format!("Failed to create browser profile directory: {err}"))?;
    let config = BrowserConfig::builder()
        .with_head()
        .window_size(browser_config.window.width, browser_config.window.height)
        .viewport(None)
        .user_data_dir(profile_dir.path())
        .build()
        .map_err(|err| format!("Failed to build browser config: {err}"))?;

    let (mut browser, mut handler) =
        Browser::launch(config).await.map_err(|err| format!("Failed to launch Chromium: {err}"))?;

    // chromiumoxide commands only make progress while the handler stream is polled.
    let handler_task = tokio::spawn(async move {
        while let Some(event) = handler.next().await {
            if let Err(err) = event {
                tracing::debug!(error = ?err, "browser handler event failed");
            }
        }
    });

    let page = match browser.new_page("about:blank").await {
        Ok(page) => page,
        Err(err) => {
            handler_task.abort();
            let _ = browser.close().await;
            let _ = browser.wait().await;
            return Err(format!("Failed to create browser page: {err}"));
        }
    };
    if let Err(err) = page.enable_log().await {
        handler_task.abort();
        let _ = browser.close().await;
        let _ = browser.wait().await;
        return Err(format!("Failed to enable browser log collection: {err}"));
    }

    // Enable the network domain for request/response tracking.
    if let Err(err) = page
        .execute(cdp_network::EnableParams::default())
        .await
    {
        handler_task.abort();
        let _ = browser.close().await;
        let _ = browser.wait().await;
        return Err(format!("Failed to enable network tracking: {err}"));
    }

    let mut tasks = vec![handler_task];
    match spawn_page_listener_tasks(page.clone(), session.clone()).await {
        Ok(listener_tasks) => tasks.extend(listener_tasks),
        Err(err) => {
            for task in tasks.drain(..) {
                task.abort();
            }
            let _ = browser.close().await;
            let _ = browser.wait().await;
            return Err(err);
        }
    }
    session.log_entries.lock().clear();
    clear_network_tracking(&session);
    *session.browser.lock().await = Some(browser);
    *session.page.write() = Some(page.clone());
    *session.profile_dir.lock() = Some(profile_dir);
    *session.tasks.lock() = tasks;

    if let Some(url) = requested_url.or(browser_config.url.as_deref()) {
        navigate_page(&page, url).await?;
    }

    drop(guard);
    Ok((session, page))
}

async fn require_browser_page(ctx: &ToolContext) -> BrowserResult<(Arc<BrowserSession>, Page)> {
    match current_browser_page(ctx).await {
        Some(page) => Ok(page),
        None => Err(
            "No browser window is open for this session. Open window first with BrowserWindow action=open.".into()
        ),
    }
}

async fn current_browser_page(ctx: &ToolContext) -> Option<(Arc<BrowserSession>, Page)> {
    let session = browser_session(&ctx.session_id);
    let page = session.page.read().clone()?;
    if is_page_alive(&page).await {
        Some((session, page))
    } else {
        clear_stale_browser_session(&ctx.session_id).await;
        None
    }
}

fn browser_config_from_context(ctx: &ToolContext) -> BrowserToolConfig {
    ctx.extensions
        .get::<ToolsConfig>()
        .and_then(|config| config.browser.clone())
        .unwrap_or_default()
}

async fn is_page_alive(page: &Page) -> bool {
    page.url().await.is_ok()
}

async fn clear_stale_browser_session(session_id: &str) {
    if let Some(session) = BROWSER_SESSION_REGISTRY.get(session_id) {
        abort_session_tasks(session.value());
        *session.page.write() = None;
        session.log_entries.lock().clear();
        clear_network_tracking(session.value());
        session.profile_dir.lock().take();
        let _ = session.browser.lock().await.take();
    }
}

async fn close_browser_session(session_id: &str) -> BrowserResult<String> {
    let Some((_, session)) = BROWSER_SESSION_REGISTRY.remove(session_id) else {
        return Ok("No browser session was open.".into());
    };

    let mut browser = session.browser.lock().await.take();
    if let Some(browser) = browser.as_mut() {
        let _ = browser.close().await;
        let _ = browser.wait().await;
    }

    abort_session_tasks(&session);
    *session.page.write() = None;
    session.log_entries.lock().clear();
    clear_network_tracking(&session);
    session.profile_dir.lock().take();

    Ok("Browser session closed.".into())
}

fn abort_session_tasks(session: &BrowserSession) {
    let mut tasks = session.tasks.lock();
    for task in tasks.drain(..) {
        task.abort();
    }
}

async fn spawn_page_listener_tasks(
    page: Page,
    session: Arc<BrowserSession>,
) -> BrowserResult<Vec<tokio::task::JoinHandle<()>>> {
    let mut console_events = page
        .event_listener::<EventConsoleApiCalled>()
        .await
        .map_err(|err| format!("Failed to subscribe to console events: {err}"))?;
    let console_session = session.clone();
    let console_task = tokio::spawn(async move {
        while let Some(event) = console_events.next().await {
            push_log_entry(console_session.as_ref(), console_event_to_entry(event.as_ref()));
        }
    });

    let mut log_events = page
        .event_listener::<EventEntryAdded>()
        .await
        .map_err(|err| format!("Failed to subscribe to log events: {err}"))?;
    let log_session = session.clone();
    let log_task = tokio::spawn(async move {
        while let Some(event) = log_events.next().await {
            push_log_entry(log_session.as_ref(), log_event_to_entry(event.as_ref()));
        }
    });

    // Network event listeners
    let mut request_events = page
        .event_listener::<EventRequestWillBeSent>()
        .await
        .map_err(|err| format!("Failed to subscribe to network request events: {err}"))?;
    let request_session = session.clone();
    let request_task = tokio::spawn(async move {
        while let Some(event) = request_events.next().await {
            let request_id = event.request_id.inner().clone();
            if should_track_request_as_active(&event.request.url) {
                request_session
                    .active_request_ids
                    .lock()
                    .insert(request_id.clone());
            }
            let entry = NetworkEntry {
                request_id,
                sequence: request_session
                    .next_network_sequence
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                url: event.request.url.clone(),
                method: event.request.method.clone(),
                resource_type: event.r#type.as_ref().map(|t| t.as_ref().to_string()),
                status: None,
                status_text: None,
                mime_type: None,
                response_headers: None,
                error_text: None,
                timestamp: *event.timestamp.inner(),
                encoded_data_length: None,
            };
            push_network_entry(request_session.as_ref(), entry);
        }
    });

    let mut response_events = page
        .event_listener::<EventResponseReceived>()
        .await
        .map_err(|err| format!("Failed to subscribe to network response events: {err}"))?;
    let response_session = session.clone();
    let response_task = tokio::spawn(async move {
        while let Some(event) = response_events.next().await {
            let req_id = event.request_id.inner().clone();
            let event_timestamp = *event.timestamp.inner();
            let redirect_url = event.response.url.clone();
            let mut entries = response_session.network_entries.lock();
            if let Some(entry) = entries
                .iter_mut()
                .rev()
                .find(|e| e.request_id == req_id && e.status.is_none() && e.error_text.is_none())
            {
                entry.url = redirect_url;
                entry.status = Some(event.response.status);
                entry.status_text = Some(event.response.status_text.clone());
                entry.mime_type = Some(event.response.mime_type.clone());
                entry.response_headers = Some(event.response.headers.inner().clone());
                entry.encoded_data_length = Some(event.response.encoded_data_length);
                entry.timestamp = entry.timestamp.max(event_timestamp);
            }
            drop(entries);
        }
    });

    let mut finished_events = page
        .event_listener::<EventLoadingFinished>()
        .await
        .map_err(|err| format!("Failed to subscribe to network completion events: {err}"))?;
    let finished_session = session.clone();
    let finished_task = tokio::spawn(async move {
        while let Some(event) = finished_events.next().await {
            let req_id = event.request_id.inner().clone();
            let event_timestamp = *event.timestamp.inner();
            let mut entries = finished_session.network_entries.lock();
            if let Some(entry) = entries
                .iter_mut()
                .rev()
                .find(|e| e.request_id == req_id && e.error_text.is_none())
            {
                entry.encoded_data_length = Some(event.encoded_data_length);
                entry.timestamp = entry.timestamp.max(event_timestamp);
            }
            drop(entries);
            mark_network_completion(finished_session.as_ref(), &req_id, event_timestamp);
        }
    });

    let mut failed_events = page
        .event_listener::<EventLoadingFailed>()
        .await
        .map_err(|err| format!("Failed to subscribe to network failure events: {err}"))?;
    let failed_session = session;
    let failed_task = tokio::spawn(async move {
        while let Some(event) = failed_events.next().await {
            let req_id = event.request_id.inner().clone();
            let event_timestamp = *event.timestamp.inner();
            let mut entries = failed_session.network_entries.lock();
            if let Some(entry) = entries
                .iter_mut()
                .rev()
                .find(|e| e.request_id == req_id && e.error_text.is_none())
            {
                entry.error_text = Some(event.error_text.clone());
                entry.timestamp = entry.timestamp.max(event_timestamp);
            }
            drop(entries);
            mark_network_completion(failed_session.as_ref(), &req_id, event_timestamp);
        }
    });

    Ok(vec![console_task, log_task, request_task, response_task, finished_task, failed_task])
}

async fn browser_status_value(
    session: &BrowserSession,
    page: &Page,
    browser_config: &BrowserToolConfig,
) -> Value {
    let url = page.url().await.ok().flatten();
    let title = evaluate_string(page, "document.title").await.ok();
    let ready_state = evaluate_string(page, "document.readyState").await.ok();

    serde_json::json!({
        "browser_open": true,
        "url": url,
        "title": title,
        "ready_state": ready_state,
        "console_entry_count": session.log_entries.lock().len(),
        "configured_url": browser_config.url,
        "notes": browser_config.notes,
    })
}

async fn browser_window_open_value(
    session: &BrowserSession,
    page: &Page,
    browser_config: &BrowserToolConfig,
    requested_url: Option<&str>,
) -> Value {
        let url = page.url().await.ok().flatten();
        serde_json::json!({
            "browser_open": true,
            "message": "Visible browser window is open and retained for this session. Use BrowserNavigate, BrowserDom, BrowserConsole, BrowserClick, or BrowserInput on this same window.",
            "url": url,
            "requested_url": requested_url,
            "configured_url": browser_config.url,
        "window": {
            "width": browser_config.window.width,
            "height": browser_config.window.height,
        },
        "notes": browser_config.notes,
        "console_entry_count": session.log_entries.lock().len(),
    })
}

fn browser_closed_value(browser_config: &BrowserToolConfig) -> Value {
    serde_json::json!({
        "browser_open": false,
        "configured_url": browser_config.url,
        "notes": browser_config.notes,
    })
}

async fn navigate_page(page: &Page, raw_url: &str) -> BrowserResult<()> {
    let url = normalize_browser_url(raw_url)?;
    page.goto(url.as_str())
        .await
        .map_err(|err| format!("Browser navigation failed: {err}"))?;
    Ok(())
}

fn normalize_browser_url(raw_url: &str) -> BrowserResult<String> {
    let trimmed = raw_url.trim();
    if trimmed.is_empty() {
        return Err("URL must not be empty.".into());
    }
    if trimmed.eq_ignore_ascii_case("about:blank") {
        return Ok("about:blank".into());
    }

    let candidate = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    };

    let url =
        Url::parse(&candidate).map_err(|err| format!("Invalid browser URL '{trimmed}': {err}"))?;
    match url.scheme() {
        "http" | "https" => {}
        _ => {
            return Err(
                "Browser tools only allow localhost http(s) URLs and about:blank.".into(),
            )
        }
    }

    let host = url
        .host_str()
        .ok_or_else(|| "Browser URL must include a host.".to_string())?;
    if !is_local_browser_host(host) {
        return Err("Browser tools are restricted to localhost addresses.".into());
    }

    Ok(url.to_string())
}

fn is_local_browser_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1") || host.ends_with(".localhost")
}

async fn evaluate_string(page: &Page, expression: &str) -> BrowserResult<String> {
    page.evaluate(expression)
        .await
        .map_err(|err| format!("Browser evaluation failed: {err}"))?
        .into_value::<String>()
        .map_err(|err| format!("Failed to decode browser evaluation result: {err}"))
}

async fn clear_element_value(page: &Page, selector: &str) -> BrowserResult<()> {
    let selector = serde_json::to_string(selector)
        .map_err(|err| format!("Failed to encode CSS selector: {err}"))?;
    let script = format!(
        r#"() => {{
            const element = document.querySelector({selector});
            if (!element) {{
                throw new Error("Element not found for selector: " + {selector});
            }}
            if ("value" in element) {{
                element.value = "";
            }}
            if (typeof element.dispatchEvent === "function") {{
                element.dispatchEvent(new Event("input", {{ bubbles: true }}));
                element.dispatchEvent(new Event("change", {{ bubbles: true }}));
            }}
            return true;
        }}"#
    );

    page.evaluate(script.as_str())
        .await
        .map_err(|err| format!("Failed to clear element value: {err}"))?;
    Ok(())
}

async fn dom_field_value(
    element: &Element,
    field: DomField,
    property: Option<&str>,
) -> BrowserResult<Value> {
    match field {
        DomField::InnerText => option_string_to_json(
            element
                .inner_text()
                .await
                .map_err(|err| format!("Failed to read element innerText: {err}"))?,
        ),
        DomField::InnerHtml => option_string_to_json(
            element
                .inner_html()
                .await
                .map_err(|err| format!("Failed to read element innerHTML: {err}"))?,
        ),
        DomField::OuterHtml => option_string_to_json(
            element
                .outer_html()
                .await
                .map_err(|err| format!("Failed to read element outerHTML: {err}"))?,
        ),
        DomField::Attributes => {
            let attributes = element
                .attributes()
                .await
                .map_err(|err| format!("Failed to read element attributes: {err}"))?;
            Ok(flat_attributes_to_json(attributes))
        }
        DomField::Value => element
            .property("value")
            .await
            .map_err(|err| format!("Failed to read element value: {err}"))?
            .ok_or_else(|| "Element does not have a value property.".to_string()),
        DomField::Property => {
            let property =
                property.ok_or_else(|| "BrowserDom field 'property' requires a property name.".to_string())?;
            element
                .property(property)
                .await
                .map_err(|err| format!("Failed to read element property '{property}': {err}"))?
                .ok_or_else(|| format!("Element property '{property}' was not present."))
        }
        DomField::ComputedStyle => Err(
            "BrowserDom field 'computed_style' is handled before element-level DOM reads."
                .into(),
        ),
    }
}

async fn computed_style_value(
    page: &Page,
    selector: &str,
    property: Option<&str>,
    properties: Option<&[String]>,
    all: bool,
    limit: Option<usize>,
) -> BrowserResult<Value> {
    let selector = serde_json::to_string(selector)
        .map_err(|err| format!("Failed to encode CSS selector: {err}"))?;
    let properties = serde_json::to_string(&resolve_computed_style_properties(property, properties))
        .map_err(|err| format!("Failed to encode computed style properties: {err}"))?;
    let limit = limit
        .map(|limit| limit.to_string())
        .unwrap_or_else(|| "Number.MAX_SAFE_INTEGER".to_string());
    let script = format!(
        r#"() => {{
            const selector = {selector};
            const properties = {properties};
            const limit = {limit};
            const pick = (element) => {{
                const style = getComputedStyle(element);
                const values = {{}};
                for (const name of properties) {{
                    values[name] = style.getPropertyValue(name).trim();
                }}
                return values;
            }};
            if ({all}) {{
                return Array.from(document.querySelectorAll(selector)).slice(0, limit).map(pick);
            }}
            const element = document.querySelector(selector);
            if (!element) {{
                throw new Error("Element not found for selector: " + selector);
            }}
            return pick(element);
        }}"#
    );

    page.evaluate(script.as_str())
        .await
        .map_err(|err| format!("Failed to read computed styles: {err}"))?
        .into_value::<Value>()
        .map_err(|err| format!("Failed to decode computed style result: {err}"))
}

fn validate_browser_input(input: &BrowserInputInput) -> BrowserResult<()> {
    let has_text = input
        .text
        .as_deref()
        .map(|text| !text.is_empty())
        .unwrap_or(false);
    let has_key = input
        .key
        .as_deref()
        .map(|key| !key.trim().is_empty())
        .unwrap_or(false);

    if has_text || has_key {
        Ok(())
    } else {
        Err("BrowserInput requires text, key, or both.".into())
    }
}

fn resolve_computed_style_properties(
    property: Option<&str>,
    properties: Option<&[String]>,
) -> Vec<String> {
    if let Some(properties) = properties {
        if !properties.is_empty() {
            return properties.to_vec();
        }
    }
    if let Some(property) = property {
        if !property.trim().is_empty() {
            return vec![property.to_string()];
        }
    }
    DEFAULT_COMPUTED_STYLE_PROPERTIES
        .iter()
        .map(|property| property.to_string())
        .collect()
}

fn option_string_to_json(value: Option<String>) -> BrowserResult<Value> {
    Ok(value.map(Value::String).unwrap_or(Value::Null))
}

fn flat_attributes_to_json(attributes: Vec<String>) -> Value {
    let mut map = serde_json::Map::new();
    for pair in attributes.chunks(2) {
        let value = pair.get(1).cloned().unwrap_or_default();
        map.insert(pair[0].clone(), Value::String(value));
    }
    Value::Object(map)
}

fn console_event_to_entry(event: &EventConsoleApiCalled) -> BrowserLogEntry {
    BrowserLogEntry {
        source: "console".into(),
        level: event.r#type.as_ref().to_string(),
        text: remote_objects_to_text(&event.args),
        timestamp: *event.timestamp.inner(),
        url: event
            .stack_trace
            .as_ref()
            .and_then(|stack| stack.call_frames.first())
            .map(|frame| frame.url.clone()),
        line_number: event
            .stack_trace
            .as_ref()
            .and_then(|stack| stack.call_frames.first())
            .map(|frame| frame.line_number),
    }
}

fn log_event_to_entry(event: &EventEntryAdded) -> BrowserLogEntry {
    BrowserLogEntry {
        source: event.entry.source.as_ref().to_string(),
        level: event.entry.level.as_ref().to_string(),
        text: if let Some(args) = &event.entry.args {
            if args.is_empty() {
                event.entry.text.clone()
            } else {
                remote_objects_to_text(args)
            }
        } else {
            event.entry.text.clone()
        },
        timestamp: *event.entry.timestamp.inner(),
        url: event.entry.url.clone(),
        line_number: event.entry.line_number,
    }
}

fn remote_objects_to_text(args: &[RemoteObject]) -> String {
    args.iter()
        .map(remote_object_to_text)
        .collect::<Vec<_>>()
        .join(" ")
}

fn remote_object_to_text(value: &RemoteObject) -> String {
    if let Some(json) = &value.value {
        if let Some(text) = json.as_str() {
            return text.to_string();
        }
        return json.to_string();
    }
    if let Some(text) = &value.unserializable_value {
        return text.as_ref().to_string();
    }
    if let Some(text) = &value.description {
        return text.clone();
    }
    value.r#type.as_ref().to_string()
}

fn success_json(value: Value) -> ToolResult {
    match serde_json::to_string_pretty(&value) {
        Ok(json) => ToolResult::success(json),
        Err(err) => ToolResult::error(format!("Failed to serialize browser result: {err}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_urls_are_not_tracked_as_active_requests() {
        assert!(!should_track_request_as_active(
            "data:image/svg+xml,%3Csvg%20xmlns%3D%27http://www.w3.org/2000/svg%27%3E"
        ));
        assert!(should_track_request_as_active("http://localhost:3000/api/v1/experiments"));
    }

    #[test]
    fn trim_vec_to_limit_keeps_most_recent_entries() {
        let mut items = vec![1, 2, 3, 4, 5];
        trim_vec_to_limit(&mut items, 3);
        assert_eq!(items, vec![3, 4, 5]);
    }

    #[test]
    fn clear_network_tracking_resets_active_requests_and_timestamps() {
        let session = BrowserSession::default();
        session.active_request_ids.lock().insert("req-1".into());
        session.network_entries.lock().push(NetworkEntry {
            request_id: "req-1".into(),
            sequence: 0,
            url: "http://localhost/".into(),
            method: "GET".into(),
            resource_type: Some("Document".into()),
            status: Some(200),
            status_text: Some("OK".into()),
            mime_type: Some("text/html".into()),
            response_headers: None,
            error_text: None,
            timestamp: 1.0,
            encoded_data_length: None,
        });
        *session.last_network_completion_timestamp.lock() = Some(1.0);

        clear_network_tracking(&session);

        assert!(session.active_request_ids.lock().is_empty());
        assert!(session.network_entries.lock().is_empty());
        assert!(session.last_network_completion_timestamp.lock().is_none());
    }

    #[test]
    fn network_entries_can_represent_multiple_lifecycles_for_same_request_id() {
        let entries = vec![
            NetworkEntry {
                request_id: "req-1".into(),
                sequence: 0,
                url: "http://localhost/start".into(),
                method: "GET".into(),
                resource_type: Some("Document".into()),
                status: Some(302),
                status_text: Some("Found".into()),
                mime_type: Some("text/html".into()),
                response_headers: None,
                error_text: None,
                timestamp: 1.0,
                encoded_data_length: None,
            },
            NetworkEntry {
                request_id: "req-1".into(),
                sequence: 1,
                url: "http://localhost/final".into(),
                method: "GET".into(),
                resource_type: Some("Document".into()),
                status: Some(200),
                status_text: Some("OK".into()),
                mime_type: Some("text/html".into()),
                response_headers: None,
                error_text: None,
                timestamp: 2.0,
                encoded_data_length: None,
            },
        ];

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].request_id, entries[1].request_id);
        assert_ne!(entries[0].sequence, entries[1].sequence);
        assert_ne!(entries[0].url, entries[1].url);
    }

    #[test]
    fn normalize_localhost_url_without_scheme() {
        assert_eq!(
            normalize_browser_url("localhost:3000").unwrap(),
            "http://localhost:3000/"
        );
    }

    #[test]
    fn normalize_about_blank() {
        assert_eq!(normalize_browser_url("about:blank").unwrap(), "about:blank");
    }

    #[test]
    fn reject_remote_host() {
        let err = normalize_browser_url("https://example.com").unwrap_err();
        assert!(err.contains("localhost"));
    }

    #[test]
    fn allow_localhost_variants() {
        assert!(is_local_browser_host("localhost"));
        assert!(is_local_browser_host("foo.localhost"));
        assert!(is_local_browser_host("127.0.0.1"));
        assert!(is_local_browser_host("::1"));
    }

    #[test]
    fn parse_browser_tools_config_from_yaml() {
        let yaml = r#"
browser:
  window:
    width: 1600
    height: 900
  url: http://localhost:3000/
  notes: >
    Open the login page first.
    Use demo@example.com and example-password.
"#;

        let config: ToolsConfig = serde_saphyr::from_str(yaml).unwrap();
        let browser = config.browser.unwrap();
        assert_eq!(browser.window.width, 1600);
        assert_eq!(browser.window.height, 900);
        assert_eq!(browser.url.as_deref(), Some("http://localhost:3000/"));
        assert!(browser.notes.as_deref().unwrap().contains("demo@example.com"));
    }

    #[test]
    fn computed_style_properties_use_defaults() {
        let properties = resolve_computed_style_properties(None, None);
        assert!(properties.iter().any(|property| property == "font-size"));
        assert!(properties.iter().any(|property| property == "color"));
        assert!(properties.iter().any(|property| property == "display"));
    }

    #[test]
    fn computed_style_properties_prefer_explicit_list() {
        let explicit = vec!["font-size".to_string(), "color".to_string()];
        let properties =
            resolve_computed_style_properties(Some("margin"), Some(explicit.as_slice()));
        assert_eq!(properties, explicit);
    }

    #[test]
    fn browser_input_validation_requires_action() {
        let input = BrowserInputInput {
            selector: "input".into(),
            text: None,
            key: None,
            clear_first: Some(true),
        };
        assert_eq!(
            validate_browser_input(&input).unwrap_err(),
            "BrowserInput requires text, key, or both."
        );
    }

    #[test]
    fn browser_input_validation_allows_text_or_key() {
        let text_input = BrowserInputInput {
            selector: "input".into(),
            text: Some("demo".into()),
            key: None,
            clear_first: Some(true),
        };
        validate_browser_input(&text_input).unwrap();

        let key_input = BrowserInputInput {
            selector: "input".into(),
            text: None,
            key: Some("Enter".into()),
            clear_first: None,
        };
        validate_browser_input(&key_input).unwrap();
    }

    #[test]
    fn status_filter_exact() {
        assert!(matches_status_filter(Some(404), "404"));
        assert!(!matches_status_filter(Some(200), "404"));
        assert!(!matches_status_filter(None, "404"));
    }

    #[test]
    fn status_filter_range() {
        assert!(matches_status_filter(Some(404), "4xx"));
        assert!(matches_status_filter(Some(500), "5xx"));
        assert!(!matches_status_filter(Some(200), "4xx"));
        assert!(matches_status_filter(Some(201), "2xx"));
    }

    #[test]
    fn test_normalize_browser_url_restricted_to_localhost() {
        // Valid
        assert_eq!(normalize_browser_url("http://localhost:8080").unwrap(), "http://localhost:8080/");
        assert_eq!(normalize_browser_url("https://127.0.0.1/").unwrap(), "https://127.0.0.1/");
        assert_eq!(normalize_browser_url("app.localhost").unwrap(), "http://app.localhost/");

        // Invalid
        assert!(normalize_browser_url("https://google.com").is_err());
        assert!(normalize_browser_url("ftp://localhost").is_err());
        assert!(normalize_browser_url("").is_err());
    }
}
