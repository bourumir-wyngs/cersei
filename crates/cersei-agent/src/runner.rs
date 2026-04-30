//! Agent runner: the core agentic loop.

use crate::events::{AgentControl, AgentEvent};
use crate::{Agent, AgentOutput, ToolCallRecord};
use cersei_hooks::{HookAction, HookContext, HookEvent};
use cersei_memory::session_storage;
use cersei_provider::{CompletionRequest, ProviderOptions, StreamAccumulator};
use cersei_tools::permissions::{PermissionDecision, PermissionRequest};
use cersei_tools::xfile_storage::{
    load_session_xfile_storage_from_path, save_session_xfile_storage_to_path,
};
use cersei_tools::{ToolContext, ToolResult};
use cersei_types::*;
use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

const END_TURN_SUMMARY_PROMPT: &str = "Briefly summarize your progress, results, and next steps.";
const AUTO_RECALL_LIMIT: usize = 3;
const MID_RUN_USER_MESSAGE_HEADER: &str =
    "User instruction received while you were working. Treat this as the latest user instruction and adjust before continuing:";
const SKIPPED_TOOL_RESULT: &str =
    "Tool skipped because the user provided new instructions before it ran.";

fn log_turn_handoff_to_human(reason: &str, summary_missing: bool, summary_prompt_sent: bool) {
    let timestamp = chrono::Local::now().to_rfc3339();
    println!(
        "[agent handoff] time={timestamp} reason={reason} summary_missing={summary_missing} summary_prompt_sent={summary_prompt_sent}"
    );
}
fn append_tool_usage_log(tool_id: &str, tool_name: &str, tool_input: &serde_json::Value) {
    let Some(home_dir) = dirs::home_dir() else {
        return;
    };
    let log_dir = home_dir.join(".abstract");
    let log_path = log_dir.join("tools.log");

    if fs::create_dir_all(&log_dir).is_err() {
        return;
    }

    let entry = serde_json::json!({
        "timestamp": chrono::Local::now().to_rfc3339(),
        "tool_id": tool_id,
        "tool_name": tool_name,
        "tool_input": tool_input,
    });

    let Ok(serialized) = serde_json::to_string(&entry) else {
        return;
    };

    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&log_path) {
        let _ = writeln!(file, "{serialized}");
    }
}

fn queue_injected_user_message(pending: &mut VecDeque<String>, message: String) {
    let message = message.trim();
    if !message.is_empty() {
        pending.push_back(message.to_string());
    }
}

fn handle_control_message(
    control: AgentControl,
    pending_user_messages: &mut VecDeque<String>,
) -> Result<()> {
    match control {
        AgentControl::Cancel => Err(CerseiError::Cancelled),
        AgentControl::InjectMessage(message) => {
            queue_injected_user_message(pending_user_messages, message);
            Ok(())
        }
        AgentControl::PermissionResponse { .. } => Ok(()),
    }
}

fn drain_control_messages(
    control_rx: &mut mpsc::Receiver<AgentControl>,
    pending_user_messages: &mut VecDeque<String>,
) -> Result<()> {
    loop {
        match control_rx.try_recv() {
            Ok(control) => handle_control_message(control, pending_user_messages)?,
            Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected) => {
                return Ok(());
            }
        }
    }
}

fn take_pending_user_message(pending_user_messages: &mut VecDeque<String>) -> Option<String> {
    if pending_user_messages.is_empty() {
        return None;
    }

    let messages: Vec<String> = pending_user_messages.drain(..).collect();
    let body = if messages.len() == 1 {
        messages[0].clone()
    } else {
        messages
            .iter()
            .enumerate()
            .map(|(idx, message)| format!("{}. {}", idx + 1, message))
            .collect::<Vec<_>>()
            .join("\n")
    };

    Some(format!("{MID_RUN_USER_MESSAGE_HEADER}\n\n{body}"))
}

fn push_pending_user_message(agent: &Agent, pending_user_messages: &mut VecDeque<String>) -> bool {
    let Some(text) = take_pending_user_message(pending_user_messages) else {
        return false;
    };
    agent.messages.lock().push(Message::user(text));
    true
}

fn append_pending_user_text_block(
    blocks: &mut Vec<ContentBlock>,
    pending_user_messages: &mut VecDeque<String>,
) -> bool {
    let Some(text) = take_pending_user_message(pending_user_messages) else {
        return false;
    };
    blocks.push(ContentBlock::Text { text });
    true
}

fn skipped_tool_result_block(tool_id: String) -> ContentBlock {
    ContentBlock::ToolResult {
        tool_use_id: tool_id,
        content: ToolResultContent::Text(SKIPPED_TOOL_RESULT.to_string()),
        is_error: Some(true),
    }
}

fn assistant_message_has_progress_summary(message: &Message) -> bool {
    let text = message.get_all_text();
    if text.trim().is_empty() {
        return false;
    }

    let lower = text.to_lowercase();
    if lower.contains("summary:")
        || lower.contains("progress:")
        || lower.contains("results:")
        || lower.contains("next steps:")
        || lower.contains("next:")
    {
        return true;
    }

    let bullets = text
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with("- ") || trimmed.starts_with("* ")
        })
        .count();
    if bullets >= 2 {
        return true;
    }

    let keywords = [
        "completed",
        "done",
        "implemented",
        "verified",
        "next step",
        "remaining",
        "progress",
    ];
    let mut matches = 0;
    for kw in &keywords {
        if lower.contains(kw) {
            matches += 1;
        }
    }
    matches >= 2
}

fn should_auto_recall_from_prompt(prompt: &str) -> bool {
    let bytes = prompt.as_bytes();
    let needle = b"again";

    bytes
        .windows(needle.len())
        .enumerate()
        .any(|(idx, window)| {
            if !window
                .iter()
                .zip(needle.iter())
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
            {
                return false;
            }

            let before_ok = idx
                .checked_sub(1)
                .and_then(|i| bytes.get(i))
                .is_none_or(|b| !b.is_ascii_alphabetic());
            let after_ok = bytes
                .get(idx + needle.len())
                .is_none_or(|b| !b.is_ascii_alphabetic());

            before_ok && after_ok
        })
}

async fn inject_auto_recalled_memories(agent: &Agent, prompt: &str) -> Result<()> {
    if !should_auto_recall_from_prompt(prompt) {
        return Ok(());
    }

    let Some(memory) = &agent.memory else {
        return Ok(());
    };

    let recalled = memory.search(prompt, AUTO_RECALL_LIMIT).await?;
    if recalled.is_empty() {
        return Ok(());
    }

    let mut note = String::from(
        "Relevant recalled memories triggered by the user's use of the word 'again':\n",
    );
    for (idx, entry) in recalled.iter().enumerate() {
        note.push_str(&format!(
            "{}. [{}] {}\n",
            idx + 1,
            entry.source,
            entry.content
        ));
    }

    agent.messages.lock().push(Message::system(note.trim_end()));
    Ok(())
}

// ─── Thinking block stripping ────────────────────────────────────────────────

