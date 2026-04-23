//! REPL loop and single-shot execution with provider continuity.
//!
//! On provider errors (rate limits, overloaded, etc.), shows an interactive
//! prompt letting the user retry, switch providers, wait, or skip.

use crate::app;
use crate::commands;
use crate::config::AppConfig;
use crate::input::InputReader;
use crate::render::{self, StreamRenderer};
use crate::reviewer;
use crate::signals::SignalHandle;
use crate::status::StatusLine;
use crate::theme::Theme;
use cersei::Agent;
use cersei::events::AgentEvent;
use cersei_memory::manager::MemoryManager;
use cersei_memory::session_storage;
use cersei_tools::Extensions;
use cersei_tools::ReviewRequest;
use cersei_tools::xfile_storage::load_session_xfile_storage_from_path;
use cersei_types::{Message, Role};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use chrono::Local;

// ─── Recovery prompt ───────────────────────────────────────────────────────

enum Recovery {
    Retry,
    Switch(String),
    Wait(u64),
    Skip,
}

fn is_provider_error(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    lower.contains("429")
        || lower.contains("529")
        || lower.contains("503")
        || lower.contains("rate limit")
        || lower.contains("overloaded")
        || lower.contains("capacity")
        || lower.contains("too many requests")
}

fn prompt_recovery(current_model: &str, config: &AppConfig) -> Recovery {
    // Build options list
    let mut options: Vec<(String, String)> = Vec::new(); // (key, model_string)

    // Configured fallbacks
    for (i, model) in config.fallback_models.iter().enumerate() {
        options.push((format!("{}", i + 1), model.clone()));
    }

    // Other available providers not already listed
    let available = cersei_provider::router::available_providers();
    for entry in &available {
        let model_str = format!("{}/{}", entry.id, entry.default_model);
        if model_str != current_model
            && !config.fallback_models.contains(&model_str)
            && !config
                .fallback_models
                .iter()
                .any(|f| f.starts_with(entry.id))
        {
            let key = format!("{}", options.len() + 1);
            options.push((key, model_str));
        }
    }

    eprintln!();
    eprintln!("  \x1b[33mOptions:\x1b[0m");
    eprintln!("    \x1b[36m[r]\x1b[0m Retry with {current_model}");
    for (key, model) in &options {
        eprintln!("    \x1b[36m[{key}]\x1b[0m Switch to {model}");
    }
    eprintln!("    \x1b[36m[w]\x1b[0m Wait 30s then retry");
    eprintln!("    \x1b[90m[Enter]\x1b[0m Skip, return to prompt");
    eprint!("\n  Choice: ");
    let _ = std::io::Write::flush(&mut std::io::stderr());

    // Read single keypress
    let choice = read_choice();

    match choice.as_str() {
        "r" | "R" => Recovery::Retry,
        "w" | "W" => Recovery::Wait(30),
        "" => Recovery::Skip,
        key => {
            // Check if it's a numbered option
            if let Some((_, model)) = options.iter().find(|(k, _)| k == key) {
                Recovery::Switch(model.clone())
            } else {
                Recovery::Skip
            }
        }
    }
}

fn read_choice() -> String {
    use crossterm::event::{self, Event, KeyCode, KeyEvent};
    use crossterm::terminal;

    if terminal::enable_raw_mode().is_ok() {
        let result = loop {
            if let Ok(Event::Key(KeyEvent { code, .. })) = event::read() {
                break match code {
                    KeyCode::Char(c) => c.to_string(),
                    KeyCode::Enter => String::new(),
                    KeyCode::Esc => String::new(),
                    _ => continue,
                };
            }
        };
        let _ = terminal::disable_raw_mode();
        eprint!("\n");
        result
    } else {
        let mut input = String::new();
        let _ = std::io::stdin().read_line(&mut input);
        input.trim().to_string()
    }
}

// ─── REPL ──────────────────────────────────────────────────────────────────

