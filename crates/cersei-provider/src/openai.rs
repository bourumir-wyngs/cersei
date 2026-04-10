//! OpenAI-compatible provider (works with OpenAI, Azure, Ollama, etc.)

use crate::*;
use cersei_types::*;
use futures::StreamExt;
use tokio::sync::mpsc;

const OPENAI_API_BASE: &str = "https://api.openai.com/v1";

pub struct OpenAi {
    auth: Auth,
    base_url: String,
    default_model: String,
    client: reqwest::Client,
}

impl OpenAi {
    pub fn new(auth: Auth) -> Self {
        Self {
            auth,
            base_url: OPENAI_API_BASE.to_string(),
            default_model: "gpt-4o".to_string(),
            client: reqwest::Client::new(),
        }
    }

    pub fn from_env() -> Result<Self> {
        let key = std::env::var("OPENAI_API_KEY")
            .map_err(|_| CerseiError::Auth("OPENAI_API_KEY not set".into()))?;
        Ok(Self::new(Auth::ApiKey(key)))
    }

    pub fn builder() -> OpenAiBuilder {
        OpenAiBuilder::default()
    }
}

#[async_trait::async_trait]
impl Provider for OpenAi {
    fn name(&self) -> &str {
        "openai"
    }

    fn context_window(&self, model: &str) -> u64 {
        match model {
            m if m.contains("gpt-4o") => 128_000,
            m if m.contains("gpt-4-turbo") => 128_000,
            m if m.contains("gpt-4") => 8_192,
            m if m.contains("gpt-3.5") => 16_385,
            _ => 128_000,
        }
    }