/// Remove Thinking and RedactedThinking blocks from loaded session history.
///
/// Extended thinking content can be very large and is not needed when resuming
/// a session — the assistant's final text and tool calls provide sufficient
/// context. Stripping them prevents context window overflow on resume.
pub fn strip_thinking_blocks(messages: Vec<Message>) -> Vec<Message> {
    messages
        .into_iter()
        .map(|mut msg| {
            if let MessageContent::Blocks(blocks) = msg.content {
                let filtered: Vec<ContentBlock> = blocks
                    .into_iter()
                    .filter(|b| {
                        !matches!(
                            b,
                            ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. }
                        )
                    })
                    .collect();
                msg.content = MessageContent::Blocks(filtered);
            }
            msg
        })
        .collect()
}

fn xfile_storage_sidecar_path(agent: &Agent, session_id: &str) -> std::path::PathBuf {
    session_storage::xfile_storage_path(&agent.working_dir, session_id)
}

fn restore_local_session_state(agent: &Agent, session_id: &str) -> Result<()> {
    let path = xfile_storage_sidecar_path(agent, session_id);
    load_session_xfile_storage_from_path(session_id, &path)
        .map(|_| ())
        .map_err(CerseiError::Config)
}

fn persist_local_session_state(agent: &Agent, session_id: &str) -> Result<()> {
    let path = xfile_storage_sidecar_path(agent, session_id);
    save_session_xfile_storage_to_path(session_id, &path)
        .map(|_| ())
        .map_err(CerseiError::Config)
}

// ─── Tool result budget ──────────────────────────────────────────────────────

const TOOL_RESULT_MIN_TRUNCATABLE_CHARS: usize = 200;
const TOOL_RESULT_PREVIEW_CHAR_LIMIT: usize = 160;
const TOOL_RESULT_PREVIEW_LINE_LIMIT: usize = 8;
const TOOL_RESULT_TRUNCATION_MARKER: &str = "[truncated in context:";
const MAX_TOOL_RESULT_CHARS: usize = 250_000;