/// Run the interactive REPL.
pub async fn run_repl(
    mut agent: Agent,
    theme: &Theme,
    session_id: &str,
    config: &AppConfig,
    memory_manager: &MemoryManager,
    tool_extensions: &Extensions,
    json_mode: bool,
    running: Arc<AtomicBool>,
    signal_handle: SignalHandle,
    reviewer_state: reviewer::ReviewerState,
) -> anyhow::Result<()> {
    let mut repl_config = config.clone();
    let mut current_session_id = session_id.to_string();
    let mut input_reader = InputReader::new()?;
    let mut renderer = StreamRenderer::new(theme, json_mode);
    let mut status = StatusLine::new(theme, &repl_config.model, &current_session_id, !json_mode);
    let mut cmd_registry = commands::CommandRegistry::new();
    let mut is_first_turn = true;
    let mut current_model = repl_config.model.clone();
    let mut exit_session_name: Option<String> = None;

    loop {
        // Prefer API-reported input tokens (exact); fall back to rough estimate before first turn
        let usage = agent.usage();
        let token_count = if usage.input_tokens > 0 {
            usage.input_tokens
        } else {
            cersei_agent::compact::estimate_messages_tokens(&agent.messages())
        };
        let prompt_str = format!(
            "\x1b[90m{token_count} {}\x1b[0m\x1b[92m> \x1b[0m",
            repl_config.effort
        );

        let input = match input_reader.readline(&prompt_str) {
            Some(line) => line,
            None => {
                input_reader.save_history();
                break;
            }
        };

        if input.is_empty() {
            continue;
        }

        let prompt = if input.starts_with('/') {
            let (cmd, args) = parse_command(&input);
            match cmd {
                "exit" | "quit" | "q" => {
                    input_reader.save_history();
                    break;
                }
                _ => match cmd_registry
                    .execute(cmd, args, &repl_config, &current_session_id)
                    .await
                {
                    Ok(commands::CommandAction::None) => None,
                    Ok(commands::CommandAction::RunReviewer { diff, hint }) => {
                        match reviewer::review_service(tool_extensions) {
                            Some(service) => {
                                match service
                                    .review(ReviewRequest::git_diff(diff).with_hint(hint))
                                    .await
                                {
                                    Ok(response) => {
                                        renderer.external_review(
                                            &response.reviewer_model,
                                            &response.reviewer_session_id,
                                            &response.review,
                                        );
                                        let mut messages = agent.messages();
                                        messages.push(Message::user(format!(
                                            "Reviewer feedback from session {} using {}:\n\n{}",
                                            response.reviewer_session_id,
                                            response.reviewer_model,
                                            response.review
                                        )));
                                        agent.set_messages(messages);
                                    }
                                    Err(err) => renderer.error(&format!("Review failed: {err}")),
                                }
                            }
                            None => renderer.error("Reviewer service is not available."),
                        }
                        None
                    }
                    Ok(commands::CommandAction::ClearHistory) => {
                        agent.clear_messages();
                        is_first_turn = true;
                        None
                    }
                    Ok(commands::CommandAction::SaveSession { name }) => {
                        let msgs = agent.messages();
                        match crate::sessions::save_named(
                            &repl_config,
                            &name,
                            &msgs,
                            &current_session_id,
                        ) {
                            Ok(_) => {
                                exit_session_name = Some(name.clone());
                                eprintln!("\x1b[33m  Session saved as '{}'\x1b[0m", name);
                            }
                            Err(e) => eprintln!("\x1b[31m  Save failed: {e}\x1b[0m"),
                        }
                        None
                    }
                    Ok(commands::CommandAction::LoadSession {
                        messages,
                        session_id: loaded_id,
                    }) => {
                        match app::build_agent(
                            &current_model,
                            &repl_config,
                            memory_manager,
                            &loaded_id,
                            signal_handle.token(),
                            Some(messages),
                            tool_extensions.clone(),
                        ) {
                            Ok((new_agent, resolved)) => {
                                let xfile_path = session_storage::xfile_storage_path(
                                    &repl_config.working_dir,
                                    &loaded_id,
                                );
                                if let Err(err) =
                                    load_session_xfile_storage_from_path(&loaded_id, &xfile_path)
                                {
                                    eprintln!(
                                        "\x1b[31m  XFileStorage restore failed: {err}\x1b[0m"
                                    );
                                }
                                agent = new_agent;
                                current_model =
                                    if let Some((provider, _)) = current_model.split_once('/') {
                                        format!("{provider}/{resolved}")
                                    } else {
                                        resolved.clone()
                                    };
                                repl_config.model = current_model.clone();
                                if let Some((provider, _)) = current_model.split_once('/') {
                                    repl_config.provider = provider.to_string();
                                }
                                current_session_id = loaded_id.clone();
                                reviewer_state
                                    .set_session_id(reviewer::reviewer_session_id(&loaded_id));
                                reviewer_state.set_xfile_session_id(loaded_id.clone());
                                is_first_turn = true;
                                status = StatusLine::new(
                                    theme,
                                    &repl_config.model,
                                    &current_session_id,
                                    !json_mode,
                                );
                            }
                            Err(e) => eprintln!("\x1b[31m  Resume failed: {e}\x1b[0m"),
                        }
                        None
                    }
                    Ok(commands::CommandAction::Compact) => {
                        let before = agent.messages().len();
                        let before_tokens = before * 500; // rough estimate for display
                        eprintln!("\x1b[90m  Compacting {} messages...\x1b[0m", before);
                        match agent.compact_now().await {
                            Ok(result) => {
                                eprintln!(
                                    "\x1b[90m  Compacted: {} → {} messages (~{} tokens freed)\x1b[0m",
                                    result.messages_before,
                                    result.messages_after,
                                    result.tokens_freed_estimate,
                                );
                                let _ = before_tokens; // suppress warning
                            }
                            Err(e) => eprintln!("\x1b[31m  Compaction failed: {e}\x1b[0m"),
                        }
                        None
                    }
                    Ok(commands::CommandAction::InjectUserMessage { message }) => {
                        let mut messages = agent.messages();
                        messages.push(Message::user(message));
                        agent.set_messages(messages);
                        None
                    }
                    Ok(commands::CommandAction::SwitchAgent { model }) => {
                        let msgs = agent.messages();
                        match app::build_agent(
                            &model,
                            &repl_config,
                            memory_manager,
                            &current_session_id,
                            signal_handle.token(),
                            Some(msgs),
                            tool_extensions.clone(),
                        ) {
                            Ok((new_agent, resolved)) => {
                                agent = new_agent;
                                current_model = if let Some((provider, _)) = model.split_once('/') {
                                    format!("{provider}/{resolved}")
                                } else {
                                    resolved.clone()
                                };
                                repl_config.model = current_model.clone();
                                if let Some((provider, _)) = current_model.split_once('/') {
                                    repl_config.provider = provider.to_string();
                                }
                                status.set_model(&current_model);
                                renderer.model_switched(&current_model);
                            }
                            Err(e) => renderer.error(&format!("Switch failed: {e}")),
                        }
                        None
                    }
                    Ok(commands::CommandAction::SwitchReviewer { model }) => {
                        repl_config.reviewer_model = model.clone();
                        reviewer_state.set_model(model.clone());
                        renderer.model_switched(&format!("reviewer {model}"));
                        None
                    }
                    Ok(commands::CommandAction::SwitchEffort { effort }) => {
                        let previous_effort = repl_config.effort;
                        let previous_max_tokens = repl_config.max_tokens;
                        let msgs = agent.messages();
                        crate::config::set_effort_budget(&mut repl_config, effort);
                        match app::build_agent(
                            &current_model,
                            &repl_config,
                            memory_manager,
                            &current_session_id,
                            signal_handle.token(),
                            Some(msgs),
                            tool_extensions.clone(),
                        ) {
                            Ok((new_agent, resolved)) => {
                                agent = new_agent;
                                current_model =
                                    if let Some((provider, _)) = current_model.split_once('/') {
                                        format!("{provider}/{resolved}")
                                    } else {
                                        resolved.clone()
                                    };
                                repl_config.model = current_model.clone();
                                if let Some((provider, _)) = current_model.split_once('/') {
                                    repl_config.provider = provider.to_string();
                                }
                                status.set_model(&current_model);
                                if !json_mode {
                                    eprintln!(
                                        "\x1b[90m  Effort set to {} thinking tokens (max_tokens {})\x1b[0m",
                                        effort, repl_config.max_tokens
                                    );
                                }
                            }
                            Err(e) => {
                                repl_config.effort = previous_effort;
                                repl_config.max_tokens = previous_max_tokens;
                                renderer.error(&format!("Effort switch failed: {e}"));
                            }
                        }
                        None
                    }
                    Err(e) => {
                        eprintln!("\x1b[31mCommand error: {e}\x1b[0m");
                        None
                    }
                },
            }
        } else {
            Some(input)
        };

        let Some(prompt) = prompt else {
            continue;
        };

        // Run agent with retry/recovery loop
        let mut should_retry = true;
        while should_retry {
            should_retry = false;

            running.store(true, Ordering::Relaxed);
            let result = run_agent_streaming(
                &agent,
                &prompt,
                &prompt_str,
                &mut renderer,
                &mut status,
                json_mode,
                is_first_turn,
            )
            .await;
            running.store(false, Ordering::Relaxed);

            match result {
                Ok(_) => {
                    is_first_turn = false;
                    signal_handle.reset();
                }
                Err(err_msg) => {
                    renderer.error(&err_msg);
                    signal_handle.reset();

                    if err_msg.trim() == "Cancelled" {
                        let msgs = agent.messages();
                        match app::build_agent(
                            &current_model,
                            &repl_config,
                            memory_manager,
                            &current_session_id,
                            signal_handle.token(),
                            Some(msgs),
                            tool_extensions.clone(),
                        ) {
                            Ok((new_agent, resolved)) => {
                                agent = new_agent;
                                current_model =
                                    if let Some((provider, _)) = current_model.split_once('/') {
                                        format!("{provider}/{resolved}")
                                    } else {
                                        resolved.clone()
                                    };
                                repl_config.model = current_model.clone();
                                status.set_model(&current_model);
                            }
                            Err(e) => renderer.error(&format!("Reset after cancel failed: {e}")),
                        }
                    } else if is_provider_error(&err_msg) {
                        match prompt_recovery(&current_model, &repl_config) {
                            Recovery::Retry => {
                                should_retry = true;
                            }
                            Recovery::Switch(new_model) => {
                                // Snapshot messages, pop the last user msg (runner already added it)
                                let mut msgs = agent.messages();
                                if msgs.last().map(|m| m.role == Role::User).unwrap_or(false) {
                                    msgs.pop();
                                }

                                match app::build_agent(
                                    &new_model,
                                    &repl_config,
                                    memory_manager,
                                    &current_session_id,
                                    signal_handle.token(),
                                    Some(msgs),
                                    tool_extensions.clone(),
                                ) {
                                    Ok((new_agent, resolved)) => {
                                        agent = new_agent;
                                        current_model = format!(
                                            "{}/{}",
                                            new_model.split('/').next().unwrap_or(""),
                                            &resolved
                                        );
                                        if current_model.starts_with('/') {
                                            current_model = resolved.clone();
                                        }
                                        repl_config.model = current_model.clone();
                                        if let Some((provider, _)) = new_model.split_once('/') {
                                            repl_config.provider = provider.to_string();
                                        }
                                        status.set_model(&current_model);
                                        renderer.model_switched(&resolved);
                                        should_retry = true;
                                    }
                                    Err(e) => {
                                        renderer.error(&format!("Switch failed: {e}"));
                                    }
                                }
                            }
                            Recovery::Wait(secs) => {
                                eprintln!("\x1b[90m  Waiting {secs}s...\x1b[0m");
                                tokio::time::sleep(Duration::from_secs(secs)).await;
                                should_retry = true;
                            }
                            Recovery::Skip => {}
                        }
                    }
                }
            }
        }
    }

    let exit_name = exit_session_name
        .as_deref()
        .unwrap_or(current_session_id.as_str());
    let msgs = agent.messages();
    crate::sessions::save_named(&repl_config, exit_name, &msgs, &current_session_id)?;
    eprintln!("\x1b[33mSession saved as '{}'\x1b[0m", exit_name);
    Ok(())
}