    fn capabilities(&self, _model: &str) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_use: true,
            vision: true,
            thinking: false,
            system_prompt: true,
            caching: false,
        }
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionStream> {
        let model = if request.model.is_empty() {
            self.default_model.clone()
        } else {
            request.model.clone()
        };

        if uses_responses_api(&self.base_url, &model) {
            return self.complete_via_responses(request, model).await;
        }

        // Build OpenAI-format messages
        let mut api_messages: Vec<serde_json::Value> = Vec::new();

        if let Some(system) = &request.system {
            api_messages.push(serde_json::json!({
                "role": "system",
                "content": system,
            }));
        }

        // Build a map of tool_use_id → tool name for tool result messages.
        // Gemini requires the function name on tool result (function_response) entries.
        let mut tool_id_to_name: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for msg in &request.messages {
            if let MessageContent::Blocks(blocks) = &msg.content {
                for block in blocks {
                    if let ContentBlock::ToolUse { id, name, .. } = block {
                        tool_id_to_name.insert(id.clone(), name.clone());
                    }
                }
            }
        }

        for msg in &request.messages {
            match msg.role {
                Role::User => {
                    // Check if this is a tool result message
                    if let MessageContent::Blocks(blocks) = &msg.content {
                        for block in blocks {
                            if let ContentBlock::ToolResult {
                                tool_use_id,
                                content,
                                is_error: _,
                            } = block
                            {
                                let mut result = serde_json::json!({
                                    "role": "tool",
                                    "tool_call_id": tool_use_id,
                                    "content": content,
                                });
                                // Gemini requires function name on tool results
                                if let Some(name) = tool_id_to_name.get(tool_use_id) {
                                    result["name"] = serde_json::json!(name);
                                }
                                api_messages.push(result);
                            }
                        }
                        // Also include any text blocks as a user message
                        let text: String = blocks
                            .iter()
                            .filter_map(|b| {
                                if let ContentBlock::Text { text } = b {
                                    Some(text.as_str())
                                } else {
                                    None
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        if !text.is_empty() {
                            api_messages.push(serde_json::json!({
                                "role": "user",
                                "content": text,
                            }));
                        }
                    } else {
                        api_messages.push(serde_json::json!({
                            "role": "user",
                            "content": msg.get_all_text(),
                        }));
                    }
                }
                Role::Assistant => {
                    // Check for tool_use blocks — serialize as tool_calls
                    if let MessageContent::Blocks(blocks) = &msg.content {
                        let tool_uses: Vec<&ContentBlock> = blocks
                            .iter()
                            .filter(|b| matches!(b, ContentBlock::ToolUse { .. }))
                            .collect();
                        if !tool_uses.is_empty() {
                            let tool_calls: Vec<serde_json::Value> = tool_uses
                                .iter()
                                .map(|b| {
                                    if let ContentBlock::ToolUse {
                                        id,
                                        name,
                                        input,
                                        thought_signature,
                                    } = b
                                    {
                                        let mut tc = serde_json::json!({
                                            "id": id,
                                            "type": "function",
                                            "function": {
                                                "name": name,
                                                "arguments": input.to_string(),
                                            }
                                        });
                                        if let Some(sig) = thought_signature {
                                            // Send in both formats: top-level and Gemini's nested format
                                            tc["thought_signature"] = serde_json::json!(sig);
                                            tc["extra_content"] = serde_json::json!({
                                                "google": {
                                                    "thought_signature": sig
                                                }
                                            });
                                        }
                                        tc
                                    } else {
                                        serde_json::json!({})
                                    }
                                })
                                .collect();

                            let text_content: String = blocks
                                .iter()
                                .filter_map(|b| {
                                    if let ContentBlock::Text { text } = b {
                                        Some(text.as_str())
                                    } else {
                                        None
                                    }
                                })
                                .collect::<Vec<_>>()
                                .join("");

                            let mut asst_msg = serde_json::json!({
                                "role": "assistant",
                                "tool_calls": tool_calls,
                            });
                            if !text_content.is_empty() {
                                asst_msg["content"] = serde_json::json!(text_content);
                            }
                            api_messages.push(asst_msg);
                        } else {
                            api_messages.push(serde_json::json!({
                                "role": "assistant",
                                "content": msg.get_all_text(),
                            }));
                        }
                    } else {
                        api_messages.push(serde_json::json!({
                            "role": "assistant",
                            "content": msg.get_all_text(),
                        }));
                    }
                }
                Role::System => {
                    api_messages.push(serde_json::json!({
                        "role": "system",
                        "content": msg.get_all_text(),
                    }));
                }
            }
        }

        let mut body = serde_json::json!({
            "model": model,
            "messages": api_messages,
            "max_completion_tokens": request.max_tokens,
            "stream": true,
        });

        if let Some(temp) = request.temperature {
            body["temperature"] = serde_json::json!(temp);
        }

        if !request.tools.is_empty() {
            let tools: Vec<serde_json::Value> = request
                .tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.input_schema,
                        }
                    })
                })
                .collect();
            body["tools"] = serde_json::Value::Array(tools);
        }

        let url = format!("{}/chat/completions", self.base_url);
        let auth_header = match &self.auth {
            Auth::ApiKey(key) | Auth::Bearer(key) => format!("Bearer {}", key),
            Auth::OAuth { token, .. } => format!("Bearer {}", token.access_token),
            Auth::Custom(_) => String::new(),
        };

        let (tx, rx) = mpsc::channel(256);

        let req = self
            .client
            .post(&url)
            .header("authorization", &auth_header)
            .header("content-type", "application/json")
            .json(&body)
            .build()
            .map_err(CerseiError::Http)?;

        let client = self.client.clone();

        tokio::spawn(async move {
            match client.execute(req).await {
                Ok(response) => {
                    if !response.status().is_success() {
                        let status = response.status().as_u16();
                        let body = response.text().await.unwrap_or_default();
                        let _ = tx
                            .send(StreamEvent::Error {
                                message: format!("HTTP {}: {}", status, body),
                            })
                            .await;
                        return;
                    }

                    let _ = tx
                        .send(StreamEvent::MessageStart {
                            id: String::new(),
                            model: String::new(),
                        })
                        .await;
                    let mut stream = response.bytes_stream();
                    let mut buffer = String::new();
                    let mut text_started = false;
                    // Track tool calls being assembled across chunks
                    // OpenAI sends: tool_calls[i].id, tool_calls[i].function.name (first chunk)
                    //               tool_calls[i].function.arguments (subsequent chunks, accumulated)
                    // index -> (id, name, args_json, thought_signature)
                    let mut tool_calls: std::collections::HashMap<
                        usize,
                        (String, String, String, Option<String>),
                    > = std::collections::HashMap::new();
                    let mut has_tool_calls = false;

                    while let Some(chunk) = stream.next().await {
                        match chunk {
                            Ok(bytes) => {
                                buffer.push_str(&String::from_utf8_lossy(&bytes));
                                while let Some(pos) = buffer.find("\n") {
                                    let line = buffer[..pos].to_string();
                                    buffer = buffer[pos + 1..].to_string();

                                    if let Some(data) = line.strip_prefix("data: ") {
                                        let data = data.trim();
                                        if data == "[DONE]" {
                                            // Emit accumulated tool calls
                                            for (idx, (id, name, args, sig)) in &tool_calls {
                                                let _input: serde_json::Value =
                                                    serde_json::from_str(args)
                                                        .unwrap_or(serde_json::Value::Null);
                                                let _ = tx
                                                    .send(StreamEvent::ContentBlockStart {
                                                        index: *idx + 1,
                                                        block_type: "tool_use".into(),
                                                        id: Some(id.clone()),
                                                        name: Some(name.clone()),
                                                        thought_signature: sig.clone(),
                                                    })
                                                    .await;
                                                // Send full args as InputJsonDelta
                                                let _ = tx
                                                    .send(StreamEvent::InputJsonDelta {
                                                        index: *idx + 1,
                                                        partial_json: args.clone(),
                                                    })
                                                    .await;
                                                let _ = tx
                                                    .send(StreamEvent::ContentBlockStop {
                                                        index: *idx + 1,
                                                    })
                                                    .await;
                                            }

                                            if text_started {
                                                let _ = tx
                                                    .send(StreamEvent::ContentBlockStop {
                                                        index: 0,
                                                    })
                                                    .await;
                                            }

                                            let stop = if has_tool_calls {
                                                StopReason::ToolUse
                                            } else {
                                                StopReason::EndTurn
                                            };

                                            // Extract usage if available
                                            let _ = tx
                                                .send(StreamEvent::MessageDelta {
                                                    stop_reason: Some(stop),
                                                    usage: None,
                                                })
                                                .await;
                                            let _ = tx.send(StreamEvent::MessageStop).await;
                                            return;
                                        }

                                        if let Ok(json) =
                                            serde_json::from_str::<serde_json::Value>(data)
                                        {
                                            let delta = &json["choices"][0]["delta"];
                                            let finish_reason =
                                                json["choices"][0]["finish_reason"].as_str();

                                            // Text content
                                            if let Some(text) = delta["content"].as_str() {
                                                if !text_started {
                                                    text_started = true;
                                                    let _ = tx
                                                        .send(StreamEvent::ContentBlockStart {
                                                            index: 0,
                                                            block_type: "text".into(),
                                                            id: None,
                                                            name: None,
                                                            thought_signature: None,
                                                        })
                                                        .await;
                                                }
                                                let _ = tx
                                                    .send(StreamEvent::TextDelta {
                                                        index: 0,
                                                        text: text.to_string(),
                                                    })
                                                    .await;
                                            }

                                            // thought_signature may arrive at the delta level
                                            // (applies to all tool calls in this chunk).
                                            let delta_sig = delta["thought_signature"]
                                                .as_str()
                                                .map(|s| s.to_string());

                                            // Tool calls (accumulated across chunks)
                                            if let Some(tc_array) = delta["tool_calls"].as_array() {
                                                has_tool_calls = true;
                                                if std::env::var("CERSEI_DEBUG_REQUEST").is_ok() {
                                                    eprintln!(
                                                        "\x1b[90m[stream] delta: {}\x1b[0m",
                                                        serde_json::to_string(delta)
                                                            .unwrap_or_default()
                                                    );
                                                }
                                                for tc in tc_array {
                                                    let idx =
                                                        tc["index"].as_u64().unwrap_or(0) as usize;
                                                    let entry = tool_calls
                                                        .entry(idx)
                                                        .or_insert_with(|| {
                                                            (
                                                                String::new(),
                                                                String::new(),
                                                                String::new(),
                                                                None,
                                                            )
                                                        });

                                                    // First chunk has id and function.name
                                                    if let Some(id) = tc["id"].as_str() {
                                                        entry.0 = id.to_string();
                                                    }
                                                    if let Some(name) =
                                                        tc["function"]["name"].as_str()
                                                    {
                                                        entry.1 = name.to_string();
                                                    }
                                                    // Arguments accumulate across chunks
                                                    if let Some(args) =
                                                        tc["function"]["arguments"].as_str()
                                                    {
                                                        entry.2.push_str(args);
                                                    }
                                                    // Gemini thought_signature — check all known locations.
                                                    let sig = tc["thought_signature"]
                                                        .as_str()
                                                        .or_else(|| {
                                                            tc["extra_content"]["google"]
                                                                ["thought_signature"]
                                                                .as_str()
                                                        })
                                                        .or_else(|| {
                                                            tc["function"]["thought_signature"]
                                                                .as_str()
                                                        })
                                                        .map(|s| s.to_string())
                                                        .or_else(|| delta_sig.clone());
                                                    if let Some(sig) = sig {
                                                        entry.3 = Some(sig);
                                                    }
                                                }
                                            }

                                            // Usage from the final chunk
                                            if let Some(usage) = json["usage"].as_object() {
                                                let input_tokens = usage
                                                    .get("prompt_tokens")
                                                    .and_then(|v| v.as_u64())
                                                    .unwrap_or(0);
                                                let output_tokens = usage
                                                    .get("completion_tokens")
                                                    .and_then(|v| v.as_u64())
                                                    .unwrap_or(0);
                                                let _ = tx
                                                    .send(StreamEvent::MessageDelta {
                                                        stop_reason: finish_reason.and_then(|r| {
                                                            match r {
                                                                "stop" => Some(StopReason::EndTurn),
                                                                "tool_calls" => {
                                                                    Some(StopReason::ToolUse)
                                                                }
                                                                "length" => {
                                                                    Some(StopReason::MaxTokens)
                                                                }
                                                                _ => None,
                                                            }
                                                        }),
                                                        usage: Some(Usage {
                                                            input_tokens,
                                                            output_tokens,
                                                            ..Default::default()
                                                        }),
                                                    })
                                                    .await;
                                            }
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                let _ = tx
                                    .send(StreamEvent::Error {
                                        message: e.to_string(),
                                    })
                                    .await;
                                return;
                            }
                        }
                    }
                }
                Err(e) => {
                    let _ = tx
                        .send(StreamEvent::Error {
                            message: e.to_string(),
                        })
                        .await;
                }
            }
        });

        Ok(CompletionStream::new(rx))
    }
}

impl OpenAi {
    async fn complete_via_responses(
        &self,
        request: CompletionRequest,
        model: String,
    ) -> Result<CompletionStream> {
        let input = build_responses_input(&request.messages);
        let mut body = serde_json::json!({
            "model": model,
            "input": input,
            "stream": true,
            "max_output_tokens": request.max_tokens,
        });

        if let Some(system) = &request.system {
            body["instructions"] = serde_json::json!(system);
        }

        if !request.tools.is_empty() {
            let tools: Vec<serde_json::Value> = request
                .tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "type": "function",
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    })
                })
                .collect();
            body["tools"] = serde_json::Value::Array(tools);
        }

        let url = format!("{}/responses", self.base_url);
        let auth_header = match &self.auth {
            Auth::ApiKey(key) | Auth::Bearer(key) => format!("Bearer {}", key),
            Auth::OAuth { token, .. } => format!("Bearer {}", token.access_token),
            Auth::Custom(_) => String::new(),
        };

        let (tx, rx) = mpsc::channel(256);
        let req = self
            .client
            .post(&url)
            .header("authorization", &auth_header)
            .header("content-type", "application/json")
            .json(&body)
            .build()
            .map_err(CerseiError::Http)?;

        let client = self.client.clone();

        tokio::spawn(async move {
            match client.execute(req).await {
                Ok(response) => {
                    if !response.status().is_success() {
                        let status = response.status().as_u16();
                        let body = response.text().await.unwrap_or_default();
                        let _ = tx
                            .send(StreamEvent::Error {
                                message: format!("HTTP {}: {}", status, body),
                            })
                            .await;
                        return;
                    }

                    let mut stream = response.bytes_stream();
                    let mut buffer = String::new();
                    let mut open_text_blocks = std::collections::HashSet::new();
                    let mut open_tool_blocks = std::collections::HashSet::new();
                    let mut stop_reason = StopReason::EndTurn;
                    let mut usage: Option<Usage> = None;

                    while let Some(chunk) = stream.next().await {
                        match chunk {
                            Ok(bytes) => {
                                buffer.push_str(&String::from_utf8_lossy(&bytes));
                                while let Some(pos) = buffer.find("\n") {
                                    let line = buffer[..pos].to_string();
                                    buffer = buffer[pos + 1..].to_string();

                                    if let Some(data) = line.strip_prefix("data: ") {
                                        let data = data.trim();
                                        if data == "[DONE]" {
                                            for index in open_text_blocks.drain() {
                                                let _ = tx
                                                    .send(StreamEvent::ContentBlockStop { index })
                                                    .await;
                                            }
                                            for index in open_tool_blocks.drain() {
                                                let _ = tx
                                                    .send(StreamEvent::ContentBlockStop { index })
                                                    .await;
                                            }
                                            let _ = tx
                                                .send(StreamEvent::MessageDelta {
                                                    stop_reason: Some(stop_reason),
                                                    usage,
                                                })
                                                .await;
                                            let _ = tx.send(StreamEvent::MessageStop).await;
                                            return;
                                        }

                                        if let Ok(json) =
                                            serde_json::from_str::<serde_json::Value>(data)
                                        {
                                            let event_type =
                                                json["type"].as_str().unwrap_or_default();
                                            match event_type {
                                                "response.created" => {
                                                    let id = json["response"]["id"]
                                                        .as_str()
                                                        .unwrap_or_default()
                                                        .to_string();
                                                    let model = json["response"]["model"]
                                                        .as_str()
                                                        .unwrap_or_default()
                                                        .to_string();
                                                    let _ = tx
                                                        .send(StreamEvent::MessageStart {
                                                            id,
                                                            model,
                                                        })
                                                        .await;
                                                }
                                                "response.output_item.added" => {
                                                    let index =
                                                        json["output_index"].as_u64().unwrap_or(0)
                                                            as usize;
                                                    let item = &json["item"];
                                                    match item["type"].as_str().unwrap_or_default()
                                                    {
                                                        "function_call" => {
                                                            open_tool_blocks.insert(index);
                                                            let id = item["call_id"]
                                                                .as_str()
                                                                .or_else(|| item["id"].as_str())
                                                                .unwrap_or_default()
                                                                .to_string();
                                                            let name = item["name"]
                                                                .as_str()
                                                                .unwrap_or_default()
                                                                .to_string();
                                                            let thought_signature = item
                                                                ["thought_signature"]
                                                                .as_str()
                                                                .map(|s| s.to_string());
                                                            let _ = tx.send(StreamEvent::ContentBlockStart {
                                                                index,
                                                                block_type: "tool_use".into(),
                                                                id: Some(id),
                                                                name: Some(name),
                                                                thought_signature,
                                                            }).await;
                                                        }
                                                        "message" => {
                                                            open_text_blocks.insert(index);
                                                            let _ = tx.send(StreamEvent::ContentBlockStart {
                                                                index,
                                                                block_type: "text".into(),
                                                                id: None,
                                                                name: None,
                                                                thought_signature: None,
                                                            }).await;
                                                        }
                                                        _ => {}
                                                    }
                                                }
                                                "response.output_text.delta" => {
                                                    let index =
                                                        json["output_index"].as_u64().unwrap_or(0)
                                                            as usize;
                                                    if !open_text_blocks.contains(&index) {
                                                        open_text_blocks.insert(index);
                                                        let _ = tx
                                                            .send(StreamEvent::ContentBlockStart {
                                                                index,
                                                                block_type: "text".into(),
                                                                id: None,
                                                                name: None,
                                                                thought_signature: None,
                                                            })
                                                            .await;
                                                    }
                                                    if let Some(delta) = json["delta"].as_str() {
                                                        let _ = tx
                                                            .send(StreamEvent::TextDelta {
                                                                index,
                                                                text: delta.to_string(),
                                                            })
                                                            .await;
                                                    }
                                                }
                                                "response.output_text.done" => {
                                                    let index =
                                                        json["output_index"].as_u64().unwrap_or(0)
                                                            as usize;
                                                    if open_text_blocks.remove(&index) {
                                                        let _ = tx
                                                            .send(StreamEvent::ContentBlockStop {
                                                                index,
                                                            })
                                                            .await;
                                                    }
                                                }
                                                "response.function_call_arguments.delta" => {
                                                    let index =
                                                        json["output_index"].as_u64().unwrap_or(0)
                                                            as usize;
                                                    if let Some(delta) = json["delta"].as_str() {
                                                        let _ = tx
                                                            .send(StreamEvent::InputJsonDelta {
                                                                index,
                                                                partial_json: delta.to_string(),
                                                            })
                                                            .await;
                                                    }
                                                }
                                                "response.function_call_arguments.done" => {
                                                    let index =
                                                        json["output_index"].as_u64().unwrap_or(0)
                                                            as usize;
                                                    stop_reason = StopReason::ToolUse;
                                                    if open_tool_blocks.remove(&index) {
                                                        let _ = tx
                                                            .send(StreamEvent::ContentBlockStop {
                                                                index,
                                                            })
                                                            .await;
                                                    }
                                                }
                                                "response.completed" => {
                                                    let output = json["response"]["output"]
                                                        .as_array()
                                                        .cloned()
                                                        .unwrap_or_default();
                                                    if output.iter().any(|item| {
                                                        item["type"].as_str()
                                                            == Some("function_call")
                                                    }) {
                                                        stop_reason = StopReason::ToolUse;
                                                    }

                                                    let usage_json = &json["response"]["usage"];
                                                    usage = Some(Usage {
                                                        input_tokens: usage_json["input_tokens"]
                                                            .as_u64()
                                                            .unwrap_or(0),
                                                        output_tokens: usage_json["output_tokens"]
                                                            .as_u64()
                                                            .unwrap_or(0),
                                                        total_tokens: usage_json["total_tokens"]
                                                            .as_u64()
                                                            .unwrap_or(0),
                                                        ..Default::default()
                                                    });
                                                }
                                                "response.failed" | "error" => {
                                                    let message = json["error"]["message"]
                                                        .as_str()
                                                        .or_else(|| json["message"].as_str())
                                                        .unwrap_or("Responses API error")
                                                        .to_string();
                                                    let _ = tx
                                                        .send(StreamEvent::Error { message })
                                                        .await;
                                                    return;
                                                }
                                                _ => {}
                                            }
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                let _ = tx
                                    .send(StreamEvent::Error {
                                        message: e.to_string(),
                                    })
                                    .await;
                                return;
                            }
                        }
                    }
                }
                Err(e) => {
                    let _ = tx
                        .send(StreamEvent::Error {
                            message: e.to_string(),
                        })
                        .await;
                }
            }
        });

        Ok(CompletionStream::new(rx))
    }
}

fn requires_responses_api(model: &str) -> bool {
    let lower = model.to_lowercase();
    lower.contains("codex") || lower.contains("-pro")
}

fn uses_responses_api(base_url: &str, model: &str) -> bool {
    base_url.trim_end_matches('/') == OPENAI_API_BASE && requires_responses_api(model)
}

fn build_responses_input(messages: &[Message]) -> Vec<serde_json::Value> {
    let mut input = Vec::new();

    for msg in messages {
        match &msg.content {
            MessageContent::Text(text) => {
                input.push(response_message_item(msg.role, text));
            }
            MessageContent::Blocks(blocks) => {
                let mut text_parts = Vec::new();
                for block in blocks {
                    match block {
                        ContentBlock::Text { text } => {
                            if !text.is_empty() {
                                text_parts.push(text.clone());
                            }
                        }
                        ContentBlock::ToolUse {
                            id,
                            name,
                            input: tool_input,
                            thought_signature,
                        } => {
                            let mut fc = serde_json::json!({
                                "type": "function_call",
                                "call_id": id,
                                "name": name,
                                "arguments": tool_input.to_string(),
                            });
                            if let Some(sig) = thought_signature {
                                fc["thought_signature"] = serde_json::json!(sig);
                            }
                            input.push(fc);
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } => {
                            let output = match content {
                                ToolResultContent::Text(text) => text.clone(),
                                ToolResultContent::Blocks(blocks) => {
                                    serde_json::to_string(blocks).unwrap_or_default()
                                }
                            };
                            input.push(serde_json::json!({
                                "type": "function_call_output",
                                "call_id": tool_use_id,
                                "output": output,
                            }));
                        }
                        _ => {}
                    }
                }

                if !text_parts.is_empty() {
                    input.push(response_message_item(msg.role, &text_parts.join("")));
                }
            }
        }
    }

    input
}

fn response_message_item(role: Role, text: &str) -> serde_json::Value {
    let content_type = if role == Role::Assistant {
        "output_text"
    } else {
        "input_text"
    };

    serde_json::json!({
        "type": "message",
        "role": match role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::System => "system",
        },
        "content": [{
            "type": content_type,
            "text": text,
        }],
    })
}

