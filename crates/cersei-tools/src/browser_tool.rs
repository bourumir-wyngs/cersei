//! Browser tools built around chromiumoxide for local web app development.

use super::*;
use chromiumoxide::cdp::browser_protocol::log::EventEntryAdded;
use chromiumoxide::cdp::browser_protocol::network::{
    self as cdp_network, EventLoadingFailed, EventLoadingFinished, EventRequestWillBeSent,
    EventResponseReceived,
};
use chromiumoxide::cdp::js_protocol::runtime::{EventConsoleApiCalled, RemoteObject};
use chromiumoxide::{Browser, BrowserConfig, Element, Page};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
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

pub struct BrowserTool;

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
pub struct BrowserRequest {
    pub action: BrowserAction,
    pub url: Option<String>,
    pub limit: Option<usize>,
    pub selector: Option<String>,
    pub field: Option<DomField>,
    pub property: Option<String>,
    pub properties: Option<Vec<String>>,
    pub all: Option<bool>,
    pub text: Option<String>,
    pub key: Option<String>,
    pub clear_first: Option<bool>,
    pub url_filter: Option<String>,
    pub resource_type: Option<String>,
    pub status_filter: Option<String>,
    pub failed_only: Option<bool>,
    pub storage: Option<StorageType>,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserAction {
    Open,
    Status,
    Close,
    Navigate,
    Console,
    Dom,
    Click,
    Input,
    Network,
    Css,
    Storage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageType {
    Cookies,
    LocalStorage,
    SessionStorage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DomField {
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

static BROWSER_SESSION_REGISTRY: once_cell::sync::Lazy<
    dashmap::DashMap<String, Arc<BrowserSession>>,
> = once_cell::sync::Lazy::new(dashmap::DashMap::new);

#[async_trait]
impl Tool for BrowserTool {
    fn name(&self) -> &str {
        "Browser"
    }

    fn description(&self) -> &str {
        "Automate a visible browser for local web development. actions: \
        `open`: launch/reuse window, `status`: check browser state, `close`: close browser, \
        `navigate`: go to URL, `console`: get logs, `dom`: inspect elements, \
        `click`: click element, `input`: type/press keys, `network`: inspect requests, \
        `css`: inspect rules, `storage`: inspect cookies/storage. \
        Only localhost URLs are allowed."
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
                    "enum": ["open", "status", "close", "navigate", "console", "dom", "click", "input", "network", "css", "storage"],
                    "description": "Action to perform."
                },
                "url": { "type": "string", "description": "URL for open/navigate." },
                "selector": { "type": "string", "description": "CSS selector for dom/click/input/css." },
                "text": { "type": "string", "description": "Text to type for input." },
                "key": { "type": "string", "description": "Key to press for input." },
                "limit": { "type": "integer", "description": "Limit for console/dom/network entries." },
                "field": { "type": "string", "enum": ["inner_text", "inner_html", "outer_html", "attributes", "value", "property", "computed_style"], "description": "DOM field to read." },
                "property": { "type": "string", "description": "Property name for dom field=property/computed_style." },
                "all": { "type": "boolean", "description": "Read all matching elements for dom." },
                "storage": { "type": "string", "enum": ["cookies", "local_storage", "session_storage"], "description": "Storage type to inspect." },
                "name": { "type": "string", "description": "Filter by name for storage." }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let req: BrowserRequest = match serde_json::from_value(input) {
            Ok(value) => value,
            Err(err) => return ToolResult::error(format!("Invalid input: {err}")),
        };

        match req.action {
            BrowserAction::Open => execute_open(req, ctx).await,
            BrowserAction::Status => execute_status(ctx).await,
            BrowserAction::Close => execute_close(ctx).await,
            BrowserAction::Navigate => execute_navigate(req, ctx).await,
            BrowserAction::Console => execute_console(req, ctx).await,
            BrowserAction::Dom => execute_dom(req, ctx).await,
            BrowserAction::Click => execute_click(req, ctx).await,
            BrowserAction::Input => execute_input(req, ctx).await,
            BrowserAction::Network => execute_network(req, ctx).await,
            BrowserAction::Css => execute_css(req, ctx).await,
            BrowserAction::Storage => execute_storage(req, ctx).await,
        }
    }
}

async fn execute_open(req: BrowserRequest, ctx: &ToolContext) -> ToolResult {
    let browser_config = browser_config_from_context(ctx);
    let requested_url = req.url.as_deref();
    match tokio::time::timeout(
        BROWSER_OPEN_TIMEOUT,
        ensure_browser_page(ctx, &browser_config, requested_url),
    )
    .await
    {
        Ok(Ok((session, page))) => success_json(
            browser_window_open_value(&session, &page, &browser_config, requested_url).await,
        ),
        Ok(Err(err)) => ToolResult::error(err),
        Err(_) => ToolResult::error("Browser action=open timed out. Try again."),
    }
}

async fn execute_status(ctx: &ToolContext) -> ToolResult {
    let browser_config = browser_config_from_context(ctx);
    match current_browser_page(ctx).await {
        Some((session, page)) => success_json(browser_status_value(&session, &page, &browser_config).await),
        None => success_json(browser_closed_value(&browser_config)),
    }
}

async fn execute_close(ctx: &ToolContext) -> ToolResult {
    match close_browser_session(&ctx.session_id).await {
        Ok(message) => ToolResult::success(message),
        Err(err) => ToolResult::error(err),
    }
}

async fn execute_navigate(req: BrowserRequest, ctx: &ToolContext) -> ToolResult {
    let url = match req.url {
        Some(url) => url,
        None => return ToolResult::error("`url` is required for action `navigate`"),
    };
    let (session, page) = match require_browser_page(ctx).await {
        Ok(state) => state,
        Err(err) => return ToolResult::error(err),
    };
    let browser_config = browser_config_from_context(ctx);
    match navigate_page(&page, &url).await {
        Ok(()) => success_json(browser_status_value(&session, &page, &browser_config).await),
        Err(err) => ToolResult::error(err),
    }
}

async fn execute_console(req: BrowserRequest, ctx: &ToolContext) -> ToolResult {
    let (session, _) = match require_browser_page(ctx).await {
        Ok(state) => state,
        Err(err) => return ToolResult::error(err),
    };
    let mut entries = session.log_entries.lock().clone();
    if let Some(limit) = req.limit {
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

async fn execute_dom(req: BrowserRequest, ctx: &ToolContext) -> ToolResult {
    let selector = match req.selector {
        Some(s) => s,
        None => return ToolResult::error("`selector` is required for action `dom`"),
    };
    let (_, page) = match require_browser_page(ctx).await {
        Ok(state) => state,
        Err(err) => return ToolResult::error(err),
    };
    let field = req.field.unwrap_or_default();
    let all = req.all.unwrap_or(false);

    if field == DomField::ComputedStyle {
        match computed_style_value(&page, &selector, req.property.as_deref(), req.properties.as_deref(), all, req.limit).await {
            Ok(value) => success_json(serde_json::json!({ "selector": selector, "field": field, "value": value })),
            Err(err) => ToolResult::error(err),
        }
    } else if all {
        let elements = match page.find_elements(selector.clone()).await {
            Ok(elements) => elements,
            Err(err) => return ToolResult::error(format!("Failed to query DOM: {err}")),
        };
        let mut items = Vec::new();
        for element in elements.into_iter().take(req.limit.unwrap_or(usize::MAX)) {
            match dom_field_value(&element, field, req.property.as_deref()).await {
                Ok(value) => items.push(value),
                Err(err) => return ToolResult::error(err),
            }
        }
        success_json(serde_json::json!({ "selector": selector, "field": field, "matches": items.len(), "items": items }))
    } else {
        let element = match page.find_element(selector.clone()).await {
            Ok(element) => element,
            Err(err) => return ToolResult::error(format!("Failed to query DOM: {err}")),
        };
        match dom_field_value(&element, field, req.property.as_deref()).await {
            Ok(value) => success_json(serde_json::json!({ "selector": selector, "field": field, "value": value })),
            Err(err) => ToolResult::error(err),
        }
    }
}

async fn execute_click(req: BrowserRequest, ctx: &ToolContext) -> ToolResult {
    let selector = match req.selector {
        Some(s) => s,
        None => return ToolResult::error("`selector` is required for action `click`"),
    };
    let (session, page) = match require_browser_page(ctx).await {
        Ok(state) => state,
        Err(err) => return ToolResult::error(err),
    };
    let browser_config = browser_config_from_context(ctx);
    let element = match page.find_element(selector).await {
        Ok(element) => element,
        Err(err) => return ToolResult::error(format!("Failed to query DOM: {err}")),
    };
    match element.click().await {
        Ok(_) => success_json(browser_status_value(&session, &page, &browser_config).await),
        Err(err) => ToolResult::error(format!("Browser click failed: {err}")),
    }
}

async fn execute_input(req: BrowserRequest, ctx: &ToolContext) -> ToolResult {
    let selector = match req.selector {
        Some(s) => s,
        None => return ToolResult::error("`selector` is required for action `input`"),
    };
    let (session, page) = match require_browser_page(ctx).await {
        Ok(state) => state,
        Err(err) => return ToolResult::error(err),
    };
    let browser_config = browser_config_from_context(ctx);

    if req.clear_first.unwrap_or(false) {
        if let Err(err) = clear_element_value(&page, &selector).await {
            return ToolResult::error(err);
        }
    }

    let element = match page.find_element(selector).await {
        Ok(element) => element,
        Err(err) => return ToolResult::error(format!("Failed to query DOM: {err}")),
    };
    if let Err(err) = element.click().await {
        return ToolResult::error(format!("Browser input focus failed: {err}"));
    }
    if let Some(text) = req.text.as_deref().filter(|t| !t.is_empty()) {
        if let Err(err) = element.type_str(text).await {
            return ToolResult::error(format!("Browser input type failed: {err}"));
        }
    }
    if let Some(key) = req.key.as_deref().filter(|k| !k.is_empty()) {
        if let Err(err) = element.press_key(key).await {
            return ToolResult::error(format!("Browser input key failed: {err}"));
        }
    }
    success_json(browser_status_value(&session, &page, &browser_config).await)
}

async fn execute_network(req: BrowserRequest, ctx: &ToolContext) -> ToolResult {
    let (session, _) = match require_browser_page(ctx).await {
        Ok(state) => state,
        Err(err) => return ToolResult::error(err),
    };
    let all_entries = session.network_entries.lock().clone();
    let mut entries: Vec<&NetworkEntry> = all_entries.iter().collect();

    if let Some(ref filter) = req.url_filter { entries.retain(|e| e.url.contains(filter)); }
    if let Some(ref rt) = req.resource_type {
        let rt_lc = rt.to_ascii_lowercase();
        entries.retain(|e| e.resource_type.as_ref().map(|t| t.to_ascii_lowercase() == rt_lc).unwrap_or(false));
    }
    if let Some(ref filter) = req.status_filter { entries.retain(|e| matches_status_filter(e.status, filter)); }
    if req.failed_only.unwrap_or(false) { entries.retain(|e| e.error_text.is_some()); }

    entries.reverse();
    let total = entries.len();
    if let Some(limit) = req.limit { entries.truncate(limit); }

    success_json(serde_json::json!({
        "entries": entries.into_iter().cloned().collect::<Vec<_>>(),
        "returned": std::cmp::min(total, req.limit.unwrap_or(total)),
        "total_matching": total,
        "total_collected": all_entries.len(),
    }))
}

async fn execute_css(req: BrowserRequest, ctx: &ToolContext) -> ToolResult {
    let selector = match req.selector {
        Some(s) => s,
        None => return ToolResult::error("`selector` is required for action `css`"),
    };
    let (_, page) = match require_browser_page(ctx).await {
        Ok(state) => state,
        Err(err) => return ToolResult::error(err),
    };
    match css_matched_rules(&page, &selector).await {
        Ok(value) => success_json(serde_json::json!({ "selector": selector, "matched_rules": value })),
        Err(err) => ToolResult::error(err),
    }
}

async fn execute_storage(req: BrowserRequest, ctx: &ToolContext) -> ToolResult {
    let storage_type = match req.storage {
        Some(s) => s,
        None => return ToolResult::error("`storage` is required for action `storage`"),
    };
    let (_, page) = match require_browser_page(ctx).await {
        Ok(state) => state,
        Err(err) => return ToolResult::error(err),
    };

    match storage_type {
        StorageType::Cookies => match page.get_cookies().await {
            Ok(cookies) => {
                let items: Vec<Value> = cookies.iter()
                    .filter(|c| req.name.as_ref().map(|n| c.name == *n).unwrap_or(true))
                    .map(|c| serde_json::json!({ "name": c.name, "value": c.value, "domain": c.domain, "path": c.path, "expires": c.expires }))
                    .collect();
                success_json(serde_json::json!({ "storage": "cookies", "count": items.len(), "items": items }))
            }
            Err(err) => ToolResult::error(format!("Failed to get cookies: {err}")),
        },
        StorageType::LocalStorage | StorageType::SessionStorage => {
            let obj = match storage_type { StorageType::LocalStorage => "localStorage", _ => "sessionStorage" };
            let script = if let Some(ref name) = req.name {
                format!("() => {{ const val = {obj}.getItem({}); return val === null ? null : {{ key: {}, value: val }}; }}", serde_json::to_string(name).unwrap(), serde_json::to_string(name).unwrap())
            } else {
                format!("() => {{ const items = []; for (let i = 0; i < {obj}.length; i++) {{ const key = {obj}.key(i); items.push({{ key, value: {obj}.getItem(key) }}); }} return items; }}")
            };
            match page.evaluate(script.as_str()).await {
                Ok(res) => match res.into_value::<Value>() {
                    Ok(val) => success_json(serde_json::json!({ "storage": obj, "data": val })),
                    Err(err) => ToolResult::error(format!("Failed to decode storage: {err}")),
                },
                Err(err) => ToolResult::error(format!("Failed to read {obj}: {err}")),
            }
        }
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

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
    let selector_json = serde_json::to_string(selector).map_err(|e| e.to_string())?;
    let script = format!(
        r#"() => {{
            const el = document.querySelector({selector_json});
            if (!el) throw new Error("Element not found: " + {selector_json});
            const rules = [];
            try {{
                for (const sheet of document.styleSheets) {{
                    let href = null;
                    try {{ href = sheet.href || (sheet.ownerNode && sheet.ownerNode.tagName === 'STYLE' ? 'inline' : null); }} catch(_) {{}}
                    let cssRules;
                    try {{ cssRules = sheet.cssRules; }} catch(_) {{ continue; }}
                    for (let i = 0; i < cssRules.length; i++) {{
                        const rule = cssRules[i];
                        if (rule.type !== CSSRule.STYLE_RULE) continue;
                        if (!el.matches(rule.selectorText)) continue;
                        const props = {{}};
                        for (let j = 0; j < rule.style.length; j++) {{
                            const prop = rule.style[j];
                            props[prop] = rule.style.getPropertyValue(prop);
                        }}
                        rules.push({{ selector: rule.selectorText, stylesheet: href, properties: props }});
                    }}
                }}
            }} catch(e) {{}}
            const inline = el.getAttribute('style');
            if (inline) {{
                const props = {{}};
                for (let j = 0; j < el.style.length; j++) {{
                    const prop = el.style[j];
                    props[prop] = el.style.getPropertyValue(prop);
                }}
                rules.push({{ selector: "(inline style)", stylesheet: null, properties: props }});
            }}
            return rules;
        }}"#
    );
    page.evaluate(script.as_str()).await
        .map_err(|e| e.to_string())?
        .into_value::<Value>()
        .map_err(|e| e.to_string())
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
        if let Some(url) = requested_url { navigate_page(&page, url).await?; }
        drop(guard);
        return Ok((session, page));
    }

    let profile_dir = TempDir::new().map_err(|e| e.to_string())?;
    let config = BrowserConfig::builder()
        .with_head()
        .window_size(browser_config.window.width, browser_config.window.height)
        .viewport(None)
        .user_data_dir(profile_dir.path())
        .build()
        .map_err(|e| e.to_string())?;

    let (browser, mut handler) = Browser::launch(config).await.map_err(|e| e.to_string())?;
    let handler_task = tokio::spawn(async move { while let Some(_) = handler.next().await {} });

    let page = browser.new_page("about:blank").await.map_err(|e| e.to_string())?;
    page.enable_log().await.map_err(|e| e.to_string())?;
    page.execute(cdp_network::EnableParams::default()).await.map_err(|e| e.to_string())?;

    let mut tasks = vec![handler_task];
    tasks.extend(spawn_page_listener_tasks(page.clone(), session.clone()).await?);
    session.log_entries.lock().clear();
    clear_network_tracking(&session);
    *session.browser.lock().await = Some(browser);
    *session.page.write() = Some(page.clone());
    *session.profile_dir.lock() = Some(profile_dir);
    *session.tasks.lock() = tasks;

    if let Some(url) = requested_url.or(browser_config.url.as_deref()) { navigate_page(&page, url).await?; }
    drop(guard);
    Ok((session, page))
}

async fn require_browser_page(ctx: &ToolContext) -> BrowserResult<(Arc<BrowserSession>, Page)> {
    match current_browser_page(ctx).await {
        Some(p) => Ok(p),
        None => Err("No browser window open. Open window first with Browser action=open.".into()),
    }
}

async fn current_browser_page(ctx: &ToolContext) -> Option<(Arc<BrowserSession>, Page)> {
    let session = browser_session(&ctx.session_id);
    let page = session.page.read().clone()?;
    if page.url().await.is_ok() { Some((session, page)) } else { clear_stale_browser_session(&ctx.session_id).await; None }
}

fn browser_config_from_context(ctx: &ToolContext) -> BrowserToolConfig {
    ctx.extensions.get::<ToolsConfig>().and_then(|c| c.browser.clone()).unwrap_or_default()
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
        return Ok("No browser session open.".into());
    };
    let mut browser = session.browser.lock().await.take();
    if let Some(browser) = browser.as_mut() {
        let _ = browser.close().await;
        let _ = browser.wait().await;
    }
    abort_session_tasks(&session);
    *session.page.write() = None;
    Ok("Browser session closed.".into())
}

fn abort_session_tasks(session: &BrowserSession) {
    for task in session.tasks.lock().drain(..) { task.abort(); }
}

async fn spawn_page_listener_tasks(page: Page, session: Arc<BrowserSession>) -> BrowserResult<Vec<tokio::task::JoinHandle<()>>> {
    let mut console_events = page.event_listener::<EventConsoleApiCalled>().await.map_err(|e| e.to_string())?;
    let s1 = session.clone();
    let t1 = tokio::spawn(async move { while let Some(e) = console_events.next().await { push_log_entry(&s1, console_event_to_entry(e.as_ref())); } });

    let mut log_events = page.event_listener::<EventEntryAdded>().await.map_err(|e| e.to_string())?;
    let s2 = session.clone();
    let t2 = tokio::spawn(async move { while let Some(e) = log_events.next().await { push_log_entry(&s2, log_event_to_entry(e.as_ref())); } });

    let mut request_events = page.event_listener::<EventRequestWillBeSent>().await.map_err(|e| e.to_string())?;
    let s3 = session.clone();
    let t3 = tokio::spawn(async move { while let Some(e) = request_events.next().await {
        let rid = e.request_id.inner().clone();
        if should_track_request_as_active(&e.request.url) { s3.active_request_ids.lock().insert(rid.clone()); }
        push_network_entry(&s3, NetworkEntry {
            request_id: rid, sequence: s3.next_network_sequence.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            url: e.request.url.clone(), method: e.request.method.clone(), resource_type: e.r#type.as_ref().map(|t| t.as_ref().to_string()),
            status: None, status_text: None, mime_type: None, response_headers: None, error_text: None, timestamp: *e.timestamp.inner(), encoded_data_length: None,
        });
    }});

    let mut response_events = page.event_listener::<EventResponseReceived>().await.map_err(|e| e.to_string())?;
    let s4 = session.clone();
    let t4 = tokio::spawn(async move { while let Some(e) = response_events.next().await {
        let rid = e.request_id.inner().clone();
        let mut entries = s4.network_entries.lock();
        if let Some(entry) = entries.iter_mut().rev().find(|en| en.request_id == rid && en.status.is_none() && en.error_text.is_none()) {
            entry.url = e.response.url.clone(); entry.status = Some(e.response.status); entry.status_text = Some(e.response.status_text.clone());
            entry.mime_type = Some(e.response.mime_type.clone()); entry.response_headers = Some(e.response.headers.inner().clone());
            entry.encoded_data_length = Some(e.response.encoded_data_length); entry.timestamp = entry.timestamp.max(*e.timestamp.inner());
        }
    }});

    let mut finished_events = page.event_listener::<EventLoadingFinished>().await.map_err(|e| e.to_string())?;
    let s5 = session.clone();
    let t5 = tokio::spawn(async move { while let Some(e) = finished_events.next().await {
        let rid = e.request_id.inner().clone();
        let ts = *e.timestamp.inner();
        let mut entries = s5.network_entries.lock();
        if let Some(entry) = entries.iter_mut().rev().find(|en| en.request_id == rid && en.error_text.is_none()) {
            entry.encoded_data_length = Some(e.encoded_data_length); entry.timestamp = entry.timestamp.max(ts);
        }
        drop(entries);
        mark_network_completion(&s5, &rid, ts);
    }});

    let mut failed_events = page.event_listener::<EventLoadingFailed>().await.map_err(|e| e.to_string())?;
    let s6 = session;
    let t6 = tokio::spawn(async move { while let Some(e) = failed_events.next().await {
        let rid = e.request_id.inner().clone();
        let ts = *e.timestamp.inner();
        let mut entries = s6.network_entries.lock();
        if let Some(entry) = entries.iter_mut().rev().find(|en| en.request_id == rid && en.error_text.is_none()) {
            entry.error_text = Some(e.error_text.clone()); entry.timestamp = entry.timestamp.max(ts);
        }
        drop(entries);
        mark_network_completion(&s6, &rid, ts);
    }});

    Ok(vec![t1, t2, t3, t4, t5, t6])
}

async fn browser_status_value(session: &BrowserSession, page: &Page, config: &BrowserToolConfig) -> Value {
    let url = page.url().await.ok().flatten();
    let title = evaluate_string(page, "document.title").await.ok();
    serde_json::json!({ "browser_open": true, "url": url, "title": title, "console_entry_count": session.log_entries.lock().len(), "configured_url": config.url, "notes": config.notes })
}

async fn browser_window_open_value(session: &BrowserSession, page: &Page, config: &BrowserToolConfig, req_url: Option<&str>) -> Value {
    let url = page.url().await.ok().flatten();
    serde_json::json!({ "browser_open": true, "message": "Browser open. Use navigate, dom, click, input, console, etc.", "url": url, "requested_url": req_url, "configured_url": config.url, "window": { "width": config.window.width, "height": config.window.height }, "notes": config.notes, "console_entry_count": session.log_entries.lock().len() })
}

fn browser_closed_value(config: &BrowserToolConfig) -> Value {
    serde_json::json!({ "browser_open": false, "configured_url": config.url, "notes": config.notes })
}

async fn navigate_page(page: &Page, raw_url: &str) -> BrowserResult<()> {
    let url = normalize_browser_url(raw_url)?;
    page.goto(url.as_str()).await.map_err(|e| e.to_string())?;
    Ok(())
}

fn normalize_browser_url(raw: &str) -> BrowserResult<String> {
    let t = raw.trim();
    if t.is_empty() { return Err("Empty URL".into()); }
    if t.eq_ignore_ascii_case("about:blank") { return Ok("about:blank".into()); }
    let candidate = if t.contains("://") { t.to_string() } else { format!("http://{t}") };
    let url = Url::parse(&candidate).map_err(|e| e.to_string())?;
    if !matches!(url.scheme(), "http" | "https") { return Err("Localhost http(s) only".into()); }
    let host = url.host_str().ok_or("No host")?;
    if !is_local_browser_host(host) { return Err("Localhost only".into()); }
    Ok(url.to_string())
}

fn is_local_browser_host(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    matches!(h.as_str(), "localhost" | "127.0.0.1" | "::1") || h.ends_with(".localhost")
}

async fn evaluate_string(page: &Page, exp: &str) -> BrowserResult<String> {
    page.evaluate(exp).await.map_err(|e| e.to_string())?.into_value::<String>().map_err(|e| e.to_string())
}

async fn clear_element_value(page: &Page, selector: &str) -> BrowserResult<()> {
    let sel = serde_json::to_string(selector).unwrap();
    let script = format!("() => {{ const el = document.querySelector({sel}); if (el && 'value' in el) {{ el.value = ''; el.dispatchEvent(new Event('input', {{ bubbles: true }})); el.dispatchEvent(new Event('change', {{ bubbles: true }})); }} }}");
    page.evaluate(script.as_str()).await.map_err(|e| e.to_string())?;
    Ok(())
}

async fn dom_field_value(el: &Element, field: DomField, prop: Option<&str>) -> BrowserResult<Value> {
    match field {
        DomField::InnerText => Ok(el.inner_text().await.map_err(|e| e.to_string())?.map(Value::String).unwrap_or(Value::Null)),
        DomField::InnerHtml => Ok(el.inner_html().await.map_err(|e| e.to_string())?.map(Value::String).unwrap_or(Value::Null)),
        DomField::OuterHtml => Ok(el.outer_html().await.map_err(|e| e.to_string())?.map(Value::String).unwrap_or(Value::Null)),
        DomField::Attributes => Ok(flat_attributes_to_json(el.attributes().await.map_err(|e| e.to_string())?)),
        DomField::Value => Ok(el.property("value").await.map_err(|e| e.to_string())?.unwrap_or(Value::Null)),
        DomField::Property => {
            let p = prop.ok_or("Property name required")?;
            Ok(el.property(p).await.map_err(|e| e.to_string())?.ok_or("Property not found")?)
        }
        _ => Err("Invalid field".into()),
    }
}

async fn computed_style_value(page: &Page, selector: &str, prop: Option<&str>, props: Option<&[String]>, all: bool, limit: Option<usize>) -> BrowserResult<Value> {
    let sel = serde_json::to_string(selector).unwrap();
    let p_list = serde_json::to_string(&resolve_computed_style_properties(prop, props)).unwrap();
    let lim = limit.unwrap_or(usize::MAX);
    let script = format!(r#"() => {{
        const sel = {sel}; const props = {p_list}; const lim = {lim};
        const pick = (el) => {{ const s = getComputedStyle(el); const v = {{}}; for (const n of props) v[n] = s.getPropertyValue(n).trim(); return v; }};
        if ({all}) return Array.from(document.querySelectorAll(sel)).slice(0, lim).map(pick);
        const el = document.querySelector(sel); if (!el) throw new Error("Not found"); return pick(el);
    }}"#);
    page.evaluate(script.as_str()).await.map_err(|e| e.to_string())?.into_value::<Value>().map_err(|e| e.to_string())
}

fn resolve_computed_style_properties(prop: Option<&str>, props: Option<&[String]>) -> Vec<String> {
    if let Some(p) = props { if !p.is_empty() { return p.to_vec(); } }
    if let Some(p) = prop { if !p.trim().is_empty() { return vec![p.to_string()]; } }
    DEFAULT_COMPUTED_STYLE_PROPERTIES.iter().map(|p| p.to_string()).collect()
}

fn flat_attributes_to_json(attrs: Vec<String>) -> Value {
    let mut map = serde_json::Map::new();
    for chunk in attrs.chunks(2) { if chunk.len() == 2 { map.insert(chunk[0].clone(), Value::String(chunk[1].clone())); } }
    Value::Object(map)
}

fn console_event_to_entry(e: &EventConsoleApiCalled) -> BrowserLogEntry {
    BrowserLogEntry { source: "console".into(), level: e.r#type.as_ref().to_string(), text: remote_objects_to_text(&e.args), timestamp: *e.timestamp.inner(), url: e.stack_trace.as_ref().and_then(|s| s.call_frames.first()).map(|f| f.url.clone()), line_number: e.stack_trace.as_ref().and_then(|s| s.call_frames.first()).map(|f| f.line_number) }
}

fn log_event_to_entry(e: &EventEntryAdded) -> BrowserLogEntry {
    BrowserLogEntry { source: e.entry.source.as_ref().to_string(), level: e.entry.level.as_ref().to_string(), text: if let Some(a) = &e.entry.args { if a.is_empty() { e.entry.text.clone() } else { remote_objects_to_text(a) } } else { e.entry.text.clone() }, timestamp: *e.entry.timestamp.inner(), url: e.entry.url.clone(), line_number: e.entry.line_number }
}

fn remote_objects_to_text(args: &[RemoteObject]) -> String { args.iter().map(remote_object_to_text).collect::<Vec<_>>().join(" ") }
fn remote_object_to_text(v: &RemoteObject) -> String { if let Some(j) = &v.value { if let Some(s) = j.as_str() { return s.to_string(); } return j.to_string(); } if let Some(t) = &v.unserializable_value { return t.as_ref().to_string(); } if let Some(t) = &v.description { return t.clone(); } v.r#type.as_ref().to_string() }
fn success_json(v: Value) -> ToolResult { match serde_json::to_string_pretty(&v) { Ok(j) => ToolResult::success(j), Err(e) => ToolResult::error(e.to_string()) } }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_browser_url_restricted_to_localhost() {
        assert_eq!(normalize_browser_url("http://localhost:8080").unwrap(), "http://localhost:8080/");
        assert_eq!(normalize_browser_url("app.localhost").unwrap(), "http://app.localhost/");
        assert!(normalize_browser_url("https://google.com").is_err());
    }
}