/// Run a single prompt (non-interactive).
pub async fn run_single_shot(
    agent: Agent,
    prompt: &str,
    theme: &Theme,
    session_id: &str,
    config: &AppConfig,
    _memory_manager: &MemoryManager,
    _tool_extensions: &Extensions,
    json_mode: bool,
    running: Arc<AtomicBool>,
    _signal_handle: SignalHandle,
) -> anyhow::Result<()> {
    let mut renderer = StreamRenderer::new(theme, json_mode);
    let mut status = StatusLine::new(theme, &config.model, session_id, false);

    running.store(true, Ordering::Relaxed);
    let result =
        run_agent_streaming(&agent, prompt, "", &mut renderer, &mut status, json_mode, true).await;
    running.store(false, Ordering::Relaxed);

    match result {
        Ok(_) => Ok(()),
        Err(msg) => {
            renderer.error(&msg);
            Err(anyhow::anyhow!("{msg}"))
        }
    }
}

fn print_handover_waiting_banner(prompt: &str) {
    let timestamp = Local::now().format("%H:%M:%S");
    eprintln!("\x1b[32m*************** {timestamp} ***************\x1b[0m");
    eprintln!("{prompt}");
    let _ = std::io::Write::flush(&mut std::io::stderr());
}

/// Core event loop: stream agent events and render them.
/// Returns Ok(()) on success or Err(error_message) on failure.
async fn run_agent_streaming(
    agent: &Agent,
    prompt: &str,
    input_prompt: &str,
    renderer: &mut StreamRenderer,
    status: &mut StatusLine,
    json_mode: bool,
    _is_first: bool,
) -> Result<(), String> {
    let mut stream = agent.run_stream(prompt);

    while let Some(event) = stream.next().await {
        if json_mode {
            render::print_json_event(&event);
        }

        match event {
            AgentEvent::TextDelta(text) => {
                renderer.push_text(&text);
            }
            AgentEvent::ThinkingDelta(text) => {
                renderer.push_thinking(&text);
            }
            AgentEvent::ToolStart { name, id: _, input } => {
                renderer.tool_start(&name, &input);
            }
            AgentEvent::ToolEnd {
                name,
                id: _,
                result,
                is_error,
                duration,
            } => {
                renderer.tool_end(&name, &result, is_error, duration);
            }
            AgentEvent::PermissionRequired(_request) => {}
            AgentEvent::CostUpdate {
                cumulative_cost,
                input_tokens,
                output_tokens,
                ..
            } => {
                status.update_cost(input_tokens, output_tokens, cumulative_cost);
            }
            AgentEvent::TurnComplete { usage, .. } => {
                if let Some(cost) = usage.cost_usd {
                    status.update_cost(usage.input_tokens, usage.output_tokens, cost);
                }
            }
            AgentEvent::TokenWarning { pct_used, .. } => {
                status.update_context(pct_used);
            }
            AgentEvent::CompactStart { reason, .. } => {
                if !json_mode {
                    eprintln!("\x1b[90m  Compacting context ({:?})...\x1b[0m", reason);
                }
            }
            AgentEvent::SessionSaved { .. } => {
                if !json_mode {
                    print_handover_waiting_banner(input_prompt);
                }
            }
            AgentEvent::CompactEnd {
                messages_after,
                tokens_freed,
            } => {
                if !json_mode {
                    eprintln!(
                        "\x1b[90m  Compacted: {} messages, ~{} tokens freed\x1b[0m",
                        messages_after, tokens_freed
                    );
                }
            }
            AgentEvent::SessionLoaded {
                session_id,
                message_count,
            } => {
                if !json_mode {
                    eprintln!(
                        "\x1b[90m  Resumed session {} ({} messages)\x1b[0m",
                        &session_id[..8.min(session_id.len())],
                        message_count
                    );
                }
            }
            AgentEvent::SubAgentSpawned {
                agent_id, prompt, ..
            } => {
                if !json_mode {
                    let preview: String = prompt.chars().take(60).collect();
                    eprintln!(
                        "\x1b[90m  Sub-agent {}: {preview}...\x1b[0m",
                        &agent_id[..8.min(agent_id.len())]
                    );
                }
            }
            AgentEvent::Error(msg) => {
                renderer.flush();
                return Err(msg);
            }
            AgentEvent::Complete(_output) => {
                renderer.complete();
                return Ok(());
            }
            _ => {}
        }
    }

    Ok(())
}

fn parse_command(input: &str) -> (&str, &str) {
    let input = input.trim_start_matches('/');
    if let Some(space) = input.find(char::is_whitespace) {
        (&input[..space], input[space..].trim())
    } else {
        (input, "")
    }
}