// ─── Builder ─────────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct OpenAiBuilder {
    api_key: Option<String>,
    base_url: Option<String>,
    model: Option<String>,
}

impl OpenAiBuilder {
    pub fn api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn build(self) -> Result<OpenAi> {
        let auth = if let Some(key) = self.api_key {
            Auth::ApiKey(key)
        } else {
            return Err(CerseiError::Auth(
                "No API key provided. Set OPENAI_API_KEY or use .api_key()".into(),
            ));
        };

        Ok(OpenAi {
            auth,
            base_url: self.base_url.unwrap_or_else(|| OPENAI_API_BASE.to_string()),
            default_model: self.model.unwrap_or_else(|| "gpt-4o".to_string()),
            client: reqwest::Client::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live_test_provider() -> Option<OpenAi> {
        OpenAi::from_env().ok()
    }

    #[test]
    fn google_pro_models_stay_on_chat_completions() {
        assert!(!uses_responses_api(
            "https://generativelanguage.googleapis.com/v1beta/openai",
            "gemini-3.1-pro-preview"
        ));
    }

    #[test]
    fn openai_pro_models_use_responses_api() {
        assert!(uses_responses_api(OPENAI_API_BASE, "gpt-5.4-pro"));
    }

    fn env_or_default(key: &str, default: &str) -> String {
        std::env::var(key).unwrap_or_else(|_| default.to_string())
    }

    async fn explain_pi(model: &str) -> std::result::Result<String, String> {
        let provider = match live_test_provider() {
            Some(provider) => provider,
            None => return Err("OPENAI_API_KEY not set".into()),
        };

        let mut request = CompletionRequest::new(model);
        request
            .messages
            .push(Message::user("Explain Pi in one sentence."));
        request.max_tokens = 128;

        let response = provider
            .complete_blocking(request)
            .await
            .map_err(|e| e.to_string())?;
        Ok(response.message.get_all_text())
    }

    fn assert_pi_response(text: &str) {
        let lower = text.to_lowercase();
        assert!(!text.trim().is_empty(), "response should not be empty");
        assert!(
            lower.contains("pi")
                || lower.contains("3.14")
                || lower.contains("ratio")
                || lower.contains("circle"),
            "expected a Pi-related answer, got: {text}"
        );
    }

    #[tokio::test]
    #[ignore = "requires OPENAI_API_KEY and a live OpenAI API call"]
    async fn live_gpt_5_4_pro_explains_pi() {
        let model = env_or_default("OPENAI_TEST_MODEL_PRO", "gpt-5.4-pro-2026-03-05");
        let text = explain_pi(&model)
            .await
            .unwrap_or_else(|err| panic!("live completion failed for {model}: {err}"));
        assert_pi_response(&text);
    }

    #[tokio::test]
    #[ignore = "requires OPENAI_API_KEY and a live OpenAI API call"]
    async fn live_gpt_5_4_explains_pi() {
        let model = env_or_default("OPENAI_TEST_MODEL_CHAT", "gpt-5.4");
        let text = explain_pi(&model)
            .await
            .unwrap_or_else(|err| panic!("live completion failed for {model}: {err}"));
        assert_pi_response(&text);
    }
}
