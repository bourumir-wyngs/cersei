use crate::app;
use crate::config::AppConfig;
use crate::sessions;
use cersei_memory::manager::MemoryManager;
use cersei_memory::session_storage;
use cersei_tools::{Extensions, ReviewExecutor, ReviewRequest, ReviewResponse, ReviewService};
use parking_lot::RwLock;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct ReviewerState {
    inner: Arc<RwLock<ReviewerStateInner>>,
}

#[derive(Debug, Clone)]
struct ReviewerStateInner {
    model: String,
    session_id: String,
    xfile_session_id: String,
}

impl ReviewerState {
    pub fn new(model: String, session_id: String, xfile_session_id: String) -> Self {
        Self {
            inner: Arc::new(RwLock::new(ReviewerStateInner {
                model,
                session_id,
                xfile_session_id,
            })),
        }
    }

    pub fn model(&self) -> String {
        self.inner.read().model.clone()
    }

    pub fn set_model(&self, model: String) {
        self.inner.write().model = model;
    }

    pub fn session_id(&self) -> String {
        self.inner.read().session_id.clone()
    }

    pub fn set_session_id(&self, session_id: String) {
        self.inner.write().session_id = session_id;
    }

    pub fn xfile_session_id(&self) -> String {
        self.inner.read().xfile_session_id.clone()
    }

    pub fn set_xfile_session_id(&self, xfile_session_id: String) {
        self.inner.write().xfile_session_id = xfile_session_id;
    }
}

pub fn reviewer_session_id(coding_session_id: &str) -> String {
    format!("{coding_session_id}-reviewer")
}

pub fn review_service(extensions: &Extensions) -> Option<Arc<ReviewService>> {
    extensions.get::<ReviewService>()
}

pub struct CliReviewerExecutor {
    config: AppConfig,
    memory_manager: Arc<MemoryManager>,
    tool_extensions: Extensions,
    state: ReviewerState,
}

impl CliReviewerExecutor {
    pub fn new(
        config: AppConfig,
        memory_manager: Arc<MemoryManager>,
        tool_extensions: Extensions,
        state: ReviewerState,
    ) -> Self {
        Self {
            config,
            memory_manager,
            tool_extensions,
            state,
        }
    }
}

#[async_trait::async_trait]
impl ReviewExecutor for CliReviewerExecutor {
    async fn review(&self, request: ReviewRequest) -> Result<ReviewResponse, String> {
        let reviewer_model = self.state.model();
        let reviewer_session_id = self.state.session_id();
        let xfile_session_id = self.state.xfile_session_id();
        let existing_messages = load_reviewer_messages(&self.config, &reviewer_session_id)
            .map_err(|err| format!("Failed to load reviewer session: {err}"))?;
        let (agent, resolved_model) = app::build_reviewer_agent(
            &reviewer_model,
            &self.config,
            self.memory_manager.as_ref(),
            &reviewer_session_id,
            &xfile_session_id,
            CancellationToken::new(),
            Some(existing_messages),
            self.tool_extensions.clone(),
        )
        .map_err(|err| format!("Failed to build reviewer agent: {err}"))?;

        let output = agent
            .run(&build_review_prompt(&request))
            .await
            .map_err(|err| format!("Reviewer execution failed: {err}"))?;

        let review = output.text().trim().to_string();
        if review.is_empty() {
            return Err("Reviewer returned an empty response.".to_string());
        }

        let messages = agent.messages();
        sessions::save_named(
            &self.config,
            &reviewer_session_id,
            &messages,
            &reviewer_session_id,
        )
        .map_err(|err| format!("Failed to save reviewer session: {err}"))?;

        Ok(ReviewResponse {
            review,
            reviewer_model: resolved_model,
            reviewer_session_id,
        })
    }
}

fn build_review_prompt(request: &ReviewRequest) -> String {
    format!(
        "Review this {} from another agent's work. Focus on major defects, unsafe code, and suspicious behavior.\n\n{}",
        request.source.label(),
        request.diff.as_str()
    )
}

fn load_reviewer_messages(
    config: &AppConfig,
    reviewer_session_id: &str,
) -> anyhow::Result<Vec<cersei_types::Message>> {
    let path = session_storage::transcript_path(&config.working_dir, reviewer_session_id);
    if !path.exists() {
        return Ok(Vec::new());
    }

    let entries = session_storage::load_transcript(&path)
        .map_err(|err| anyhow::anyhow!("Failed to load reviewer transcript: {err}"))?;
    Ok(cersei_agent::strip_thinking_blocks(
        session_storage::messages_from_transcript(&entries),
    ))
}