/// Truncate oldest tool results when cumulative size exceeds budget.
/// Modifies messages in place.
pub fn apply_tool_result_budget(messages: &mut [Message], budget_chars: usize) {
    // Collect total tool result size
    let total: usize = messages
        .iter()
        .flat_map(|m| match &m.content {
            MessageContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| {
                    if let ContentBlock::ToolResult { content, .. } = b {
                        Some(match content {
                            ToolResultContent::Text(t) => t.len(),
                            ToolResultContent::Blocks(b) => b
                                .iter()
                                .map(|bb| {
                                    if let ContentBlock::Text { text } = bb {
                                        text.len()
                                    } else {
                                        0
                                    }
                                })
                                .sum(),
                        })
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>(),
            _ => vec![],
        })
        .sum();

    if total <= budget_chars {
        return;
    }

    // Truncate oldest tool results first (skip the last KEEP_RECENT messages)
    let keep_recent = 6; // don't touch recent tool results
    let truncatable_end = messages.len().saturating_sub(keep_recent);
    let mut freed = 0usize;
    let target_free = total - budget_chars;

    for msg in messages[..truncatable_end].iter_mut() {
        if freed >= target_free {
            break;
        }
        if let MessageContent::Blocks(blocks) = &mut msg.content {
            for block in blocks.iter_mut() {
                if freed >= target_free {
                    break;
                }
                if let ContentBlock::ToolResult { content, .. } = block {
                    if let ToolResultContent::Text(text) = content {
                        let size = text.len();
                        if size <= TOOL_RESULT_MIN_TRUNCATABLE_CHARS
                            || text.contains(TOOL_RESULT_TRUNCATION_MARKER)
                        {
                            continue;
                        }

                        let replacement = truncate_tool_result_preview(text);
                        let freed_now = size.saturating_sub(replacement.len());
                        if freed_now == 0 {
                            continue;
                        }

                        *text = replacement;
                        freed += freed_now;
                    }
                }
            }
        }
    }
}

fn truncate_tool_result_preview(text: &str) -> String {
    let preview = text_preview(
        text,
        TOOL_RESULT_PREVIEW_LINE_LIMIT,
        TOOL_RESULT_PREVIEW_CHAR_LIMIT,
    );
    let omitted = text.chars().count().saturating_sub(preview.chars().count());

    format!("{preview}\n\n{TOOL_RESULT_TRUNCATION_MARKER} {omitted} chars omitted]")
}

fn text_preview(text: &str, max_lines: usize, max_chars: usize) -> String {
    let mut preview = text.lines().take(max_lines).collect::<Vec<_>>().join("\n");
    if preview.chars().count() > max_chars {
        preview = preview.chars().take(max_chars).collect();
    }
    preview.trim_end().to_string()
}

/// Run the agent without streaming (blocking until complete).
pub async fn run_agent(agent: &Agent, prompt: &str) -> Result<AgentOutput> {
    let (event_tx, _event_rx) = mpsc::channel(512);
    let (_control_tx, control_rx) = mpsc::channel(64);

    let prompt = prompt.to_string();

    // Run in a background task and collect events
    let result = run_agent_streaming(agent, &prompt, event_tx, control_rx).await;

    match result {
        Ok(output) => {
            agent.emit(AgentEvent::Complete(output.clone()));
            Ok(output)
        }
        Err(e) => {
            agent.emit(AgentEvent::Error(e.to_string()));
            Err(e)
        }
    }
}

/// Core agentic loop with streaming events.
pub async fn run_agent_streaming(
    agent: &Agent,
    prompt: &str,
    event_tx: mpsc::Sender<AgentEvent>,
    mut control_rx: mpsc::Receiver<AgentControl>,
) -> Result<AgentOutput> {
    // Load session history (skip if messages were pre-populated via with_messages)
    if agent.messages.lock().is_empty() {
        if let (Some(memory), Some(session_id)) = (&agent.memory, &agent.session_id) {
            let history = strip_thinking_blocks(memory.load(session_id).await?);
            restore_local_session_state(agent, session_id)?;
            if !history.is_empty() {
                let count = history.len();
                agent.messages.lock().extend(history);
                let _ = event_tx
                    .send(AgentEvent::SessionLoaded {
                        session_id: session_id.clone(),
                        message_count: count,
                    })
                    .await;
                agent.emit(AgentEvent::SessionLoaded {
                    session_id: session_id.clone(),
                    message_count: count,
                });
            }
        }
    } // end session load guard

    inject_auto_recalled_memories(agent, prompt).await?;

    // Add user prompt
    agent.messages.lock().push(Message::user(prompt));

    let mut tool_calls: Vec<ToolCallRecord> = Vec::new();
    let mut turn: u32 = 0;
    let mut last_stop_reason = StopReason::EndTurn;
    let mut _last_usage = Usage::default();
    let mut auto_summary_requested = false;
    let mut pending_user_messages = VecDeque::new();
    let mut control_open = true;

    // Build tool context
    let tool_ctx = ToolContext {
        working_dir: agent.working_dir.clone(),
        session_id: agent
            .session_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        permissions: Arc::clone(&agent.permission_policy),
        cost_tracker: Arc::clone(&agent.cost_tracker),
        mcp_manager: agent.mcp_manager.clone(),
        extensions: agent.tool_extensions.clone(),
        network_policy: agent.network_policy.clone(),
    };

    // Agentic loop
    loop {
        turn += 1;
        if turn > agent.max_turns {
            if auto_summary_requested {
                log_turn_handoff_to_human("max_turns_reached", true, true);
                break;
            }

            agent
                .messages
                .lock()
                .push(Message::user(END_TURN_SUMMARY_PROMPT));
            auto_summary_requested = true;
            turn = agent.max_turns;
            log_turn_handoff_to_human("max_turns_reached", true, true);
            continue;
        }

        // Check cancellation
        if agent.cancel_token.is_cancelled() {
            return Err(CerseiError::Cancelled);
        }
        drain_control_messages(&mut control_rx, &mut pending_user_messages)?;
        push_pending_user_message(agent, &mut pending_user_messages);

        let _ = event_tx.send(AgentEvent::TurnStart { turn }).await;
        agent.emit(AgentEvent::TurnStart { turn });

        // Apply tool result budget before sending
        {
            let mut msgs = agent.messages.lock();
            apply_tool_result_budget(&mut msgs, agent.tool_result_budget);
        }

        // Build completion request
        let messages = agent.messages.lock().clone();
        let tool_defs: Vec<ToolDefinition> =
            agent.tools.iter().map(|t| t.to_definition()).collect();

        let model = agent
            .model
            .clone()
            .unwrap_or_else(|| "claude-sonnet-4-6".to_string());

        let mut options = ProviderOptions::default();
        if let Some(budget) = agent.thinking_budget {
            options.set("thinking_budget", budget);
        }

        let request = CompletionRequest {
            model: model.clone(),
            messages: messages.clone(),
            system: agent.system_prompt.clone(),
            tools: tool_defs,
            max_tokens: agent.max_tokens,
            temperature: agent.temperature,
            stop_sequences: Vec::new(),
            options,
        };

        // Debug: print request when CERSEI_DEBUG_REQUEST is set
        if std::env::var("CERSEI_DEBUG_REQUEST").is_ok() {
            eprintln!(
                "\n\x1b[90m─── REQUEST (turn {}) ───────────────────────────────\x1b[0m",
                turn
            );
            eprintln!("\x1b[90mModel: {}\x1b[0m", request.model);
            let sys_chars = request.system.as_deref().map(|s| s.len()).unwrap_or(0);
            eprintln!(
                "\x1b[90mSystem prompt: {} chars (~{} tokens)\x1b[0m",
                sys_chars,
                sys_chars / 4
            );
            let tools_json = serde_json::to_string(&request.tools).unwrap_or_default();
            eprintln!(
                "\x1b[90mTools: {} ({} chars, ~{} tokens)\x1b[0m",
                request.tools.len(),
                tools_json.len(),
                tools_json.len() / 4
            );
            eprintln!("\x1b[90mMessages: {}\x1b[0m", request.messages.len());
            let mut msg_total_chars = 0usize;
            for (i, msg) in request.messages.iter().enumerate() {
                let full = serde_json::to_string(&msg.content).unwrap_or_default();
                msg_total_chars += full.len();
                let preview_src = msg.get_all_text();
                let preview_src = if preview_src.trim().is_empty() {
                    &full
                } else {
                    &preview_src
                };
                let preview: String = preview_src.chars().take(100).collect();
                let ellipsis = if preview_src.len() > 100 { "…" } else { "" };
                eprintln!(
                    "\x1b[90m  [{}] {:?} ({} chars): {}{}\x1b[0m",
                    i,
                    msg.role,
                    full.len(),
                    preview,
                    ellipsis
                );
            }
            let total_chars = sys_chars + tools_json.len() + msg_total_chars;
            eprintln!(
                "\x1b[90mTotal: ~{} tokens (sys={} tools={} msgs={})\x1b[0m",
                total_chars / 4,
                sys_chars / 4,
                tools_json.len() / 4,
                msg_total_chars / 4
            );
            eprintln!("\x1b[90m─────────────────────────────────────────────────────\x1b[0m\n");
        }

        // Use last known input token count from API, fall back to rough estimate
        let token_estimate = {
            let usage = agent.cumulative_usage.lock().clone();
            if usage.input_tokens > 0 {
                usage.input_tokens
            } else {
                crate::compact::estimate_messages_tokens(&messages)
            }
        };

        let _ = event_tx
            .send(AgentEvent::ModelRequestStart {
                turn,
                message_count: messages.len(),
                token_estimate,
            })
            .await;

        // Send to provider
        let stream = agent.provider.complete(request).await?;
        let mut rx = stream.into_receiver();
        let mut accumulator = StreamAccumulator::new();

        let _ = event_tx
            .send(AgentEvent::ModelResponseStart {
                turn,
                model: model.clone(),
            })
            .await;

        // Process stream events
        loop {
            // Check cancellation during streaming
            if agent.cancel_token.is_cancelled() {
                return Err(CerseiError::Cancelled);
            }

            tokio::select! {
                control = control_rx.recv(), if control_open => {
                    match control {
                        Some(control) => {
                            handle_control_message(control, &mut pending_user_messages)?;
                        }
                        None => {
                            control_open = false;
                        }
                    }
                }
                maybe_event = rx.recv() => {
                    let Some(event) = maybe_event else {
                        break;
                    };
                    match &event {
                        StreamEvent::TextDelta { text, .. } => {
                            let _ = event_tx.send(AgentEvent::TextDelta(text.clone())).await;
                            agent.emit(AgentEvent::TextDelta(text.clone()));
                        }
                        StreamEvent::ThinkingDelta { thinking, .. } => {
                            let _ = event_tx
                                .send(AgentEvent::ThinkingDelta(thinking.clone()))
                                .await;
                            agent.emit(AgentEvent::ThinkingDelta(thinking.clone()));
                        }
                        StreamEvent::Error { message } => {
                            return Err(CerseiError::Provider(message.clone()));
                        }
                        _ => {}
                    }
                    accumulator.process_event(event);
                }
            }
        }
        drain_control_messages(&mut control_rx, &mut pending_user_messages)?;

        // Convert accumulated response
        let response = accumulator.into_response()?;
        last_stop_reason = response.stop_reason.clone();
        _last_usage = response.usage.clone();

        // Update cumulative usage
        agent.cumulative_usage.lock().merge(&response.usage);
        agent.cost_tracker.add(&response.usage);

        // Emit cost update
        let cumulative = agent.cumulative_usage.lock().clone();
        let _ = event_tx
            .send(AgentEvent::CostUpdate {
                turn_cost: response.usage.cost_usd.unwrap_or(0.0),
                cumulative_cost: cumulative.cost_usd.unwrap_or(0.0),
                input_tokens: cumulative.input_tokens,
                output_tokens: cumulative.output_tokens,
            })
            .await;
        agent.emit(AgentEvent::CostUpdate {
            turn_cost: response.usage.cost_usd.unwrap_or(0.0),
            cumulative_cost: cumulative.cost_usd.unwrap_or(0.0),
            input_tokens: cumulative.input_tokens,
            output_tokens: cumulative.output_tokens,
        });

        // Add assistant message to history
        agent.messages.lock().push(response.message.clone());

        // Fire PostModelTurn hooks
        let hook_ctx = HookContext {
            event: HookEvent::PostModelTurn,
            tool_name: None,
            tool_input: None,
            tool_result: None,
            tool_is_error: None,
            turn,
            cumulative_cost_usd: cumulative.cost_usd.unwrap_or(0.0),
            message_count: agent.messages.lock().len(),
        };
        let hook_action = cersei_hooks::run_hooks(&agent.hooks, &hook_ctx).await;
        if let HookAction::Block(reason) = hook_action {
            return Err(CerseiError::Provider(format!(
                "Blocked by hook: {}",
                reason
            )));
        }

        let _ = event_tx
            .send(AgentEvent::TurnComplete {
                turn,
                stop_reason: response.stop_reason.clone(),
                usage: response.usage.clone(),
            })
            .await;
        agent.emit(AgentEvent::TurnComplete {
            turn,
            stop_reason: response.stop_reason.clone(),
            usage: response.usage.clone(),
        });

        // Handle stop reason
        match &response.stop_reason {
            StopReason::EndTurn => {
                if push_pending_user_message(agent, &mut pending_user_messages) {
                    continue;
                }

                if !assistant_message_has_progress_summary(&response.message) {
                    if !auto_summary_requested {
                        agent
                            .messages
                            .lock()
                            .push(Message::user(END_TURN_SUMMARY_PROMPT));
                        auto_summary_requested = true;
                        log_turn_handoff_to_human("missing_summary_on_end_turn", true, true);
                        continue;
                    }

                    log_turn_handoff_to_human("missing_summary_on_end_turn", true, true);
                    break;
                }
                log_turn_handoff_to_human("end_turn", false, auto_summary_requested);
                break;
            }
            StopReason::ToolUse => {
                // Process tool calls
                let tool_use_blocks: Vec<(String, String, serde_json::Value)> = response
                    .message
                    .content_blocks()
                    .into_iter()
                    .filter_map(|b| {
                        if let ContentBlock::ToolUse {
                            id, name, input, ..
                        } = b
                        {
                            Some((id, name, input))
                        } else {
                            None
                        }
                    })
                    .collect();

                let mut result_blocks: Vec<ContentBlock> = Vec::new();

                if !pending_user_messages.is_empty() {
                    for (tool_id, tool_name, tool_input) in tool_use_blocks {
                        result_blocks.push(skipped_tool_result_block(tool_id.clone()));
                        tool_calls.push(ToolCallRecord {
                            name: tool_name,
                            id: tool_id,
                            input: tool_input,
                            result: SKIPPED_TOOL_RESULT.to_string(),
                            is_error: true,
                            duration: Duration::ZERO,
                        });
                    }
                    append_pending_user_text_block(&mut result_blocks, &mut pending_user_messages);
                    agent
                        .messages
                        .lock()
                        .push(Message::user_blocks(result_blocks));
                    continue;
                }

                for idx in 0..tool_use_blocks.len() {
                    let (tool_id, tool_name, tool_input) = tool_use_blocks[idx].clone();
                    // Check cancellation before each tool execution
                    if agent.cancel_token.is_cancelled() {
                        return Err(CerseiError::Cancelled);
                    }
                    drain_control_messages(&mut control_rx, &mut pending_user_messages)?;
                    if !pending_user_messages.is_empty() {
                        for (remaining_id, remaining_name, remaining_input) in
                            tool_use_blocks[idx..].iter().cloned()
                        {
                            result_blocks.push(skipped_tool_result_block(remaining_id.clone()));
                            tool_calls.push(ToolCallRecord {
                                name: remaining_name,
                                id: remaining_id,
                                input: remaining_input,
                                result: SKIPPED_TOOL_RESULT.to_string(),
                                is_error: true,
                                duration: Duration::ZERO,
                            });
                        }
                        break;
                    }

                    let _ = event_tx
                        .send(AgentEvent::ToolStart {
                            name: tool_name.clone(),
                            id: tool_id.clone(),
                            input: tool_input.clone(),
                        })
                        .await;
                    agent.emit(AgentEvent::ToolStart {
                        name: tool_name.clone(),
                        id: tool_id.clone(),
                        input: tool_input.clone(),
                    });

                    append_tool_usage_log(&tool_id, &tool_name, &tool_input);
                    let start = Instant::now();

                    // Find the tool
                    let tool = agent.tools.iter().find(|t| t.name() == tool_name);

                    let exec_tool_id = tool_id.clone();
                    let exec_tool_name = tool_name.clone();
                    let exec_tool_input = tool_input.clone();
                    let tool_execution = async {
                        if let Some(tool) = tool {
                            if let Some(preflight_result) =
                                tool.preflight(&exec_tool_input, &tool_ctx)
                            {
                                preflight_result
                            } else {
                                // Check permissions
                                let perm_req = PermissionRequest {
                                    tool_name: exec_tool_name.clone(),
                                    tool_input: exec_tool_input.clone(),
                                    permission_level: tool.permission_level(),
                                    description: format!("Execute tool '{}'", exec_tool_name),
                                    id: exec_tool_id,
                                    working_dir: tool_ctx.working_dir.clone(),
                                };

                                let decision = agent.permission_policy.check(&perm_req).await;

                                match decision {
                                    PermissionDecision::Allow
                                    | PermissionDecision::AllowOnce
                                    | PermissionDecision::AllowForSession => {
                                        // Fire PreToolUse hooks
                                        let hook_ctx = HookContext {
                                            event: HookEvent::PreToolUse,
                                            tool_name: Some(exec_tool_name.clone()),
                                            tool_input: Some(exec_tool_input.clone()),
                                            tool_result: None,
                                            tool_is_error: None,
                                            turn,
                                            cumulative_cost_usd: cumulative.cost_usd.unwrap_or(0.0),
                                            message_count: agent.messages.lock().len(),
                                        };
                                        let hook_action =
                                            cersei_hooks::run_hooks(&agent.hooks, &hook_ctx).await;

                                        match hook_action {
                                            HookAction::Block(reason) => ToolResult::error(
                                                format!("Blocked by hook: {}", reason),
                                            ),
                                            HookAction::ModifyInput(new_input) => {
                                                tool.execute(new_input, &tool_ctx).await
                                            }
                                            _ => {
                                                tool.execute(exec_tool_input.clone(), &tool_ctx)
                                                    .await
                                            }
                                        }
                                    }
                                    PermissionDecision::Deny(reason) => {
                                        ToolResult::error(format!("Permission denied: {}", reason))
                                    }
                                }
                            }
                        } else {
                            ToolResult::error(format!("Unknown tool: {}", exec_tool_name))
                        }
                    };
                    tokio::pin!(tool_execution);

                    let result = loop {
                        if agent.cancel_token.is_cancelled() {
                            return Err(CerseiError::Cancelled);
                        }
                        tokio::select! {
                            control = control_rx.recv(), if control_open => {
                                match control {
                                    Some(control) => {
                                        handle_control_message(control, &mut pending_user_messages)?;
                                    }
                                    None => {
                                        control_open = false;
                                    }
                                }
                            }
                            result = &mut tool_execution => {
                                break result;
                            }
                        }
                    };

                    let duration = start.elapsed();

                    let _ = event_tx
                        .send(AgentEvent::ToolEnd {
                            name: tool_name.clone(),
                            id: tool_id.clone(),
                            result: result.content.clone(),
                            is_error: result.is_error,
                            duration,
                        })
                        .await;
                    agent.emit(AgentEvent::ToolEnd {
                        name: tool_name.clone(),
                        id: tool_id.clone(),
                        result: result.content.clone(),
                        is_error: result.is_error,
                        duration,
                    });

                    tool_calls.push(ToolCallRecord {
                        name: tool_name.clone(),
                        id: tool_id.clone(),
                        input: tool_input,
                        result: result.content.clone(),
                        is_error: result.is_error,
                        duration,
                    });

                    // Cap individual tool result size to avoid single-result context overflow.
                    // ~250k chars ≈ ~62k tokens, which is still workable on modern large-context models.
                    let content_text = if result.content.len() > MAX_TOOL_RESULT_CHARS {
                        let truncated = &result.content[..MAX_TOOL_RESULT_CHARS];
                        format!(
                            "{}\n\n[...truncated: {} chars omitted]",
                            truncated,
                            result.content.len() - MAX_TOOL_RESULT_CHARS
                        )
                    } else {
                        result.content
                    };
                    result_blocks.push(ContentBlock::ToolResult {
                        tool_use_id: tool_id,
                        content: ToolResultContent::Text(content_text),
                        is_error: Some(result.is_error),
                    });

                    drain_control_messages(&mut control_rx, &mut pending_user_messages)?;
                    if !pending_user_messages.is_empty() {
                        for (remaining_id, remaining_name, remaining_input) in
                            tool_use_blocks[idx + 1..].iter().cloned()
                        {
                            result_blocks.push(skipped_tool_result_block(remaining_id.clone()));
                            tool_calls.push(ToolCallRecord {
                                name: remaining_name,
                                id: remaining_id,
                                input: remaining_input,
                                result: SKIPPED_TOOL_RESULT.to_string(),
                                is_error: true,
                                duration: Duration::ZERO,
                            });
                        }
                        break;
                    }
                }

                append_pending_user_text_block(&mut result_blocks, &mut pending_user_messages);
                // Add tool results as user message
                agent
                    .messages
                    .lock()
                    .push(Message::user_blocks(result_blocks));
            }
            StopReason::MaxTokens => {
                if push_pending_user_message(agent, &mut pending_user_messages) {
                    continue;
                }

                // Inject continuation message
                if !auto_summary_requested {
                    agent
                        .messages
                        .lock()
                        .push(Message::user("Summarize the current status"));
                    auto_summary_requested = true;
                }
                agent
                    .messages
                    .lock()
                    .push(Message::user("Continue from where you left off."));
            }
            _ => {
                if push_pending_user_message(agent, &mut pending_user_messages) {
                    continue;
                }

                let reason = format!("{:?}", response.stop_reason);
                let summary_prompt_sent = auto_summary_requested;
                log_turn_handoff_to_human(&reason, false, summary_prompt_sent);
                break;
            }
        }
    }

    // Persist session
    if let (Some(memory), Some(session_id)) = (&agent.memory, &agent.session_id) {
        let messages = agent.messages.lock().clone();
        memory.store(session_id, &messages).await?;
        persist_local_session_state(agent, session_id)?;
        let _ = event_tx
            .send(AgentEvent::SessionSaved {
                session_id: session_id.clone(),
            })
            .await;
        agent.emit(AgentEvent::SessionSaved {
            session_id: session_id.clone(),
        });
    }

    // Build output
    let last_message = agent
        .messages
        .lock()
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .cloned()
        .unwrap_or_else(|| Message::assistant(""));

    let output = AgentOutput {
        message: last_message,
        usage: agent.cumulative_usage.lock().clone(),
        stop_reason: last_stop_reason,
        turns: turn,
        tool_calls,
    };

    // Notify reporters
    for reporter in &agent.reporters {
        reporter.on_complete(&output).await;
    }

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use cersei_provider::{CompletionRequest, CompletionStream, Provider, ProviderCapabilities};
    use cersei_tools::permissions::{PermissionPolicy, PermissionRequest};
    use cersei_tools::PermissionLevel;
    use cersei_tools::{Tool, ToolCategory};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::sync::Notify;

    struct TwoStepProvider {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Provider for TwoStepProvider {
        fn name(&self) -> &str {
            "test"
        }

        fn context_window(&self, _model: &str) -> u64 {
            4096
        }

        fn capabilities(&self, _model: &str) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                tool_use: true,
                ..Default::default()
            }
        }

        async fn complete(&self, _request: CompletionRequest) -> Result<CompletionStream> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let (tx, rx) = mpsc::channel(16);

            tokio::spawn(async move {
                let _ = tx
                    .send(StreamEvent::MessageStart {
                        id: format!("msg-{call}"),
                        model: "test".into(),
                    })
                    .await;

                if call == 0 {
                    let input = json!({ "value": "bad" });
                    let _ = tx
                        .send(StreamEvent::ContentBlockStart {
                            index: 0,
                            block_type: "tool_use".into(),
                            id: Some("tool-1".into()),
                            name: Some("PreflightTool".into()),
                            thought_signature: None,
                        })
                        .await;
                    let _ = tx
                        .send(StreamEvent::InputJsonDelta {
                            index: 0,
                            partial_json: serde_json::to_string(&input).unwrap(),
                        })
                        .await;
                    let _ = tx.send(StreamEvent::ContentBlockStop { index: 0 }).await;
                    let _ = tx
                        .send(StreamEvent::MessageDelta {
                            stop_reason: Some(StopReason::ToolUse),
                            usage: Some(Usage::default()),
                        })
                        .await;
                } else {
                    let _ = tx
                        .send(StreamEvent::ContentBlockStart {
                            index: 0,
                            block_type: "text".into(),
                            id: None,
                            name: None,
                            thought_signature: None,
                        })
                        .await;
                    let _ = tx
                        .send(StreamEvent::TextDelta {
                            index: 0,
                            text: "done".into(),
                        })
                        .await;
                    let _ = tx.send(StreamEvent::ContentBlockStop { index: 0 }).await;
                    let _ = tx
                        .send(StreamEvent::MessageDelta {
                            stop_reason: Some(StopReason::EndTurn),
                            usage: Some(Usage::default()),
                        })
                        .await;
                }

                let _ = tx.send(StreamEvent::MessageStop).await;
            });

            Ok(CompletionStream::new(rx))
        }
    }

    struct CountingPermissions {
        checks: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl PermissionPolicy for CountingPermissions {
        async fn check(&self, _request: &PermissionRequest) -> PermissionDecision {
            self.checks.fetch_add(1, Ordering::SeqCst);
            PermissionDecision::Allow
        }
    }

    #[test]
    fn auto_recall_trigger_matches_whole_word_case_insensitively() {
        assert!(should_auto_recall_from_prompt("please check again"));
        assert!(should_auto_recall_from_prompt("Again, this broke"));
        assert!(should_auto_recall_from_prompt("(AGAIN)?"));
        assert!(!should_auto_recall_from_prompt(
            "This is against expectations"
        ));
        assert!(!should_auto_recall_from_prompt("bargain"));
        assert!(!should_auto_recall_from_prompt("A gain of confidence"));
    }

    fn tool_result_message(tool_use_id: &str, content: impl Into<String>) -> Message {
        Message::user_blocks(vec![ContentBlock::ToolResult {
            tool_use_id: tool_use_id.to_string(),
            content: ToolResultContent::Text(content.into()),
            is_error: Some(false),
        }])
    }

    fn tool_result_text(message: &Message) -> &str {
        match &message.content {
            MessageContent::Blocks(blocks) => match &blocks[0] {
                ContentBlock::ToolResult {
                    content: ToolResultContent::Text(text),
                    ..
                } => text,
                other => panic!("expected text tool result, got {other:?}"),
            },
            other => panic!("expected block message, got {other:?}"),
        }
    }

    #[test]
    fn apply_tool_result_budget_keeps_spreadsheet_preview_when_truncating() {
        let mut spreadsheet_result = String::from(
            "Spreadsheet: /workspace/data/matpower_network.xlsx\n\
Sheet: bus_names_coordinates\n\
Rows: 1..5\n\n\
bus_i,name,lat,lon,base_kv,zone\n",
        );
        for idx in 1..=20 {
            spreadsheet_result.push_str(&format!("{idx},Bus {idx},46.{idx},7.{idx},380,CH\n"));
        }

        let mut messages = vec![
            Message::user("inspect spreadsheet"),
            tool_result_message("tool-1", spreadsheet_result),
            Message::user("keep recent 1"),
            tool_result_message("tool-2", "x".repeat(1_200)),
            Message::user("keep recent 2"),
            Message::assistant("not a tool result"),
            Message::user("keep recent 3"),
            tool_result_message("tool-3", "y".repeat(1_200)),
        ];

        apply_tool_result_budget(&mut messages, 500);

        let truncated = tool_result_text(&messages[1]);
        assert!(truncated.contains("Spreadsheet: /workspace/data/matpower_network.xlsx"));
        assert!(truncated.contains("Sheet: bus_names_coordinates"));
        assert!(truncated.contains("bus_i,name,lat,lon,base_kv,zone"));
        assert!(truncated.contains(TOOL_RESULT_TRUNCATION_MARKER));
        assert!(!truncated.contains("[truncated — re-read file if needed]"));
    }

    #[test]
    fn apply_tool_result_budget_keeps_recent_tool_results_intact() {
        let older = "older result ".repeat(80);
        let recent_spreadsheet = "Spreadsheet: /workspace/data/TYNDP.xlsx\n\
Sheet: bus_mod\n\
Rows: 1..8\n\n\
bus_id,bus_name,country,voltage\n\
1,GENEVA,CH,380\n\
2,LAUSANNE,CH,220\n"
            .repeat(10);
        let recent_snapshot = recent_spreadsheet.clone();

        let mut messages = vec![
            Message::user("old request"),
            tool_result_message("tool-1", older),
            Message::user("old request 2"),
            tool_result_message("tool-2", "z".repeat(1_000)),
            Message::user("recent 1"),
            Message::assistant("recent 2"),
            Message::user("recent 3"),
            tool_result_message("tool-3", recent_spreadsheet),
            Message::assistant("recent 4"),
            Message::user("recent 5"),
        ];

        apply_tool_result_budget(&mut messages, 450);

        assert!(tool_result_text(&messages[1]).contains(TOOL_RESULT_TRUNCATION_MARKER));
        assert_eq!(tool_result_text(&messages[7]), recent_snapshot);
    }

    #[test]
    fn apply_tool_result_budget_is_idempotent_for_already_truncated_previews() {
        let large = "Spreadsheet: /workspace/data/matpower_network.xlsx\n\
Sheet: bus\n\
Rows: 1..20\n\n\
col_a,col_b,col_c,col_d\n"
            .to_string()
            + &"1,2,3,4\n".repeat(40);

        let mut messages = vec![
            Message::user("old request"),
            tool_result_message("tool-1", large),
            Message::user("recent 1"),
            Message::assistant("recent 2"),
            Message::user("recent 3"),
            Message::assistant("recent 4"),
            Message::user("recent 5"),
            Message::assistant("recent 6"),
        ];

        apply_tool_result_budget(&mut messages, 200);
        let once = tool_result_text(&messages[1]).to_string();
        apply_tool_result_budget(&mut messages, 150);

        assert_eq!(tool_result_text(&messages[1]), once);
        assert_eq!(
            tool_result_text(&messages[1])
                .matches(TOOL_RESULT_TRUNCATION_MARKER)
                .count(),
            1
        );
    }

    struct PreflightTool {
        executed: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Tool for PreflightTool {
        fn name(&self) -> &str {
            "PreflightTool"
        }

        fn description(&self) -> &str {
            "Tool used to verify preflight ordering."
        }

        fn input_schema(&self) -> serde_json::Value {
            json!({
                "type": "object",
                "properties": {
                    "value": { "type": "string" }
                }
            })
        }

        fn permission_level(&self) -> PermissionLevel {
            PermissionLevel::Execute
        }

        fn category(&self) -> ToolCategory {
            ToolCategory::Custom
        }

        fn preflight(&self, _input: &serde_json::Value, _ctx: &ToolContext) -> Option<ToolResult> {
            Some(ToolResult::error("preflight blocked"))
        }

        async fn execute(&self, _input: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
            self.executed.fetch_add(1, Ordering::SeqCst);
            ToolResult::success("should not execute")
        }
    }

    #[tokio::test]
    async fn preflight_blocks_before_permission_checks() {
        let permission_checks = Arc::new(AtomicUsize::new(0));
        let executed = Arc::new(AtomicUsize::new(0));

        let agent = Agent::builder()
            .provider(TwoStepProvider {
                calls: AtomicUsize::new(0),
            })
            .tool(PreflightTool {
                executed: Arc::clone(&executed),
            })
            .permission_policy(CountingPermissions {
                checks: Arc::clone(&permission_checks),
            })
            .working_dir(std::env::temp_dir())
            .model("test-model")
            .max_turns(4)
            .build()
            .unwrap();

        let output = run_agent(&agent, "test").await.unwrap();

        assert_eq!(permission_checks.load(Ordering::SeqCst), 0);
        assert_eq!(executed.load(Ordering::SeqCst), 0);
        assert_eq!(output.tool_calls.len(), 1);
        assert!(output.tool_calls[0].is_error);
        assert!(output.tool_calls[0].result.contains("preflight blocked"));
    }

    struct CountingTool {
        name: &'static str,
        executed: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Tool for CountingTool {
        fn name(&self) -> &str {
            self.name
        }

        fn description(&self) -> &str {
            "Tool used to verify injected-message control flow."
        }

        fn input_schema(&self) -> serde_json::Value {
            json!({
                "type": "object",
                "properties": {}
            })
        }

        fn permission_level(&self) -> PermissionLevel {
            PermissionLevel::None
        }

        async fn execute(&self, _input: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
            self.executed.fetch_add(1, Ordering::SeqCst);
            ToolResult::success(format!("{} result", self.name))
        }
    }

    struct BlockingTool {
        finish: Arc<Notify>,
        executed: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Tool for BlockingTool {
        fn name(&self) -> &str {
            "BlockingTool"
        }

        fn description(&self) -> &str {
            "Tool that waits until the test releases it."
        }

        fn input_schema(&self) -> serde_json::Value {
            json!({
                "type": "object",
                "properties": {}
            })
        }

        fn permission_level(&self) -> PermissionLevel {
            PermissionLevel::None
        }

        async fn execute(&self, _input: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
            self.executed.fetch_add(1, Ordering::SeqCst);
            self.finish.notified().await;
            ToolResult::success("blocking result")
        }
    }

    fn assert_injected_tool_message(
        message: &Message,
        expected_results: &[(&str, &str, bool)],
        expected_user_text: &str,
    ) {
        let MessageContent::Blocks(blocks) = &message.content else {
            panic!("expected user block message, got {:?}", message.content);
        };

        for (tool_id, expected_text, expected_error) in expected_results {
            let Some(ContentBlock::ToolResult {
                content: ToolResultContent::Text(text),
                is_error,
                ..
            }) = blocks.iter().find(|block| {
                matches!(
                    block,
                    ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == tool_id
                )
            })
            else {
                panic!("missing tool result for {tool_id}");
            };
            assert!(
                text.contains(expected_text),
                "tool result for {tool_id} did not contain {expected_text:?}: {text:?}"
            );
            assert_eq!(is_error.unwrap_or(false), *expected_error);
        }

        let text = blocks
            .iter()
            .filter_map(|block| {
                if let ContentBlock::Text { text } = block {
                    Some(text.as_str())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains(MID_RUN_USER_MESSAGE_HEADER));
        assert!(
            text.contains(expected_user_text),
            "injected text did not contain {expected_user_text:?}: {text:?}"
        );
    }

    async fn send_tool_use_response(
        tx: mpsc::Sender<StreamEvent>,
        message_id: String,
        tools: Vec<(&'static str, &'static str)>,
    ) {
        let _ = tx
            .send(StreamEvent::MessageStart {
                id: message_id,
                model: "test".into(),
            })
            .await;
        for (index, (tool_id, tool_name)) in tools.into_iter().enumerate() {
            let _ = tx
                .send(StreamEvent::ContentBlockStart {
                    index,
                    block_type: "tool_use".into(),
                    id: Some(tool_id.into()),
                    name: Some(tool_name.into()),
                    thought_signature: None,
                })
                .await;
            let _ = tx
                .send(StreamEvent::InputJsonDelta {
                    index,
                    partial_json: "{}".into(),
                })
                .await;
            let _ = tx.send(StreamEvent::ContentBlockStop { index }).await;
        }
        let _ = tx
            .send(StreamEvent::MessageDelta {
                stop_reason: Some(StopReason::ToolUse),
                usage: Some(Usage::default()),
            })
            .await;
        let _ = tx.send(StreamEvent::MessageStop).await;
    }

    async fn send_text_response(tx: mpsc::Sender<StreamEvent>, message_id: String, text: &str) {
        let _ = tx
            .send(StreamEvent::MessageStart {
                id: message_id,
                model: "test".into(),
            })
            .await;
        let _ = tx
            .send(StreamEvent::ContentBlockStart {
                index: 0,
                block_type: "text".into(),
                id: None,
                name: None,
                thought_signature: None,
            })
            .await;
        let _ = tx
            .send(StreamEvent::TextDelta {
                index: 0,
                text: text.into(),
            })
            .await;
        let _ = tx.send(StreamEvent::ContentBlockStop { index: 0 }).await;
        let _ = tx
            .send(StreamEvent::MessageDelta {
                stop_reason: Some(StopReason::EndTurn),
                usage: Some(Usage::default()),
            })
            .await;
        let _ = tx.send(StreamEvent::MessageStop).await;
    }

    struct InjectBeforeToolProvider {
        calls: AtomicUsize,
        release_first: Arc<Notify>,
        saw_injected_request: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Provider for InjectBeforeToolProvider {
        fn name(&self) -> &str {
            "inject-before-tool"
        }

        fn context_window(&self, _model: &str) -> u64 {
            4096
        }

        fn capabilities(&self, _model: &str) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                tool_use: true,
                ..Default::default()
            }
        }

        async fn complete(&self, request: CompletionRequest) -> Result<CompletionStream> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == 1 {
                let last = request
                    .messages
                    .last()
                    .expect("missing injected user message");
                assert_injected_tool_message(
                    last,
                    &[("tool-1", SKIPPED_TOOL_RESULT, true)],
                    "change course",
                );
                self.saw_injected_request.fetch_add(1, Ordering::SeqCst);
            }

            let (tx, rx) = mpsc::channel(16);
            let release_first = Arc::clone(&self.release_first);
            tokio::spawn(async move {
                if call == 0 {
                    release_first.notified().await;
                    send_tool_use_response(
                        tx,
                        "inject-before-tool-0".into(),
                        vec![("tool-1", "SkippedTool")],
                    )
                    .await;
                } else {
                    send_text_response(
                        tx,
                        "inject-before-tool-1".into(),
                        "Summary: changed course\nNext steps: continue with the user's update",
                    )
                    .await;
                }
            });

            Ok(CompletionStream::new(rx))
        }
    }

    #[tokio::test]
    async fn injected_message_before_tool_execution_skips_stale_tool_call() {
        let release_first = Arc::new(Notify::new());
        let saw_injected_request = Arc::new(AtomicUsize::new(0));
        let executed = Arc::new(AtomicUsize::new(0));

        let agent = Agent::builder()
            .provider(InjectBeforeToolProvider {
                calls: AtomicUsize::new(0),
                release_first: Arc::clone(&release_first),
                saw_injected_request: Arc::clone(&saw_injected_request),
            })
            .tool(CountingTool {
                name: "SkippedTool",
                executed: Arc::clone(&executed),
            })
            .working_dir(std::env::temp_dir())
            .model("test-model")
            .max_turns(4)
            .build()
            .unwrap();

        let mut stream = agent.run_stream("original request");
        while let Some(event) = stream.next().await {
            match event {
                AgentEvent::ModelResponseStart { turn: 1, .. } => {
                    stream.inject_message("change course".into());
                    release_first.notify_one();
                    break;
                }
                AgentEvent::Error(err) => panic!("agent errored before injection: {err}"),
                _ => {}
            }
        }

        let output = loop {
            match stream.next().await {
                Some(AgentEvent::ToolStart { name, .. }) => {
                    panic!("stale tool should have been skipped, started {name}");
                }
                Some(AgentEvent::Complete(output)) => break output,
                Some(AgentEvent::Error(err)) => panic!("agent errored: {err}"),
                Some(_) => {}
                None => panic!("agent stream ended without completion"),
            }
        };

        assert_eq!(executed.load(Ordering::SeqCst), 0);
        assert_eq!(saw_injected_request.load(Ordering::SeqCst), 1);
        assert!(output.message.get_all_text().contains("Summary:"));
    }

    struct InjectDuringToolProvider {
        calls: AtomicUsize,
        saw_injected_request: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Provider for InjectDuringToolProvider {
        fn name(&self) -> &str {
            "inject-during-tool"
        }

        fn context_window(&self, _model: &str) -> u64 {
            4096
        }

        fn capabilities(&self, _model: &str) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                tool_use: true,
                ..Default::default()
            }
        }

        async fn complete(&self, request: CompletionRequest) -> Result<CompletionStream> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == 1 {
                let last = request
                    .messages
                    .last()
                    .expect("missing injected user message");
                assert_injected_tool_message(
                    last,
                    &[
                        ("tool-1", "blocking result", false),
                        ("tool-2", SKIPPED_TOOL_RESULT, true),
                    ],
                    "stop after this tool",
                );
                self.saw_injected_request.fetch_add(1, Ordering::SeqCst);
            }

            let (tx, rx) = mpsc::channel(16);
            tokio::spawn(async move {
                if call == 0 {
                    send_tool_use_response(
                        tx,
                        "inject-during-tool-0".into(),
                        vec![("tool-1", "BlockingTool"), ("tool-2", "SecondTool")],
                    )
                    .await;
                } else {
                    send_text_response(
                        tx,
                        "inject-during-tool-1".into(),
                        "Summary: stopped after the first tool\nNext steps: follow the update",
                    )
                    .await;
                }
            });

            Ok(CompletionStream::new(rx))
        }
    }

    #[tokio::test]
    async fn injected_message_during_tool_execution_keeps_result_and_skips_remaining_tools() {
        let finish_blocking_tool = Arc::new(Notify::new());
        let blocking_executed = Arc::new(AtomicUsize::new(0));
        let second_executed = Arc::new(AtomicUsize::new(0));
        let saw_injected_request = Arc::new(AtomicUsize::new(0));

        let agent = Agent::builder()
            .provider(InjectDuringToolProvider {
                calls: AtomicUsize::new(0),
                saw_injected_request: Arc::clone(&saw_injected_request),
            })
            .tool(BlockingTool {
                finish: Arc::clone(&finish_blocking_tool),
                executed: Arc::clone(&blocking_executed),
            })
            .tool(CountingTool {
                name: "SecondTool",
                executed: Arc::clone(&second_executed),
            })
            .working_dir(std::env::temp_dir())
            .model("test-model")
            .max_turns(4)
            .build()
            .unwrap();

        let mut stream = agent.run_stream("run two tools");
        let output = loop {
            match stream.next().await {
                Some(AgentEvent::ToolStart { name, .. }) if name == "BlockingTool" => {
                    stream.inject_message("stop after this tool".into());
                    finish_blocking_tool.notify_one();
                }
                Some(AgentEvent::ToolStart { name, .. }) if name == "SecondTool" => {
                    panic!("second tool should have been skipped, started {name}");
                }
                Some(AgentEvent::Complete(output)) => break output,
                Some(AgentEvent::Error(err)) => panic!("agent errored: {err}"),
                Some(_) => {}
                None => panic!("agent stream ended without completion"),
            }
        };

        assert_eq!(blocking_executed.load(Ordering::SeqCst), 1);
        assert_eq!(second_executed.load(Ordering::SeqCst), 0);
        assert_eq!(saw_injected_request.load(Ordering::SeqCst), 1);
        assert!(output.message.get_all_text().contains("Summary:"));
    }

    struct SummaryProvider {
        responses: Vec<&'static str>,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Provider for SummaryProvider {
        fn name(&self) -> &str {
            "summary-test"
        }

        fn context_window(&self, _model: &str) -> u64 {
            4096
        }

        fn capabilities(&self, _model: &str) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                tool_use: false,
                ..Default::default()
            }
        }

        async fn complete(
            &self,
            request: CompletionRequest,
        ) -> cersei_types::Result<CompletionStream> {
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let response_text = self.responses.get(call).copied().unwrap_or("done");
            let (tx, rx) = tokio::sync::mpsc::channel(16);

            if call > 0 {
                let last_user = request
                    .messages
                    .iter()
                    .rev()
                    .find(|m| m.role == Role::User)
                    .map(|m| m.get_all_text())
                    .unwrap_or_default();
                assert_eq!(last_user, END_TURN_SUMMARY_PROMPT);
            }

            tokio::spawn(async move {
                let _ = tx
                    .send(StreamEvent::MessageStart {
                        id: format!("summary-{call}"),
                        model: "test".into(),
                    })
                    .await;
                let _ = tx
                    .send(StreamEvent::ContentBlockStart {
                        index: 0,
                        block_type: "text".into(),
                        id: None,
                        name: None,
                        thought_signature: None,
                    })
                    .await;
                let _ = tx
                    .send(StreamEvent::TextDelta {
                        index: 0,
                        text: response_text.into(),
                    })
                    .await;
                let _ = tx.send(StreamEvent::ContentBlockStop { index: 0 }).await;
                let _ = tx
                    .send(StreamEvent::MessageDelta {
                        stop_reason: Some(StopReason::EndTurn),
                        usage: Some(Usage::default()),
                    })
                    .await;
                let _ = tx.send(StreamEvent::MessageStop).await;
            });

            Ok(CompletionStream::new(rx))
        }
    }

    #[tokio::test]
    async fn end_turn_without_summary_triggers_follow_up_prompt() {
        let agent = Agent::builder()
            .provider(SummaryProvider {
                responses: vec![
                    "done",
                    "Summary: implemented validator\nNext steps: add more tests",
                ],
                calls: std::sync::atomic::AtomicUsize::new(0),
            })
            .working_dir(std::env::temp_dir())
            .model("test-model")
            .max_turns(4)
            .build()
            .unwrap();

        let output = run_agent(&agent, "test").await.unwrap();

        assert_eq!(output.turns, 2);
        assert!(output.message.get_all_text().contains("Summary:"));
    }
    #[tokio::test]
    async fn max_turns_reached_logs_summary_prompt_sent() {
        let agent = Agent::builder()
            .provider(SummaryProvider {
                responses: vec!["done"],
                calls: std::sync::atomic::AtomicUsize::new(0),
            })
            .working_dir(std::env::temp_dir())
            .model("test-model")
            .max_turns(0)
            .build()
            .unwrap();

        let output = run_agent(&agent, "test").await.unwrap();

        assert_eq!(output.turns, 1);
        assert_eq!(output.message.get_all_text(), "");
    }

    #[tokio::test]
    async fn end_turn_with_summary_does_not_trigger_follow_up_prompt() {
        let agent = Agent::builder()
            .provider(SummaryProvider {
                responses: vec!["Summary: implemented validator\nNext steps: add more tests"],
                calls: std::sync::atomic::AtomicUsize::new(0),
            })
            .working_dir(std::env::temp_dir())
            .model("test-model")
            .max_turns(4)
            .build()
            .unwrap();

        let output = run_agent(&agent, "test").await.unwrap();

        assert_eq!(output.turns, 1);
        assert!(output.message.get_all_text().contains("Summary:"));
    }
}
