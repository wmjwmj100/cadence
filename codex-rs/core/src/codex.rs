use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt::Debug;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use crate::AuthManager;
use crate::CodexAuth;
use crate::SandboxState;
use crate::agent::AgentControl;
use crate::agent::AgentStatus;
use crate::agent::agent_status_from_event;
use crate::agent::build_agent_context_developer_instructions_for_agent;
use crate::analytics_client::AnalyticsEventsClient;
use crate::analytics_client::AppInvocation;
use crate::analytics_client::build_track_events_context;
use crate::apps::render_apps_section;
use crate::blackboard;
use crate::blackboard::DEFAULT_BLACKBOARD_SNAPSHOT_CHAR_LIMIT;
use crate::compact;
use crate::compact::run_inline_auto_compact_task;
use crate::compact::should_use_remote_compact_task;
use crate::compact_remote::run_inline_remote_auto_compact_task;
use crate::connectors;
use crate::exec_policy::ExecPolicyManager;
use crate::features::FEATURES;
use crate::features::Feature;
use crate::features::Features;
use crate::features::maybe_push_unstable_features_warning;
use crate::models_manager::manager::ModelsManager;
use crate::parse_command::parse_command;
use crate::parse_turn_item;
use crate::rollout::session_index;
use crate::stream_events_utils::HandleOutputCtx;
use crate::stream_events_utils::handle_non_tool_response_item;
use crate::stream_events_utils::handle_output_item_done;
use crate::stream_events_utils::last_assistant_message_from_item;
use crate::swarm::default_root_swarm_is_complex;
use crate::swarm::swarm_developer_instructions_for_session_source as resolve_swarm_prompt;
use crate::terminal;
use crate::truncate::TruncationPolicy;
use crate::turn_metadata::TurnMetadataState;
use crate::util::error_or_panic;
use async_channel::Receiver;
use async_channel::Sender;
use codex_hooks::HookEvent;
use codex_hooks::HookEventAfterAgent;
use codex_hooks::HookPayload;
use codex_hooks::Hooks;
use codex_hooks::HooksConfig;
use codex_network_proxy::NetworkProxy;
use codex_protocol::ThreadId;
use codex_protocol::approvals::ExecPolicyAmendment;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::config_types::WebSearchMode;
use codex_protocol::dynamic_tools::DynamicToolResponse;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::items::PlanItem;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserMessageItem;
use codex_protocol::mcp::CallToolResult;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::format_allow_prefixes;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::protocol::FileChange;
use codex_protocol::protocol::HasLegacyEvent;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ItemStartedEvent;
use codex_protocol::protocol::RawResponseItemEvent;
use codex_protocol::protocol::ReviewRequest;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::StreamInfoEvent;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnContextItem;
use codex_protocol::protocol::TurnContextNetworkItem;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::protocol::UserInputOrigin;
use codex_protocol::request_user_input::RequestUserInputArgs;
use codex_protocol::request_user_input::RequestUserInputResponse;
use codex_rmcp_client::ElicitationResponse;
use codex_rmcp_client::OAuthCredentialsStoreMode;
use futures::future::BoxFuture;
use futures::prelude::*;
use futures::stream::FuturesOrdered;
use rmcp::model::ListResourceTemplatesResult;
use rmcp::model::ListResourcesResult;
use rmcp::model::PaginatedRequestParams;
use rmcp::model::ReadResourceRequestParams;
use rmcp::model::ReadResourceResult;
use rmcp::model::RequestId;
use serde_json;
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::sync::RwLock;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use tracing::debug;
use tracing::error;
use tracing::field;
use tracing::info;
use tracing::info_span;
use tracing::instrument;
use tracing::trace;
use tracing::trace_span;
use tracing::warn;
use uuid::Uuid;

const TERMINATION_JUDGE_RECENT_ASSISTANT_LIMIT: usize = 15;
const TERMINATION_JUDGE_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
enum TerminationJudgeDecision {
    Finalize { user_next_steps: Option<String> },
    Continue,
}

#[derive(Debug, serde::Deserialize)]
struct TerminationJudgeOutput {
    decision: String,
    #[serde(default)]
    user_next_steps: Option<String>,
}

use crate::ModelProviderInfo;
use crate::client::ModelClient;
use crate::client::ModelClientSession;
use crate::client_common::Prompt;
use crate::client_common::ResponseEvent;
use crate::codex_thread::ThreadConfigSnapshot;
use crate::compact::collect_user_messages;
use crate::config::CONFIG_TOML_FILE;
use crate::config::Config;
use crate::config::Constrained;
use crate::config::ConstraintResult;
use crate::config::GhostSnapshotConfig;
use crate::config::StartedNetworkProxy;
use crate::config::resolve_web_search_mode_for_turn;
use crate::config::types::McpServerConfig;
use crate::config::types::ShellEnvironmentPolicy;
use crate::context_manager::ContextManager;
use crate::context_manager::TotalTokenUsageBreakdown;
use crate::debug_trace;
use crate::environment_context::EnvironmentContext;
use crate::error::CodexErr;
use crate::error::Result as CodexResult;
#[cfg(test)]
use crate::exec::StreamOutput;

fn model_io_debug_dir(config: &Config) -> Option<PathBuf> {
    let Ok(raw) = std::env::var("CODEX_DEBUG_MODEL_IO") else {
        return None;
    };
    let enabled = match raw.trim().to_ascii_lowercase().as_str() {
        "" | "0" | "false" | "no" | "off" => false,
        _ => true,
    };
    if !enabled {
        return None;
    }

    match std::env::var("CODEX_DEBUG_MODEL_IO_DIR") {
        Ok(dir) if !dir.trim().is_empty() => Some(PathBuf::from(dir.trim())),
        _ => Some(config.codex_home.join("debug").join("model-io")),
    }
}

fn shared_blackboard_owner_thread_id(
    conversation_id: ThreadId,
    session_source: &SessionSource,
) -> ThreadId {
    match session_source {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id, ..
        }) => *parent_thread_id,
        _ => conversation_id,
    }
}

fn visible_agent_name(agent_control: &AgentControl, thread_id: ThreadId) -> String {
    agent_control
        .agent_name_for_thread(thread_id)
        .unwrap_or_else(|| crate::agent::UNNAMED_AGENT_NAME.to_string())
}

fn visible_agent_name_for_session(
    agent_control: &AgentControl,
    thread_id: ThreadId,
    session_source: &SessionSource,
) -> String {
    match session_source {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            agent_name_hint: Some(agent_name_hint),
            ..
        }) => agent_name_hint.clone(),
        _ => agent_control
            .agent_name_for_thread(thread_id)
            .unwrap_or_else(|| crate::agent::PRIMARY_AGENT_NAME.to_string()),
    }
}

fn durable_agent_id_for_session(
    agent_control: &AgentControl,
    thread_id: ThreadId,
    session_source: &SessionSource,
) -> String {
    match session_source {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            agent_name_hint: Some(agent_name_hint),
            ..
        }) => agent_name_hint.clone(),
        _ => agent_control
            .agent_name_for_thread(thread_id)
            .unwrap_or_else(|| crate::agent::PRIMARY_AGENT_NAME.to_string()),
    }
}

fn developer_instructions_for_collaboration_mode(
    collaboration_mode: &CollaborationMode,
    session_source: &SessionSource,
) -> Option<DeveloperInstructions> {
    if collaboration_mode.mode == ModeKind::Swarm {
        let root_complex = collaboration_mode
            .settings
            .developer_instructions
            .as_deref()
            .map(|instructions| instructions.contains("# Collaboration Mode: Swarm Complex"))
            .unwrap_or_else(default_root_swarm_is_complex);
        return Some(DeveloperInstructions::new(
            resolve_swarm_prompt(session_source, root_complex).to_string(),
        ));
    }
    DeveloperInstructions::from_collaboration_mode(collaboration_mode)
}

#[derive(Debug, PartialEq)]
pub enum SteerInputError {
    NoActiveTurn(Vec<UserInput>),
    ExpectedTurnMismatch { expected: String, actual: String },
    EmptyInput,
}
use crate::exec_policy::ExecPolicyUpdateError;
use crate::feedback_tags;
use crate::file_watcher::FileWatcher;
use crate::file_watcher::FileWatcherEvent;
use crate::git_info::get_git_repo_root;
use crate::instructions::UserInstructions;
use crate::mcp::CODEX_APPS_MCP_SERVER_NAME;
use crate::mcp::auth::compute_auth_statuses;
use crate::mcp::effective_mcp_servers;
use crate::mcp::maybe_prompt_and_install_mcp_dependencies;
use crate::mcp::with_codex_apps_mcp;
use crate::mcp_connection_manager::McpConnectionManager;
use crate::mcp_connection_manager::filter_codex_apps_mcp_tools_only;
use crate::mcp_connection_manager::filter_mcp_tools_by_name;
use crate::memories;
use crate::mentions::build_connector_slug_counts;
use crate::mentions::build_skill_name_counts;
use crate::mentions::collect_explicit_app_ids;
use crate::mentions::collect_tool_mentions_from_messages;
use crate::project_doc::get_user_instructions;
use crate::prompt_paths::prompt_cwd;
use crate::proposed_plan_parser::ProposedPlanParser;
use crate::proposed_plan_parser::ProposedPlanSegment;
use crate::proposed_plan_parser::extract_proposed_plan_text;
use crate::protocol::AgentMessageContentDeltaEvent;
use crate::protocol::AgentReasoningSectionBreakEvent;
use crate::protocol::ApplyPatchApprovalRequestEvent;
use crate::protocol::AskForApproval;
use crate::protocol::BackgroundEventEvent;
use crate::protocol::DeprecationNoticeEvent;
use crate::protocol::ErrorEvent;
use crate::protocol::Event;
use crate::protocol::EventMsg;
use crate::protocol::ExecApprovalRequestEvent;
use crate::protocol::McpServerRefreshConfig;
use crate::protocol::NetworkApprovalContext;
use crate::protocol::Op;
use crate::protocol::PlanDeltaEvent;
use crate::protocol::RateLimitSnapshot;
use crate::protocol::ReasoningContentDeltaEvent;
use crate::protocol::ReasoningRawContentDeltaEvent;
use crate::protocol::RequestUserInputEvent;
use crate::protocol::ReviewDecision;
use crate::protocol::SandboxPolicy;
use crate::protocol::SessionConfiguredEvent;
use crate::protocol::SessionNetworkProxyRuntime;
use crate::protocol::SkillDependencies as ProtocolSkillDependencies;
use crate::protocol::SkillErrorInfo;
use crate::protocol::SkillInterface as ProtocolSkillInterface;
use crate::protocol::SkillMetadata as ProtocolSkillMetadata;
use crate::protocol::SkillToolDependency as ProtocolSkillToolDependency;
use crate::protocol::StreamErrorEvent;
use crate::protocol::Submission;
use crate::protocol::TokenCountEvent;
use crate::protocol::TokenUsage;
use crate::protocol::TokenUsageInfo;
use crate::protocol::TurnDiffEvent;
use crate::protocol::WarningEvent;
use crate::rollout::RolloutRecorder;
use crate::rollout::RolloutRecorderParams;
use crate::rollout::map_session_init_error;
use crate::rollout::metadata;
use crate::rollout::policy::EventPersistenceMode;
use crate::shell;
use crate::shell_snapshot::ShellSnapshot;
use crate::skills::SkillError;
use crate::skills::SkillInjections;
use crate::skills::SkillLoadOutcome;
use crate::skills::SkillMetadata;
use crate::skills::SkillsManager;
use crate::skills::build_skill_injections;
use crate::skills::collect_env_var_dependencies;
use crate::skills::collect_explicit_skill_mentions;
use crate::skills::injection::ToolMentionKind;
use crate::skills::injection::app_id_from_path;
use crate::skills::injection::tool_kind_for_path;
use crate::skills::resolve_skill_dependencies_for_turn;
use crate::state::ActiveTurn;
use crate::state::SessionServices;
use crate::state::SessionState;
use crate::state_db;
use crate::tasks::GhostSnapshotTask;
use crate::tasks::RegularTask;
use crate::tasks::ReviewTask;
use crate::tasks::SessionTask;
use crate::tasks::SessionTaskContext;
use crate::tools::ToolRouter;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::handlers::collab_inbox;
use crate::tools::js_repl::JsReplHandle;
use crate::tools::network_approval::NetworkApprovalService;
use crate::tools::network_approval::build_blocked_request_observer;
use crate::tools::network_approval::build_network_policy_decider;
use crate::tools::parallel::ToolCallRuntime;
use crate::tools::sandboxing::ApprovalStore;
use crate::tools::spec::ToolsConfig;
use crate::tools::spec::ToolsConfigParams;
use crate::turn_diff_tracker::TurnDiffTracker;
use crate::unified_exec::UnifiedExecProcessManager;
use crate::util::backoff;
use crate::windows_sandbox::WindowsSandboxLevelExt;
use codex_async_utils::OrCancelExt;
use codex_otel::OtelManager;
use codex_otel::TelemetryAuthMode;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::Personality;
use codex_protocol::config_types::ReasoningSummary as ReasoningSummaryConfig;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::ContentItem;
use codex_protocol::models::DeveloperInstructions;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::user_input::UserInput;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_readiness::Readiness;
use codex_utils_readiness::ReadinessFlag;

/// The high-level interface to the Codex system.
/// It operates as a queue pair where you send submissions and receive events.
pub struct Codex {
    pub(crate) tx_sub: Sender<Submission>,
    pub(crate) rx_event: Receiver<Event>,
    // Last known status of the agent.
    pub(crate) agent_status: watch::Receiver<AgentStatus>,
    pub(crate) session: Arc<Session>,
}

/// Wrapper returned by [`Codex::spawn`] containing the spawned [`Codex`],
/// the submission id for the initial `ConfigureSession` request and the
/// unique session id.
pub struct CodexSpawnOk {
    pub codex: Codex,
    pub thread_id: ThreadId,
    #[deprecated(note = "use thread_id")]
    pub conversation_id: ThreadId,
}

pub(crate) const INITIAL_SUBMIT_ID: &str = "";
pub(crate) const SUBMISSION_CHANNEL_CAPACITY: usize = 64;

impl Codex {
    /// Spawn a new [`Codex`] and initialize the session.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn spawn(
        config: Config,
        auth_manager: Arc<AuthManager>,
        models_manager: Arc<ModelsManager>,
        skills_manager: Arc<SkillsManager>,
        file_watcher: Arc<FileWatcher>,
        conversation_history: InitialHistory,
        session_source: SessionSource,
        agent_control: AgentControl,
        dynamic_tools: Vec<DynamicToolSpec>,
        persist_extended_history: bool,
    ) -> CodexResult<CodexSpawnOk> {
        let (tx_sub, rx_sub) = async_channel::bounded(SUBMISSION_CHANNEL_CAPACITY);
        let (tx_event, rx_event) = async_channel::unbounded();

        let loaded_skills = skills_manager.skills_for_config(&config);

        for err in &loaded_skills.errors {
            error!(
                "failed to load skill {}: {}",
                err.path.display(),
                err.message
            );
        }

        let allowed_skills_for_implicit_invocation =
            loaded_skills.allowed_skills_for_implicit_invocation();
        let user_instructions =
            get_user_instructions(&config, Some(&allowed_skills_for_implicit_invocation)).await;

        let exec_policy = ExecPolicyManager::load(&config.config_layer_stack)
            .await
            .map_err(|err| CodexErr::Fatal(format!("failed to load rules: {err}")))?;

        let config = Arc::new(config);
        let _ = models_manager
            .list_models(
                &config,
                crate::models_manager::manager::RefreshStrategy::OnlineIfUncached,
            )
            .await;
        let model = models_manager
            .get_default_model(
                &config.model,
                &config,
                crate::models_manager::manager::RefreshStrategy::OnlineIfUncached,
            )
            .await;

        // Resolve base instructions for the session. Priority order:
        // 1. config.base_instructions override
        // 2. conversation history => session_meta.base_instructions
        // 3. base_instructions for current model
        let model_info = models_manager.get_model_info(model.as_str(), &config).await;
        let base_instructions = config
            .base_instructions
            .clone()
            .or_else(|| conversation_history.get_base_instructions().map(|s| s.text))
            .unwrap_or_else(|| model_info.get_model_instructions(config.personality));

        // Respect thread-start tools. When missing (resumed/forked threads), read from the db
        // first, then fall back to rollout-file tools.
        let persisted_tools = if dynamic_tools.is_empty()
            && config.features.enabled(Feature::Sqlite)
        {
            let thread_id = match &conversation_history {
                InitialHistory::Resumed(resumed) => Some(resumed.conversation_id),
                InitialHistory::Forked(_) => conversation_history.forked_from_id(),
                InitialHistory::New => None,
            };
            match thread_id {
                Some(thread_id) => {
                    let state_db_ctx = state_db::get_state_db(&config, None).await;
                    state_db::get_dynamic_tools(state_db_ctx.as_deref(), thread_id, "codex_spawn")
                        .await
                }
                None => None,
            }
        } else {
            None
        };
        let dynamic_tools = if dynamic_tools.is_empty() {
            persisted_tools
                .or_else(|| conversation_history.get_dynamic_tools())
                .unwrap_or_default()
        } else {
            dynamic_tools
        };

        // TODO (aibrahim): Consolidate config.model and config.model_reasoning_effort into
        // config.collaboration_mode to avoid extracting these fields separately and constructing
        // CollaborationMode here.
        //
        // Session mode is pinned to Swarm for all root and spawned agents.
        let mut collaboration_mode = CollaborationMode {
            mode: ModeKind::Swarm,
            settings: Settings {
                model: model.clone(),
                reasoning_effort: config.model_reasoning_effort,
                developer_instructions: None,
            },
        };
        if collaboration_mode.settings.developer_instructions.is_none() {
            if collaboration_mode.mode == ModeKind::Swarm {
                collaboration_mode.settings.developer_instructions = Some(
                    resolve_swarm_prompt(&session_source, default_root_swarm_is_complex())
                        .to_string(),
                );
            } else if let Some(instructions) = models_manager
                .list_collaboration_modes()
                .into_iter()
                .find(|preset| preset.mode == Some(collaboration_mode.mode))
                .and_then(|preset| preset.developer_instructions.flatten())
            {
                collaboration_mode.settings.developer_instructions = Some(instructions);
            }
        }
        let session_configuration = SessionConfiguration {
            provider: config.model_provider.clone(),
            collaboration_mode,
            model_reasoning_summary: config.model_reasoning_summary,
            developer_instructions: config.developer_instructions.clone(),
            user_instructions,
            personality: config.personality,
            base_instructions,
            compact_prompt: config.compact_prompt.clone(),
            approval_policy: config.permissions.approval_policy.clone(),
            sandbox_policy: config.permissions.sandbox_policy.clone(),
            windows_sandbox_level: WindowsSandboxLevel::from_config(&config),
            cwd: config.cwd.clone(),
            codex_home: config.codex_home.clone(),
            thread_name: None,
            original_config_do_not_use: Arc::clone(&config),
            session_source,
            dynamic_tools,
            persist_extended_history,
        };

        // Generate a unique ID for the lifetime of this Codex session.
        let session_source_clone = session_configuration.session_source.clone();
        let (agent_status_tx, agent_status_rx) = watch::channel(AgentStatus::PendingInit);

        let session_init_span = info_span!("session_init");
        let session = Session::new(
            session_configuration,
            config.clone(),
            auth_manager.clone(),
            models_manager.clone(),
            exec_policy,
            tx_event.clone(),
            agent_status_tx.clone(),
            conversation_history,
            session_source_clone,
            skills_manager,
            file_watcher,
            agent_control,
        )
        .instrument(session_init_span)
        .await
        .map_err(|e| {
            error!("Failed to create session: {e:#}");
            map_session_init_error(&e, &config.codex_home)
        })?;
        let thread_id = session.conversation_id;

        // This task will run until Op::Shutdown is received.
        let session_loop_span = info_span!("session_loop", thread_id = %thread_id);
        tokio::spawn(
            submission_loop(Arc::clone(&session), config, rx_sub).instrument(session_loop_span),
        );
        let codex = Codex {
            tx_sub,
            rx_event,
            agent_status: agent_status_rx,
            session,
        };

        #[allow(deprecated)]
        Ok(CodexSpawnOk {
            codex,
            thread_id,
            conversation_id: thread_id,
        })
    }

    /// Submit the `op` wrapped in a `Submission` with a unique ID.
    pub async fn submit(&self, op: Op) -> CodexResult<String> {
        let id = Uuid::now_v7().to_string();
        let sub = Submission { id: id.clone(), op };
        self.submit_with_id(sub).await?;
        Ok(id)
    }

    /// Use sparingly: prefer `submit()` so Codex is responsible for generating
    /// unique IDs for each submission.
    pub async fn submit_with_id(&self, sub: Submission) -> CodexResult<()> {
        self.tx_sub
            .send(sub)
            .await
            .map_err(|_| CodexErr::InternalAgentDied)?;
        Ok(())
    }

    pub async fn next_event(&self) -> CodexResult<Event> {
        let event = self
            .rx_event
            .recv()
            .await
            .map_err(|_| CodexErr::InternalAgentDied)?;
        Ok(event)
    }

    pub async fn steer_input(
        &self,
        input: Vec<UserInput>,
        expected_turn_id: Option<&str>,
    ) -> Result<String, SteerInputError> {
        self.session.steer_input(input, expected_turn_id).await
    }

    pub(crate) async fn agent_status(&self) -> AgentStatus {
        self.agent_status.borrow().clone()
    }

    pub(crate) async fn thread_config_snapshot(&self) -> ThreadConfigSnapshot {
        let state = self.session.state.lock().await;
        state.session_configuration.thread_config_snapshot()
    }

    pub(crate) fn state_db(&self) -> Option<state_db::StateDbHandle> {
        self.session.state_db()
    }

    pub(crate) fn enabled(&self, feature: Feature) -> bool {
        self.session.enabled(feature)
    }
}

/// Context for an initialized model agent
///
/// A session has at most 1 running task at a time, and can be interrupted by user input.
pub(crate) struct Session {
    pub(crate) conversation_id: ThreadId,
    tx_event: Sender<Event>,
    agent_status: watch::Sender<AgentStatus>,
    state: Mutex<SessionState>,
    /// The set of enabled features should be invariant for the lifetime of the
    /// session.
    features: Features,
    pending_mcp_server_refresh_config: Mutex<Option<McpServerRefreshConfig>>,
    pub(crate) active_turn: Mutex<Option<ActiveTurn>>,
    pub(crate) services: SessionServices,
    js_repl: Arc<JsReplHandle>,
    next_internal_sub_id: AtomicU64,
}
/// The context needed for a single turn of the thread.
#[derive(Debug)]
pub(crate) struct TurnContext {
    pub(crate) sub_id: String,
    pub(crate) config: Arc<Config>,
    pub(crate) auth_manager: Option<Arc<AuthManager>>,
    pub(crate) model_info: ModelInfo,
    pub(crate) otel_manager: OtelManager,
    pub(crate) provider: ModelProviderInfo,
    pub(crate) reasoning_effort: Option<ReasoningEffortConfig>,
    pub(crate) reasoning_summary: ReasoningSummaryConfig,
    pub(crate) session_source: SessionSource,
    /// The session's current working directory. All relative paths provided by
    /// the model as well as sandbox policies are resolved against this path
    /// instead of `std::env::current_dir()`.
    pub(crate) cwd: PathBuf,
    pub(crate) developer_instructions: Option<String>,
    pub(crate) compact_prompt: Option<String>,
    pub(crate) user_instructions: Option<String>,
    pub(crate) collaboration_mode: CollaborationMode,
    pub(crate) personality: Option<Personality>,
    pub(crate) approval_policy: AskForApproval,
    pub(crate) sandbox_policy: SandboxPolicy,
    pub(crate) network: Option<NetworkProxy>,
    pub(crate) windows_sandbox_level: WindowsSandboxLevel,
    pub(crate) shell_environment_policy: ShellEnvironmentPolicy,
    pub(crate) tools_config: ToolsConfig,
    pub(crate) features: Features,
    pub(crate) ghost_snapshot: GhostSnapshotConfig,
    pub(crate) final_output_json_schema: Option<Value>,
    pub(crate) codex_linux_sandbox_exe: Option<PathBuf>,
    pub(crate) tool_call_gate: Arc<ReadinessFlag>,
    pub(crate) truncation_policy: TruncationPolicy,
    pub(crate) js_repl: Arc<JsReplHandle>,
    pub(crate) dynamic_tools: Vec<DynamicToolSpec>,
    pub(crate) turn_metadata_state: Arc<TurnMetadataState>,
}
impl TurnContext {
    pub(crate) fn model_context_window(&self) -> Option<i64> {
        let effective_context_window_percent = self.model_info.effective_context_window_percent;
        self.model_info.context_window.map(|context_window| {
            context_window.saturating_mul(effective_context_window_percent) / 100
        })
    }

    pub(crate) async fn with_model(&self, model: String, models_manager: &ModelsManager) -> Self {
        let mut config = (*self.config).clone();
        config.model = Some(model.clone());
        let model_info = models_manager.get_model_info(model.as_str(), &config).await;
        let truncation_policy = model_info.truncation_policy.into();
        let supported_reasoning_levels = model_info
            .supported_reasoning_levels
            .iter()
            .map(|preset| preset.effort)
            .collect::<Vec<_>>();
        let reasoning_effort = if let Some(current_reasoning_effort) = self.reasoning_effort {
            if supported_reasoning_levels.contains(&current_reasoning_effort) {
                Some(current_reasoning_effort)
            } else {
                supported_reasoning_levels
                    .get(supported_reasoning_levels.len().saturating_sub(1) / 2)
                    .copied()
                    .or(model_info.default_reasoning_level)
            }
        } else {
            supported_reasoning_levels
                .get(supported_reasoning_levels.len().saturating_sub(1) / 2)
                .copied()
                .or(model_info.default_reasoning_level)
        };
        config.model_reasoning_effort = reasoning_effort;

        let collaboration_mode =
            self.collaboration_mode
                .with_updates(Some(model.clone()), Some(reasoning_effort), None);
        let features = self.features.clone();
        let tools_config = ToolsConfig::new(&ToolsConfigParams {
            model_info: &model_info,
            features: &features,
            web_search_mode: self.tools_config.web_search_mode,
            mode_kind: collaboration_mode.mode,
        });

        Self {
            sub_id: self.sub_id.clone(),
            config: Arc::new(config),
            auth_manager: self.auth_manager.clone(),
            model_info: model_info.clone(),
            otel_manager: self
                .otel_manager
                .clone()
                .with_model(model.as_str(), model_info.slug.as_str()),
            provider: self.provider.clone(),
            reasoning_effort,
            reasoning_summary: self.reasoning_summary,
            session_source: self.session_source.clone(),
            cwd: self.cwd.clone(),
            developer_instructions: self.developer_instructions.clone(),
            compact_prompt: self.compact_prompt.clone(),
            user_instructions: self.user_instructions.clone(),
            collaboration_mode,
            personality: self.personality,
            approval_policy: self.approval_policy,
            sandbox_policy: self.sandbox_policy.clone(),
            network: self.network.clone(),
            windows_sandbox_level: self.windows_sandbox_level,
            shell_environment_policy: self.shell_environment_policy.clone(),
            tools_config,
            features,
            ghost_snapshot: self.ghost_snapshot.clone(),
            final_output_json_schema: self.final_output_json_schema.clone(),
            codex_linux_sandbox_exe: self.codex_linux_sandbox_exe.clone(),
            tool_call_gate: Arc::new(ReadinessFlag::new()),
            truncation_policy,
            js_repl: Arc::clone(&self.js_repl),
            dynamic_tools: self.dynamic_tools.clone(),
            turn_metadata_state: self.turn_metadata_state.clone(),
        }
    }

    pub(crate) fn resolve_path(&self, path: Option<String>) -> PathBuf {
        path.as_ref()
            .map(PathBuf::from)
            .map_or_else(|| self.cwd.clone(), |p| self.cwd.join(p))
    }

    pub(crate) fn compact_prompt(&self) -> &str {
        self.compact_prompt
            .as_deref()
            .unwrap_or(compact::SUMMARIZATION_PROMPT)
    }

    pub(crate) fn to_turn_context_item(
        &self,
        collaboration_mode: CollaborationMode,
    ) -> TurnContextItem {
        TurnContextItem {
            turn_id: Some(self.sub_id.clone()),
            cwd: self.cwd.clone(),
            approval_policy: self.approval_policy,
            sandbox_policy: self.sandbox_policy.clone(),
            network: self.turn_context_network_item(),
            model: self.model_info.slug.clone(),
            personality: self.personality,
            collaboration_mode: Some(collaboration_mode),
            effort: self.reasoning_effort,
            summary: self.reasoning_summary,
            user_instructions: self.user_instructions.clone(),
            developer_instructions: self.developer_instructions.clone(),
            final_output_json_schema: self.final_output_json_schema.clone(),
            truncation_policy: Some(self.truncation_policy.into()),
        }
    }

    fn turn_context_network_item(&self) -> Option<TurnContextNetworkItem> {
        let network = self
            .config
            .config_layer_stack
            .requirements()
            .network
            .as_ref()?;
        Some(TurnContextNetworkItem {
            allowed_domains: network.allowed_domains.clone().unwrap_or_default(),
            denied_domains: network.denied_domains.clone().unwrap_or_default(),
        })
    }
}

#[derive(Clone)]
pub(crate) struct SessionConfiguration {
    /// Provider identifier ("openai", "openrouter", ...).
    provider: ModelProviderInfo,

    collaboration_mode: CollaborationMode,
    model_reasoning_summary: ReasoningSummaryConfig,

    /// Developer instructions that supplement the base instructions.
    developer_instructions: Option<String>,

    /// Model instructions that are appended to the base instructions.
    user_instructions: Option<String>,

    /// Personality preference for the model.
    personality: Option<Personality>,

    /// Base instructions for the session.
    base_instructions: String,

    /// Compact prompt override.
    compact_prompt: Option<String>,

    /// When to escalate for approval for execution
    approval_policy: Constrained<AskForApproval>,
    /// How to sandbox commands executed in the system
    sandbox_policy: Constrained<SandboxPolicy>,
    windows_sandbox_level: WindowsSandboxLevel,

    /// Working directory that should be treated as the *root* of the
    /// session. All relative paths supplied by the model as well as the
    /// execution sandbox are resolved against this directory **instead**
    /// of the process-wide current working directory. CLI front-ends are
    /// expected to expand this to an absolute path before sending the
    /// `ConfigureSession` operation so that the business-logic layer can
    /// operate deterministically.
    cwd: PathBuf,
    /// Directory containing all Codex state for this session.
    codex_home: PathBuf,
    /// Optional user-facing name for the thread, updated during the session.
    thread_name: Option<String>,

    // TODO(pakrym): Remove config from here
    original_config_do_not_use: Arc<Config>,
    /// Source of the session (cli, vscode, exec, mcp, ...)
    session_source: SessionSource,
    dynamic_tools: Vec<DynamicToolSpec>,
    persist_extended_history: bool,
}

impl SessionConfiguration {
    pub(crate) fn codex_home(&self) -> &PathBuf {
        &self.codex_home
    }

    fn thread_config_snapshot(&self) -> ThreadConfigSnapshot {
        ThreadConfigSnapshot {
            model: self.collaboration_mode.model().to_string(),
            model_provider_id: self.original_config_do_not_use.model_provider_id.clone(),
            approval_policy: self.approval_policy.value(),
            sandbox_policy: self.sandbox_policy.get().clone(),
            cwd: self.cwd.clone(),
            reasoning_effort: self.collaboration_mode.reasoning_effort(),
            personality: self.personality,
            session_source: self.session_source.clone(),
        }
    }

    pub(crate) fn apply(&self, updates: &SessionSettingsUpdate) -> ConstraintResult<Self> {
        let mut next_configuration = self.clone();
        if let Some(mut collaboration_mode) = updates.collaboration_mode.clone() {
            collaboration_mode.mode = ModeKind::Swarm;
            next_configuration.collaboration_mode = collaboration_mode;
        }
        if let Some(summary) = updates.reasoning_summary {
            next_configuration.model_reasoning_summary = summary;
        }
        if let Some(personality) = updates.personality {
            next_configuration.personality = Some(personality);
        }
        if let Some(approval_policy) = updates.approval_policy {
            next_configuration.approval_policy.set(approval_policy)?;
        }
        if let Some(sandbox_policy) = updates.sandbox_policy.clone() {
            next_configuration.sandbox_policy.set(sandbox_policy)?;
        }
        if let Some(windows_sandbox_level) = updates.windows_sandbox_level {
            next_configuration.windows_sandbox_level = windows_sandbox_level;
        }
        if let Some(cwd) = updates.cwd.clone() {
            next_configuration.cwd = cwd;
        }
        Ok(next_configuration)
    }
}

#[derive(Default, Clone)]
pub(crate) struct SessionSettingsUpdate {
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) approval_policy: Option<AskForApproval>,
    pub(crate) sandbox_policy: Option<SandboxPolicy>,
    pub(crate) windows_sandbox_level: Option<WindowsSandboxLevel>,
    pub(crate) collaboration_mode: Option<CollaborationMode>,
    pub(crate) reasoning_summary: Option<ReasoningSummaryConfig>,
    pub(crate) final_output_json_schema: Option<Option<Value>>,
    pub(crate) personality: Option<Personality>,
}

fn should_sync_swarm_collaboration_mode(
    previous: &CollaborationMode,
    next: &CollaborationMode,
) -> bool {
    previous != next && next.mode == ModeKind::Swarm
}

impl Session {
    /// Builds the `x-codex-beta-features` header value for this session.
    ///
    /// `ModelClient` is session-scoped and intentionally does not depend on the full `Config`, so
    /// we precompute the comma-separated list of enabled experimental feature keys at session
    /// creation time and thread it into the client.
    fn build_model_client_beta_features_header(config: &Config) -> Option<String> {
        let beta_features_header = FEATURES
            .iter()
            .filter_map(|spec| {
                if spec.stage.experimental_menu_description().is_some()
                    && config.features.enabled(spec.id)
                {
                    Some(spec.key)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join(",");

        if beta_features_header.is_empty() {
            None
        } else {
            Some(beta_features_header)
        }
    }

    async fn start_managed_network_proxy(
        spec: &crate::config::NetworkProxySpec,
        sandbox_policy: &SandboxPolicy,
        network_policy_decider: Option<Arc<dyn codex_network_proxy::NetworkPolicyDecider>>,
        blocked_request_observer: Option<Arc<dyn codex_network_proxy::BlockedRequestObserver>>,
        managed_network_requirements_enabled: bool,
    ) -> anyhow::Result<(StartedNetworkProxy, SessionNetworkProxyRuntime)> {
        let network_proxy = spec
            .start_proxy(
                sandbox_policy,
                network_policy_decider,
                blocked_request_observer,
                managed_network_requirements_enabled,
            )
            .await
            .map_err(|err| anyhow::anyhow!("failed to start managed network proxy: {err}"))?;
        let session_network_proxy = {
            let proxy = network_proxy.proxy();
            SessionNetworkProxyRuntime {
                http_addr: proxy.http_addr().to_string(),
                socks_addr: proxy.socks_addr().to_string(),
                admin_addr: proxy.admin_addr().to_string(),
            }
        };
        Ok((network_proxy, session_network_proxy))
    }

    /// Don't expand the number of mutated arguments on config. We are in the process of getting rid of it.
    pub(crate) fn build_per_turn_config(session_configuration: &SessionConfiguration) -> Config {
        // todo(aibrahim): store this state somewhere else so we don't need to mut config
        let config = session_configuration.original_config_do_not_use.clone();
        let mut per_turn_config = (*config).clone();
        per_turn_config.model_reasoning_effort =
            session_configuration.collaboration_mode.reasoning_effort();
        per_turn_config.model_reasoning_summary = session_configuration.model_reasoning_summary;
        per_turn_config.personality = session_configuration.personality;
        let resolved_web_search_mode = resolve_web_search_mode_for_turn(
            &per_turn_config.web_search_mode,
            session_configuration.sandbox_policy.get(),
        );
        if let Err(err) = per_turn_config
            .web_search_mode
            .set(resolved_web_search_mode)
        {
            let fallback_value = per_turn_config.web_search_mode.value();
            tracing::warn!(
                error = %err,
                ?resolved_web_search_mode,
                ?fallback_value,
                "resolved web_search_mode is disallowed by requirements; keeping constrained value"
            );
        }
        per_turn_config.features = config.features.clone();
        per_turn_config
    }

    pub(crate) async fn codex_home(&self) -> PathBuf {
        let state = self.state.lock().await;
        state.session_configuration.codex_home().clone()
    }

    fn start_file_watcher_listener(self: &Arc<Self>) {
        let mut rx = self.services.file_watcher.subscribe();
        let weak_sess = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(FileWatcherEvent::SkillsChanged { .. }) => {
                        let Some(sess) = weak_sess.upgrade() else {
                            break;
                        };
                        let event = Event {
                            id: sess.next_internal_sub_id(),
                            msg: EventMsg::SkillsUpdateAvailable,
                        };
                        sess.send_event_raw(event).await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn make_turn_context(
        auth_manager: Option<Arc<AuthManager>>,
        otel_manager: &OtelManager,
        provider: ModelProviderInfo,
        session_configuration: &SessionConfiguration,
        per_turn_config: Config,
        model_info: ModelInfo,
        network: Option<NetworkProxy>,
        sub_id: String,
        js_repl: Arc<JsReplHandle>,
    ) -> TurnContext {
        let reasoning_effort = session_configuration.collaboration_mode.reasoning_effort();
        let reasoning_summary = session_configuration.model_reasoning_summary;
        let otel_manager = otel_manager.clone().with_model(
            session_configuration.collaboration_mode.model(),
            model_info.slug.as_str(),
        );
        let session_source = session_configuration.session_source.clone();
        let auth_manager_for_context = auth_manager;
        let provider_for_context = provider;
        let otel_manager_for_context = otel_manager;
        let per_turn_config = Arc::new(per_turn_config);

        let tools_config = ToolsConfig::new(&ToolsConfigParams {
            model_info: &model_info,
            features: &per_turn_config.features,
            web_search_mode: Some(per_turn_config.web_search_mode.value()),
            mode_kind: session_configuration.collaboration_mode.mode,
        });

        let cwd = session_configuration.cwd.clone();
        let turn_metadata_state = Arc::new(TurnMetadataState::new(
            sub_id.clone(),
            cwd.clone(),
            session_configuration.sandbox_policy.get(),
            session_configuration.windows_sandbox_level,
            per_turn_config
                .features
                .enabled(Feature::UseLinuxSandboxBwrap),
        ));
        TurnContext {
            sub_id,
            config: per_turn_config.clone(),
            auth_manager: auth_manager_for_context,
            model_info: model_info.clone(),
            otel_manager: otel_manager_for_context,
            provider: provider_for_context,
            reasoning_effort,
            reasoning_summary,
            session_source,
            cwd,
            developer_instructions: session_configuration.developer_instructions.clone(),
            compact_prompt: session_configuration.compact_prompt.clone(),
            user_instructions: session_configuration.user_instructions.clone(),
            collaboration_mode: session_configuration.collaboration_mode.clone(),
            personality: session_configuration.personality,
            approval_policy: session_configuration.approval_policy.value(),
            sandbox_policy: session_configuration.sandbox_policy.get().clone(),
            network,
            windows_sandbox_level: session_configuration.windows_sandbox_level,
            shell_environment_policy: per_turn_config.permissions.shell_environment_policy.clone(),
            tools_config,
            features: per_turn_config.features.clone(),
            ghost_snapshot: per_turn_config.ghost_snapshot.clone(),
            final_output_json_schema: None,
            codex_linux_sandbox_exe: per_turn_config.codex_linux_sandbox_exe.clone(),
            tool_call_gate: Arc::new(ReadinessFlag::new()),
            truncation_policy: model_info.truncation_policy.into(),
            js_repl,
            dynamic_tools: session_configuration.dynamic_tools.clone(),
            turn_metadata_state,
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn new(
        mut session_configuration: SessionConfiguration,
        config: Arc<Config>,
        auth_manager: Arc<AuthManager>,
        models_manager: Arc<ModelsManager>,
        exec_policy: ExecPolicyManager,
        tx_event: Sender<Event>,
        agent_status: watch::Sender<AgentStatus>,
        initial_history: InitialHistory,
        session_source: SessionSource,
        skills_manager: Arc<SkillsManager>,
        file_watcher: Arc<FileWatcher>,
        agent_control: AgentControl,
    ) -> anyhow::Result<Arc<Self>> {
        debug!(
            "Configuring session: model={}; provider={:?}",
            session_configuration.collaboration_mode.model(),
            session_configuration.provider
        );
        if !session_configuration.cwd.is_absolute() {
            return Err(anyhow::anyhow!(
                "cwd is not absolute: {:?}",
                session_configuration.cwd
            ));
        }

        let forked_from_id = initial_history.forked_from_id();

        let (conversation_id, rollout_params) = match &initial_history {
            InitialHistory::New | InitialHistory::Forked(_) => {
                let conversation_id = ThreadId::default();
                (
                    conversation_id,
                    RolloutRecorderParams::new(
                        conversation_id,
                        forked_from_id,
                        session_source,
                        BaseInstructions {
                            text: session_configuration.base_instructions.clone(),
                        },
                        session_configuration.dynamic_tools.clone(),
                        if session_configuration.persist_extended_history {
                            EventPersistenceMode::Extended
                        } else {
                            EventPersistenceMode::Limited
                        },
                    ),
                )
            }
            InitialHistory::Resumed(resumed_history) => (
                resumed_history.conversation_id,
                RolloutRecorderParams::resume(
                    resumed_history.rollout_path.clone(),
                    if session_configuration.persist_extended_history {
                        EventPersistenceMode::Extended
                    } else {
                        EventPersistenceMode::Limited
                    },
                ),
            ),
        };
        let state_builder = match &initial_history {
            InitialHistory::Resumed(resumed) => metadata::builder_from_items(
                resumed.history.as_slice(),
                resumed.rollout_path.as_path(),
            ),
            InitialHistory::New | InitialHistory::Forked(_) => None,
        };

        if !matches!(
            session_configuration.session_source,
            SessionSource::SubAgent(_)
        ) {
            let _ = agent_control
                .register_agent_name(conversation_id, crate::agent::PRIMARY_AGENT_NAME);
        }

        let blackboard_owner_thread_id = shared_blackboard_owner_thread_id(
            conversation_id,
            &session_configuration.session_source,
        );
        let shared_blackboard_path = blackboard::ensure_session_blackboard(
            &session_configuration.cwd,
            blackboard_owner_thread_id,
        )
        .await
        .map_err(|err| {
            anyhow::anyhow!(
                "failed to initialize shared blackboard for session {conversation_id}: {err}"
            )
        })?;

        // Kick off independent async setup tasks in parallel to reduce startup latency.
        //
        // - initialize RolloutRecorder with new or resumed session info
        // - perform default shell discovery
        // - load history metadata
        let rollout_fut = async {
            if config.ephemeral {
                Ok::<_, anyhow::Error>((None, None))
            } else {
                let state_db_ctx = state_db::init_if_enabled(&config, None).await;
                let rollout_recorder = RolloutRecorder::new(
                    &config,
                    rollout_params,
                    state_db_ctx.clone(),
                    state_builder.clone(),
                )
                .await?;
                Ok((Some(rollout_recorder), state_db_ctx))
            }
        };

        let history_meta_fut = crate::message_history::history_metadata(&config);
        let auth_manager_clone = Arc::clone(&auth_manager);
        let config_for_mcp = Arc::clone(&config);
        let auth_and_mcp_fut = async move {
            let auth = auth_manager_clone.auth().await;
            let mcp_servers = effective_mcp_servers(&config_for_mcp, auth.as_ref());
            let auth_statuses = compute_auth_statuses(
                mcp_servers.iter(),
                config_for_mcp.mcp_oauth_credentials_store_mode,
            )
            .await;
            (auth, mcp_servers, auth_statuses)
        };

        // Join all independent futures.
        let (
            rollout_recorder_and_state_db,
            (history_log_id, history_entry_count),
            (auth, mcp_servers, auth_statuses),
        ) = tokio::join!(rollout_fut, history_meta_fut, auth_and_mcp_fut);

        let (rollout_recorder, state_db_ctx) = rollout_recorder_and_state_db.map_err(|e| {
            error!("failed to initialize rollout recorder: {e:#}");
            e
        })?;
        let rollout_path = rollout_recorder
            .as_ref()
            .map(|rec| rec.rollout_path.clone());

        let mut post_session_configured_events = Vec::<Event>::new();

        for usage in config.features.legacy_feature_usages() {
            post_session_configured_events.push(Event {
                id: INITIAL_SUBMIT_ID.to_owned(),
                msg: EventMsg::DeprecationNotice(DeprecationNoticeEvent {
                    summary: usage.summary.clone(),
                    details: usage.details.clone(),
                }),
            });
        }
        if crate::config::uses_deprecated_instructions_file(&config.config_layer_stack) {
            post_session_configured_events.push(Event {
                id: INITIAL_SUBMIT_ID.to_owned(),
                msg: EventMsg::DeprecationNotice(DeprecationNoticeEvent {
                    summary: "`experimental_instructions_file` is deprecated and ignored. Use `model_instructions_file` instead."
                        .to_string(),
                    details: Some(
                        "Move the setting to `model_instructions_file` in config.toml (or under a profile) to load instructions from a file."
                            .to_string(),
                    ),
                }),
            });
        }
        for message in &config.startup_warnings {
            post_session_configured_events.push(Event {
                id: "".to_owned(),
                msg: EventMsg::Warning(WarningEvent {
                    message: message.clone(),
                }),
            });
        }
        maybe_push_unstable_features_warning(&config, &mut post_session_configured_events);
        if config.permissions.approval_policy.value() == AskForApproval::OnFailure {
            post_session_configured_events.push(Event {
                id: "".to_owned(),
                msg: EventMsg::Warning(WarningEvent {
                    message: "`on-failure` approval policy is deprecated and will be removed in a future release. Use `on-request` for interactive approvals or `never` for non-interactive runs.".to_string(),
                }),
            });
        }

        let auth = auth.as_ref();
        let auth_mode = auth.map(CodexAuth::auth_mode).map(TelemetryAuthMode::from);
        let otel_manager = OtelManager::new(
            conversation_id,
            session_configuration.collaboration_mode.model(),
            session_configuration.collaboration_mode.model(),
            auth.and_then(CodexAuth::get_account_id),
            auth.and_then(CodexAuth::get_account_email),
            auth_mode,
            crate::default_client::originator().value,
            config.otel.log_user_prompt,
            terminal::user_agent(),
            session_configuration.session_source.clone(),
        );
        config.features.emit_metrics(&otel_manager);
        otel_manager.counter(
            "codex.thread.started",
            1,
            &[(
                "is_git",
                if get_git_repo_root(&session_configuration.cwd).is_some() {
                    "true"
                } else {
                    "false"
                },
            )],
        );

        otel_manager.conversation_starts(
            config.model_provider.name.as_str(),
            session_configuration.collaboration_mode.reasoning_effort(),
            config.model_reasoning_summary,
            config.model_context_window,
            config.model_auto_compact_token_limit,
            config.permissions.approval_policy.value(),
            config.permissions.sandbox_policy.get().clone(),
            mcp_servers.keys().map(String::as_str).collect(),
            config.active_profile.clone(),
        );

        let mut default_shell = shell::default_user_shell();
        // Create the mutable state for the Session.
        let shell_snapshot_tx = if config.features.enabled(Feature::ShellSnapshot) {
            ShellSnapshot::start_snapshotting(
                config.codex_home.clone(),
                conversation_id,
                session_configuration.cwd.clone(),
                &mut default_shell,
                otel_manager.clone(),
            )
        } else {
            let (tx, rx) = watch::channel(None);
            default_shell.shell_snapshot = rx;
            tx
        };
        let thread_name =
            match session_index::find_thread_name_by_id(&config.codex_home, &conversation_id).await
            {
                Ok(name) => name,
                Err(err) => {
                    warn!("Failed to read session index for thread name: {err}");
                    None
                }
            };
        session_configuration.thread_name = thread_name.clone();
        let mut state = SessionState::new(session_configuration.clone());
        state.set_shared_blackboard_path(shared_blackboard_path);
        let managed_network_requirements_enabled = config.managed_network_requirements_enabled();
        let network_approval = Arc::new(NetworkApprovalService::default());
        // The managed proxy can call back into core for allowlist-miss decisions.
        let network_policy_decider_session = if managed_network_requirements_enabled {
            config
                .permissions
                .network
                .as_ref()
                .map(|_| Arc::new(RwLock::new(std::sync::Weak::<Session>::new())))
        } else {
            None
        };
        let blocked_request_observer = if managed_network_requirements_enabled {
            config
                .permissions
                .network
                .as_ref()
                .map(|_| build_blocked_request_observer(Arc::clone(&network_approval)))
        } else {
            None
        };
        let network_policy_decider =
            network_policy_decider_session
                .as_ref()
                .map(|network_policy_decider_session| {
                    build_network_policy_decider(
                        Arc::clone(&network_approval),
                        Arc::clone(network_policy_decider_session),
                    )
                });
        let (network_proxy, session_network_proxy) =
            if let Some(spec) = config.permissions.network.as_ref() {
                let (network_proxy, session_network_proxy) = Self::start_managed_network_proxy(
                    spec,
                    config.permissions.sandbox_policy.get(),
                    network_policy_decider.as_ref().map(Arc::clone),
                    blocked_request_observer.as_ref().map(Arc::clone),
                    managed_network_requirements_enabled,
                )
                .await?;
                (Some(network_proxy), Some(session_network_proxy))
            } else {
                (None, None)
            };

        let services = SessionServices {
            mcp_connection_manager: Arc::new(RwLock::new(McpConnectionManager::default())),
            mcp_startup_cancellation_token: Mutex::new(CancellationToken::new()),
            unified_exec_manager: UnifiedExecProcessManager::default(),
            analytics_events_client: AnalyticsEventsClient::new(
                Arc::clone(&config),
                Arc::clone(&auth_manager),
            ),
            hooks: Hooks::new(HooksConfig {
                legacy_notify_argv: config.notify.clone(),
            }),
            rollout: Mutex::new(rollout_recorder),
            user_shell: Arc::new(default_shell),
            shell_snapshot_tx,
            show_raw_agent_reasoning: config.show_raw_agent_reasoning,
            exec_policy,
            auth_manager: Arc::clone(&auth_manager),
            otel_manager,
            models_manager: Arc::clone(&models_manager),
            tool_approvals: Mutex::new(ApprovalStore::default()),
            skills_manager,
            file_watcher,
            agent_control,
            network_proxy,
            network_approval: Arc::clone(&network_approval),
            state_db: state_db_ctx.clone(),
            model_client: ModelClient::new(
                Some(Arc::clone(&auth_manager)),
                conversation_id,
                session_configuration.provider.clone(),
                session_configuration.session_source.clone(),
                config.model_verbosity,
                config.features.enabled(Feature::ResponsesWebsockets)
                    || config.features.enabled(Feature::ResponsesWebsocketsV2),
                config.features.enabled(Feature::ResponsesWebsocketsV2),
                config.features.enabled(Feature::EnableRequestCompression),
                config.features.enabled(Feature::RuntimeMetrics),
                Self::build_model_client_beta_features_header(config.as_ref()),
                model_io_debug_dir(config.as_ref()),
            ),
        };
        let js_repl = Arc::new(JsReplHandle::with_node_path(
            config.js_repl_node_path.clone(),
            config.codex_home.clone(),
        ));

        let prewarm_model_info = models_manager
            .get_model_info(session_configuration.collaboration_mode.model(), &config)
            .await;
        let startup_regular_task = RegularTask::with_startup_prewarm(
            services.model_client.clone(),
            services.otel_manager.clone(),
            prewarm_model_info,
        );
        state.set_startup_regular_task(startup_regular_task);

        let sess = Arc::new(Session {
            conversation_id,
            tx_event: tx_event.clone(),
            agent_status,
            state: Mutex::new(state),
            features: config.features.clone(),
            pending_mcp_server_refresh_config: Mutex::new(None),
            active_turn: Mutex::new(None),
            services,
            js_repl,
            next_internal_sub_id: AtomicU64::new(0),
        });
        if let Some(network_policy_decider_session) = network_policy_decider_session {
            let mut guard = network_policy_decider_session.write().await;
            *guard = Arc::downgrade(&sess);
        }

        // Dispatch the SessionConfiguredEvent first and then report any errors.
        // If resuming, include converted initial messages in the payload so UIs can render them immediately.
        let initial_messages = initial_history.get_event_msgs();
        let events = std::iter::once(Event {
            id: INITIAL_SUBMIT_ID.to_owned(),
            msg: EventMsg::SessionConfigured(SessionConfiguredEvent {
                session_id: conversation_id,
                forked_from_id,
                thread_name: session_configuration.thread_name.clone(),
                model: session_configuration.collaboration_mode.model().to_string(),
                model_provider_id: config.model_provider_id.clone(),
                approval_policy: session_configuration.approval_policy.value(),
                sandbox_policy: session_configuration.sandbox_policy.get().clone(),
                cwd: session_configuration.cwd.clone(),
                reasoning_effort: session_configuration.collaboration_mode.reasoning_effort(),
                history_log_id,
                history_entry_count,
                initial_messages,
                network_proxy: session_network_proxy,
                rollout_path,
            }),
        })
        .chain(post_session_configured_events.into_iter());
        for event in events {
            sess.send_event_raw(event).await;
        }

        // Start the watcher after SessionConfigured so it cannot emit earlier events.
        sess.start_file_watcher_listener();

        // Construct sandbox_state before initialize() so it can be sent to each
        // MCP server immediately after it becomes ready (avoiding blocking).
        let sandbox_state = SandboxState {
            sandbox_policy: session_configuration.sandbox_policy.get().clone(),
            codex_linux_sandbox_exe: config.codex_linux_sandbox_exe.clone(),
            sandbox_cwd: session_configuration.cwd.clone(),
            use_linux_sandbox_bwrap: config.features.enabled(Feature::UseLinuxSandboxBwrap),
        };
        let mut required_mcp_servers: Vec<String> = mcp_servers
            .iter()
            .filter(|(_, server)| server.enabled && server.required)
            .map(|(name, _)| name.clone())
            .collect();
        required_mcp_servers.sort();
        let cancel_token = sess.mcp_startup_cancellation_token().await;

        sess.services
            .mcp_connection_manager
            .write()
            .await
            .initialize(
                &mcp_servers,
                config.mcp_oauth_credentials_store_mode,
                auth_statuses.clone(),
                tx_event.clone(),
                cancel_token,
                sandbox_state,
            )
            .await;
        if !required_mcp_servers.is_empty() {
            let failures = sess
                .services
                .mcp_connection_manager
                .read()
                .await
                .required_startup_failures(&required_mcp_servers)
                .await;
            if !failures.is_empty() {
                let details = failures
                    .iter()
                    .map(|failure| format!("{}: {}", failure.server, failure.error))
                    .collect::<Vec<_>>()
                    .join("; ");
                return Err(anyhow::anyhow!(
                    "required MCP servers failed to initialize: {details}"
                ));
            }
        }

        // record_initial_history can emit events. We record only after the SessionConfiguredEvent is emitted.
        sess.record_initial_history(initial_history).await;

        memories::start_memories_startup_task(
            &sess,
            Arc::clone(&config),
            &session_configuration.session_source,
        );

        Ok(sess)
    }

    pub(crate) fn get_tx_event(&self) -> Sender<Event> {
        self.tx_event.clone()
    }

    pub(crate) fn state_db(&self) -> Option<state_db::StateDbHandle> {
        self.services.state_db.clone()
    }

    /// Ensure all rollout writes are durably flushed.
    pub(crate) async fn flush_rollout(&self) {
        let recorder = {
            let guard = self.services.rollout.lock().await;
            guard.clone()
        };
        if let Some(rec) = recorder
            && let Err(e) = rec.flush().await
        {
            warn!("failed to flush rollout recorder: {e}");
        }
    }

    pub(crate) async fn ensure_rollout_materialized(&self) {
        let recorder = {
            let guard = self.services.rollout.lock().await;
            guard.clone()
        };
        if let Some(rec) = recorder
            && let Err(e) = rec.persist().await
        {
            warn!("failed to materialize rollout recorder: {e}");
        }
    }

    fn next_internal_sub_id(&self) -> String {
        let id = self
            .next_internal_sub_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        format!("auto-compact-{id}")
    }

    pub(crate) async fn get_total_token_usage(&self) -> i64 {
        let state = self.state.lock().await;
        state.get_total_token_usage(state.server_reasoning_included())
    }

    pub(crate) async fn get_total_token_usage_breakdown(&self) -> TotalTokenUsageBreakdown {
        let state = self.state.lock().await;
        state.history.get_total_token_usage_breakdown()
    }

    pub(crate) async fn get_estimated_token_count(
        &self,
        turn_context: &TurnContext,
    ) -> Option<i64> {
        let state = self.state.lock().await;
        state.history.estimate_token_count(turn_context)
    }

    pub(crate) async fn get_base_instructions(&self) -> BaseInstructions {
        let state = self.state.lock().await;
        BaseInstructions {
            text: state.session_configuration.base_instructions.clone(),
        }
    }

    pub(crate) async fn merge_mcp_tool_selection(&self, tool_names: Vec<String>) -> Vec<String> {
        let mut state = self.state.lock().await;
        state.merge_mcp_tool_selection(tool_names)
    }

    pub(crate) async fn get_mcp_tool_selection(&self) -> Option<Vec<String>> {
        let state = self.state.lock().await;
        state.get_mcp_tool_selection()
    }

    pub(crate) async fn clear_mcp_tool_selection(&self) {
        let mut state = self.state.lock().await;
        state.clear_mcp_tool_selection();
    }

    // Merges connector IDs into the session-level explicit connector selection.
    pub(crate) async fn merge_connector_selection(
        &self,
        connector_ids: HashSet<String>,
    ) -> HashSet<String> {
        let mut state = self.state.lock().await;
        state.merge_connector_selection(connector_ids)
    }

    // Returns the connector IDs currently selected for this session.
    pub(crate) async fn get_connector_selection(&self) -> HashSet<String> {
        let state = self.state.lock().await;
        state.get_connector_selection()
    }

    // Clears connector IDs that were accumulated for explicit selection.
    pub(crate) async fn clear_connector_selection(&self) {
        let mut state = self.state.lock().await;
        state.clear_connector_selection();
    }

    pub(crate) async fn push_recent_real_user_input(&self, input: String) {
        let mut state = self.state.lock().await;
        state.push_recent_real_user_input(input);
    }

    pub(crate) async fn recent_real_user_inputs(&self) -> Vec<String> {
        let state = self.state.lock().await;
        state.recent_real_user_inputs()
    }

    async fn record_initial_history(&self, conversation_history: InitialHistory) {
        let turn_context = self.new_default_turn().await;
        match conversation_history {
            InitialHistory::New => {
                // Build and record initial items (user instructions + environment context)
                let items = self.build_initial_context(&turn_context).await;
                self.record_conversation_items(&turn_context, &items).await;
                {
                    let mut state = self.state.lock().await;
                    state.initial_context_seeded = true;
                }
                self.set_previous_model(None).await;
                // Ensure initial items are visible to immediate readers (e.g., tests, forks).
                self.flush_rollout().await;
            }
            InitialHistory::Resumed(resumed_history) => {
                let rollout_items = resumed_history.history;
                let previous_model = Self::last_rollout_model_name(&rollout_items)
                    .map(std::string::ToString::to_string);
                {
                    let mut state = self.state.lock().await;
                    state.initial_context_seeded = false;
                }
                self.set_previous_model(previous_model).await;

                // If resuming, warn when the last recorded model differs from the current one.
                let curr = turn_context.model_info.slug.as_str();
                if let Some(prev) =
                    Self::last_rollout_model_name(&rollout_items).filter(|p| *p != curr)
                {
                    warn!("resuming session with different model: previous={prev}, current={curr}");
                    self.send_event(
                        &turn_context,
                        EventMsg::Warning(WarningEvent {
                            message: format!(
                                "This session was recorded with model `{prev}` but is resuming with `{curr}`. \
                         Consider switching back to `{prev}` as it may affect Codex performance."
                            ),
                        }),
                    )
                    .await;
                }

                // Always add response items to conversation history
                let reconstructed_history = self
                    .reconstruct_history_from_rollout(&turn_context, &rollout_items)
                    .await;
                if !reconstructed_history.is_empty() {
                    self.record_into_history(&reconstructed_history, &turn_context)
                        .await;
                }

                // Seed usage info from the recorded rollout so UIs can show token counts
                // immediately on resume/fork.
                if let Some(info) = Self::last_token_info_from_rollout(&rollout_items) {
                    let mut state = self.state.lock().await;
                    state.set_token_info(Some(info));
                }

                // Defer seeding the session's initial context until the first turn starts so
                // turn/start overrides can be merged before we write to the rollout.
                self.flush_rollout().await;
            }
            InitialHistory::Forked(rollout_items) => {
                let previous_model = Self::last_rollout_model_name(&rollout_items)
                    .map(std::string::ToString::to_string);
                self.set_previous_model(previous_model).await;

                // Always add response items to conversation history
                let reconstructed_history = self
                    .reconstruct_history_from_rollout(&turn_context, &rollout_items)
                    .await;
                if !reconstructed_history.is_empty() {
                    self.record_into_history(&reconstructed_history, &turn_context)
                        .await;
                }

                // Seed usage info from the recorded rollout so UIs can show token counts
                // immediately on resume/fork.
                if let Some(info) = Self::last_token_info_from_rollout(&rollout_items) {
                    let mut state = self.state.lock().await;
                    state.set_token_info(Some(info));
                }

                // If persisting, persist all rollout items as-is (recorder filters)
                if !rollout_items.is_empty() {
                    self.persist_rollout_items(&rollout_items).await;
                }

                // Append the current session's initial context after the reconstructed history.
                let initial_context = self.build_initial_context(&turn_context).await;
                self.record_conversation_items(&turn_context, &initial_context)
                    .await;
                {
                    let mut state = self.state.lock().await;
                    state.initial_context_seeded = true;
                }

                // Forked threads should remain file-backed immediately after startup.
                self.ensure_rollout_materialized().await;

                // Flush after seeding history and any persisted rollout copy.
                self.flush_rollout().await;
            }
        }
    }

    fn last_rollout_model_name(rollout_items: &[RolloutItem]) -> Option<&str> {
        rollout_items.iter().rev().find_map(|it| {
            if let RolloutItem::TurnContext(ctx) = it {
                Some(ctx.model.as_str())
            } else {
                None
            }
        })
    }

    fn last_token_info_from_rollout(rollout_items: &[RolloutItem]) -> Option<TokenUsageInfo> {
        rollout_items.iter().rev().find_map(|item| match item {
            RolloutItem::EventMsg(EventMsg::TokenCount(ev)) => ev.info.clone(),
            _ => None,
        })
    }

    async fn previous_model(&self) -> Option<String> {
        let state = self.state.lock().await;
        state.previous_model()
    }

    pub(crate) async fn set_previous_model(&self, previous_model: Option<String>) {
        let mut state = self.state.lock().await;
        state.set_previous_model(previous_model);
    }

    fn maybe_refresh_shell_snapshot_for_cwd(
        &self,
        previous_cwd: &Path,
        next_cwd: &Path,
        codex_home: &Path,
    ) {
        if previous_cwd == next_cwd {
            return;
        }

        if !self.features.enabled(Feature::ShellSnapshot) {
            return;
        }

        ShellSnapshot::refresh_snapshot(
            codex_home.to_path_buf(),
            self.conversation_id,
            next_cwd.to_path_buf(),
            self.services.user_shell.as_ref().clone(),
            self.services.shell_snapshot_tx.clone(),
            self.services.otel_manager.clone(),
        );
    }

    pub(crate) async fn update_settings(
        &self,
        updates: SessionSettingsUpdate,
    ) -> ConstraintResult<()> {
        let mut state = self.state.lock().await;

        match state.session_configuration.apply(&updates) {
            Ok(updated) => {
                let previous_cwd = state.session_configuration.cwd.clone();
                let next_cwd = updated.cwd.clone();
                let codex_home = updated.codex_home.clone();
                let collaboration_mode_to_sync = should_sync_swarm_collaboration_mode(
                    &state.session_configuration.collaboration_mode,
                    &updated.collaboration_mode,
                )
                .then_some(updated.collaboration_mode.clone());
                state.session_configuration = updated;
                drop(state);

                self.maybe_refresh_shell_snapshot_for_cwd(&previous_cwd, &next_cwd, &codex_home);
                if let Some(collaboration_mode) = collaboration_mode_to_sync {
                    self.sync_swarm_collaboration_mode_for_spawned_agents(collaboration_mode)
                        .await;
                }

                Ok(())
            }
            Err(err) => {
                warn!("rejected session settings update: {err}");
                Err(err)
            }
        }
    }

    pub(crate) async fn new_turn_with_sub_id(
        &self,
        sub_id: String,
        updates: SessionSettingsUpdate,
    ) -> ConstraintResult<Arc<TurnContext>> {
        let (
            session_configuration,
            sandbox_policy_changed,
            previous_cwd,
            codex_home,
            collaboration_mode_to_sync,
        ) = {
            let mut state = self.state.lock().await;
            match state.session_configuration.clone().apply(&updates) {
                Ok(next) => {
                    let previous_cwd = state.session_configuration.cwd.clone();
                    let sandbox_policy_changed =
                        state.session_configuration.sandbox_policy != next.sandbox_policy;
                    let codex_home = next.codex_home.clone();
                    let collaboration_mode_to_sync = should_sync_swarm_collaboration_mode(
                        &state.session_configuration.collaboration_mode,
                        &next.collaboration_mode,
                    )
                    .then_some(next.collaboration_mode.clone());
                    state.session_configuration = next.clone();
                    (
                        next,
                        sandbox_policy_changed,
                        previous_cwd,
                        codex_home,
                        collaboration_mode_to_sync,
                    )
                }
                Err(err) => {
                    drop(state);
                    self.send_event_raw(Event {
                        id: sub_id.clone(),
                        msg: EventMsg::Error(ErrorEvent {
                            message: err.to_string(),
                            codex_error_info: Some(CodexErrorInfo::BadRequest),
                        }),
                    })
                    .await;
                    return Err(err);
                }
            }
        };

        self.maybe_refresh_shell_snapshot_for_cwd(
            &previous_cwd,
            &session_configuration.cwd,
            &codex_home,
        );
        if let Some(collaboration_mode) = collaboration_mode_to_sync {
            self.sync_swarm_collaboration_mode_for_spawned_agents(collaboration_mode)
                .await;
        }

        Ok(self
            .new_turn_from_configuration(
                sub_id,
                session_configuration,
                updates.final_output_json_schema,
                sandbox_policy_changed,
            )
            .await)
    }

    async fn sync_swarm_collaboration_mode_for_spawned_agents(
        &self,
        collaboration_mode: CollaborationMode,
    ) {
        let sync_result = self
            .services
            .agent_control
            .set_spawned_agents_collaboration_mode(self.conversation_id, collaboration_mode)
            .await;
        match sync_result {
            Ok(failures) => {
                for (thread_id, err) in failures {
                    warn!(
                        "failed to sync Swarm collaboration mode to spawned agent {thread_id}: {err}"
                    );
                }
            }
            Err(err) => {
                warn!("failed to sync Swarm collaboration mode to spawned agents: {err}");
            }
        }
    }

    async fn new_turn_from_configuration(
        &self,
        sub_id: String,
        session_configuration: SessionConfiguration,
        final_output_json_schema: Option<Option<Value>>,
        sandbox_policy_changed: bool,
    ) -> Arc<TurnContext> {
        let per_turn_config = Self::build_per_turn_config(&session_configuration);

        if sandbox_policy_changed {
            let sandbox_state = SandboxState {
                sandbox_policy: per_turn_config.permissions.sandbox_policy.get().clone(),
                codex_linux_sandbox_exe: per_turn_config.codex_linux_sandbox_exe.clone(),
                sandbox_cwd: per_turn_config.cwd.clone(),
                use_linux_sandbox_bwrap: per_turn_config
                    .features
                    .enabled(Feature::UseLinuxSandboxBwrap),
            };
            if let Err(e) = self
                .services
                .mcp_connection_manager
                .read()
                .await
                .notify_sandbox_state_change(&sandbox_state)
                .await
            {
                warn!("Failed to notify sandbox state change to MCP servers: {e:#}");
            }
        }

        let model_info = self
            .services
            .models_manager
            .get_model_info(
                session_configuration.collaboration_mode.model(),
                &per_turn_config,
            )
            .await;
        let mut turn_context: TurnContext = Self::make_turn_context(
            Some(Arc::clone(&self.services.auth_manager)),
            &self.services.otel_manager,
            session_configuration.provider.clone(),
            &session_configuration,
            per_turn_config,
            model_info,
            self.services
                .network_proxy
                .as_ref()
                .map(StartedNetworkProxy::proxy),
            sub_id,
            Arc::clone(&self.js_repl),
        );

        if let Some(final_schema) = final_output_json_schema {
            turn_context.final_output_json_schema = final_schema;
        }
        let turn_context = Arc::new(turn_context);
        turn_context.turn_metadata_state.spawn_git_enrichment_task();
        turn_context
    }

    pub(crate) async fn new_default_turn(&self) -> Arc<TurnContext> {
        self.new_default_turn_with_sub_id(self.next_internal_sub_id())
            .await
    }

    pub(crate) async fn take_startup_regular_task(&self) -> Option<RegularTask> {
        let mut state = self.state.lock().await;
        state.take_startup_regular_task()
    }

    async fn get_config(&self) -> std::sync::Arc<Config> {
        let state = self.state.lock().await;
        state
            .session_configuration
            .original_config_do_not_use
            .clone()
    }

    pub(crate) async fn reload_user_config_layer(&self) {
        let config_toml_path = {
            let state = self.state.lock().await;
            state
                .session_configuration
                .codex_home
                .join(CONFIG_TOML_FILE)
        };

        let user_config = match std::fs::read_to_string(&config_toml_path) {
            Ok(contents) => match toml::from_str::<toml::Value>(&contents) {
                Ok(config) => config,
                Err(err) => {
                    warn!("failed to parse user config while reloading layer: {err}");
                    return;
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                toml::Value::Table(Default::default())
            }
            Err(err) => {
                warn!("failed to read user config while reloading layer: {err}");
                return;
            }
        };

        let config_toml_path = match AbsolutePathBuf::try_from(config_toml_path) {
            Ok(path) => path,
            Err(err) => {
                warn!("failed to resolve user config path while reloading layer: {err}");
                return;
            }
        };

        let mut state = self.state.lock().await;
        let mut config = (*state.session_configuration.original_config_do_not_use).clone();
        config.config_layer_stack = config
            .config_layer_stack
            .with_user_config(&config_toml_path, user_config);
        state.session_configuration.original_config_do_not_use = Arc::new(config);
    }

    pub(crate) async fn new_default_turn_with_sub_id(&self, sub_id: String) -> Arc<TurnContext> {
        let session_configuration = {
            let state = self.state.lock().await;
            state.session_configuration.clone()
        };
        self.new_turn_from_configuration(sub_id, session_configuration, None, false)
            .await
    }

    pub(crate) async fn current_collaboration_mode(&self) -> CollaborationMode {
        let state = self.state.lock().await;
        state.session_configuration.collaboration_mode.clone()
    }

    fn build_environment_update_item(
        &self,
        previous: Option<&Arc<TurnContext>>,
        next: &TurnContext,
    ) -> Option<ResponseItem> {
        let prev = previous?;
        if !next.config.include_environment_context {
            return None;
        }

        let shell = self.user_shell();
        let prev_context = EnvironmentContext::from_turn_context(prev.as_ref(), shell.as_ref());
        let next_context = EnvironmentContext::from_turn_context(next, shell.as_ref());
        if prev_context.equals_except_shell(&next_context) {
            return None;
        }
        Some(ResponseItem::from(EnvironmentContext::diff(
            prev.as_ref(),
            next,
            shell.as_ref(),
        )))
    }

    fn build_permissions_update_item(
        &self,
        previous: Option<&Arc<TurnContext>>,
        next: &TurnContext,
    ) -> Option<ResponseItem> {
        let prev = previous?;
        if !next.config.include_permissions_instructions {
            return None;
        }
        if prev.sandbox_policy == next.sandbox_policy
            && prev.approval_policy == next.approval_policy
        {
            return None;
        }

        Some(
            DeveloperInstructions::from_policy(
                &next.sandbox_policy,
                next.approval_policy,
                self.services.exec_policy.current().as_ref(),
                self.features.enabled(Feature::RequestRule),
                prompt_cwd(&next.cwd).as_path(),
            )
            .into(),
        )
    }

    fn build_personality_update_item(
        &self,
        previous: Option<&Arc<TurnContext>>,
        next: &TurnContext,
    ) -> Option<ResponseItem> {
        if !self.features.enabled(Feature::Personality) {
            return None;
        }
        let previous = previous?;
        if next.model_info.slug != previous.model_info.slug {
            return None;
        }

        // if a personality is specified and it's different from the previous one, build a personality update item
        if let Some(personality) = next.personality
            && next.personality != previous.personality
        {
            let model_info = &next.model_info;
            let personality_message = Self::personality_message_for(model_info, personality);
            personality_message.map(|personality_message| {
                DeveloperInstructions::personality_spec_message(personality_message).into()
            })
        } else {
            None
        }
    }

    fn personality_message_for(model_info: &ModelInfo, personality: Personality) -> Option<String> {
        model_info
            .model_messages
            .as_ref()
            .and_then(|spec| spec.get_personality_message(Some(personality)))
            .filter(|message| !message.is_empty())
    }

    fn build_collaboration_mode_update_item(
        &self,
        previous: Option<&Arc<TurnContext>>,
        next: &TurnContext,
    ) -> Option<ResponseItem> {
        let prev = previous?;
        if prev.collaboration_mode != next.collaboration_mode {
            // If the next mode has empty developer instructions, this returns None and we emit no
            // update, so prior collaboration instructions remain in the prompt history.
            Some(
                developer_instructions_for_collaboration_mode(
                    &next.collaboration_mode,
                    &next.session_source,
                )?
                .into(),
            )
        } else {
            None
        }
    }

    fn build_model_instructions_update_item(
        &self,
        previous: Option<&Arc<TurnContext>>,
        resumed_model: Option<&str>,
        next: &TurnContext,
    ) -> Option<ResponseItem> {
        let previous_model =
            resumed_model.or_else(|| previous.map(|prev| prev.model_info.slug.as_str()))?;
        if previous_model == next.model_info.slug {
            return None;
        }

        let model_instructions = next.model_info.get_model_instructions(next.personality);
        if model_instructions.is_empty() {
            return None;
        }

        Some(DeveloperInstructions::model_switch_message(model_instructions).into())
    }

    pub(crate) fn is_model_switch_developer_message(item: &ResponseItem) -> bool {
        let ResponseItem::Message { role, content, .. } = item else {
            return false;
        };
        role == "developer"
            && content.iter().any(|content_item| {
                matches!(
                    content_item,
                    ContentItem::InputText { text } if text.starts_with("<model_switch>")
                )
            })
    }

    fn build_settings_update_items(
        &self,
        previous_context: Option<&Arc<TurnContext>>,
        resumed_model: Option<&str>,
        current_context: &TurnContext,
    ) -> Vec<ResponseItem> {
        let mut update_items = Vec::new();
        if let Some(env_item) =
            self.build_environment_update_item(previous_context, current_context)
        {
            update_items.push(env_item);
        }
        if let Some(permissions_item) =
            self.build_permissions_update_item(previous_context, current_context)
        {
            update_items.push(permissions_item);
        }
        if let Some(collaboration_mode_item) =
            self.build_collaboration_mode_update_item(previous_context, current_context)
        {
            update_items.push(collaboration_mode_item);
        }
        if let Some(model_instructions_item) = self.build_model_instructions_update_item(
            previous_context,
            resumed_model,
            current_context,
        ) {
            update_items.push(model_instructions_item);
        }
        if let Some(personality_item) =
            self.build_personality_update_item(previous_context, current_context)
        {
            update_items.push(personality_item);
        }
        update_items
    }

    /// Persist the event to rollout and send it to clients.
    pub(crate) async fn send_event(&self, turn_context: &TurnContext, msg: EventMsg) {
        let debug_trace_root =
            debug_trace::trace_output_root(turn_context.config.codex_home.as_path());
        let is_turn_started = matches!(&msg, EventMsg::TurnStarted(_));
        let initial_context_items = if is_turn_started {
            Some(self.build_initial_context(turn_context).await)
        } else {
            None
        };
        let base_instructions = if is_turn_started {
            Some(self.get_base_instructions().await.text)
        } else {
            None
        };

        let shared_blackboard_path = {
            let state = self.state.lock().await;
            state.shared_blackboard_path()
        };
        let debug_trace_event = debug_trace_root.and_then(|output_root| {
            let trace_context = debug_trace::DebugTraceContext {
                conversation_id: self.conversation_id,
                turn_id: turn_context.sub_id.clone(),
                session_source: turn_context.session_source.clone(),
                collaboration_mode_kind: turn_context.collaboration_mode.mode,
                reasoning_effort: turn_context.reasoning_effort,
                base_instructions: base_instructions.clone(),
                initial_context_items: initial_context_items.clone(),
                developer_instructions: turn_context.developer_instructions.clone(),
                user_instructions: turn_context.user_instructions.clone(),
                agent_name: self
                    .services
                    .agent_control
                    .agent_name_for_thread(self.conversation_id),
                shared_blackboard_path: shared_blackboard_path.clone(),
            };
            debug_trace::record_event(output_root.as_path(), &trace_context, &msg)
        });

        let legacy_source = msg.clone();
        let event = Event {
            id: turn_context.sub_id.clone(),
            msg,
        };
        self.send_event_raw(event).await;

        if let Some(snapshot_event) = debug_trace_event {
            self.send_event_raw(Event {
                id: turn_context.sub_id.clone(),
                msg: EventMsg::DebugTraceSnapshot(snapshot_event),
            })
            .await;
        }

        let show_raw_agent_reasoning = self.show_raw_agent_reasoning();
        for legacy in legacy_source.as_legacy_events(show_raw_agent_reasoning) {
            let legacy_event = Event {
                id: turn_context.sub_id.clone(),
                msg: legacy,
            };
            self.send_event_raw(legacy_event).await;
        }
    }

    pub(crate) async fn send_event_raw(&self, event: Event) {
        // Record the last known agent status.
        if let Some(status) = agent_status_from_event(&event.msg) {
            self.agent_status.send_replace(status);
        }
        // Persist the event into rollout (recorder filters as needed)
        let rollout_items = vec![RolloutItem::EventMsg(event.msg.clone())];
        self.persist_rollout_items(&rollout_items).await;
        if let Err(e) = self.tx_event.send(event).await {
            debug!("dropping event because channel is closed: {e}");
        }
    }

    /// Persist the event to the rollout file, flush it, and only then deliver it to clients.
    ///
    /// Most events can be delivered immediately after queueing the rollout write, but some
    /// clients (e.g. app-server thread/rollback) re-read the rollout file synchronously on
    /// receipt of the event and depend on the marker already being visible on disk.
    pub(crate) async fn send_event_raw_flushed(&self, event: Event) {
        // Record the last known agent status.
        if let Some(status) = agent_status_from_event(&event.msg) {
            self.agent_status.send_replace(status);
        }
        self.persist_rollout_items(&[RolloutItem::EventMsg(event.msg.clone())])
            .await;
        self.flush_rollout().await;
        if let Err(e) = self.tx_event.send(event).await {
            debug!("dropping event because channel is closed: {e}");
        }
    }

    pub(crate) async fn emit_turn_item_started(&self, turn_context: &TurnContext, item: &TurnItem) {
        self.send_event(
            turn_context,
            EventMsg::ItemStarted(ItemStartedEvent {
                thread_id: self.conversation_id,
                turn_id: turn_context.sub_id.clone(),
                item: item.clone(),
            }),
        )
        .await;
    }

    pub(crate) async fn emit_turn_item_completed(
        &self,
        turn_context: &TurnContext,
        item: TurnItem,
    ) {
        self.send_event(
            turn_context,
            EventMsg::ItemCompleted(ItemCompletedEvent {
                thread_id: self.conversation_id,
                turn_id: turn_context.sub_id.clone(),
                item,
            }),
        )
        .await;
    }

    /// Adds an execpolicy amendment to both the in-memory and on-disk policies so future
    /// commands can use the newly approved prefix.
    pub(crate) async fn persist_execpolicy_amendment(
        &self,
        amendment: &ExecPolicyAmendment,
    ) -> Result<(), ExecPolicyUpdateError> {
        let codex_home = self
            .state
            .lock()
            .await
            .session_configuration
            .codex_home()
            .clone();

        self.services
            .exec_policy
            .append_amendment_and_update(&codex_home, amendment)
            .await?;

        Ok(())
    }

    pub(crate) async fn turn_context_for_sub_id(&self, sub_id: &str) -> Option<Arc<TurnContext>> {
        let active = self.active_turn.lock().await;
        active
            .as_ref()
            .and_then(|turn| turn.tasks.get(sub_id))
            .map(|task| Arc::clone(&task.turn_context))
    }

    async fn active_turn_context_and_cancellation_token(
        &self,
    ) -> Option<(Arc<TurnContext>, CancellationToken)> {
        let active = self.active_turn.lock().await;
        let (_, task) = active.as_ref()?.tasks.first()?;
        Some((
            Arc::clone(&task.turn_context),
            task.cancellation_token.child_token(),
        ))
    }

    pub(crate) async fn record_execpolicy_amendment_message(
        &self,
        sub_id: &str,
        amendment: &ExecPolicyAmendment,
    ) {
        let Some(prefixes) = format_allow_prefixes(vec![amendment.command.clone()]) else {
            warn!("execpolicy amendment for {sub_id} had no command prefix");
            return;
        };
        let text = format!("Approved command prefix saved:\n{prefixes}");
        let message: ResponseItem = DeveloperInstructions::new(text.clone()).into();

        if let Some(turn_context) = self.turn_context_for_sub_id(sub_id).await {
            self.record_conversation_items(&turn_context, std::slice::from_ref(&message))
                .await;
            return;
        }

        if self
            .inject_response_items(vec![ResponseInputItem::Message {
                role: "developer".to_string(),
                content: vec![ContentItem::InputText { text }],
            }])
            .await
            .is_err()
        {
            warn!("no active turn found to record execpolicy amendment message for {sub_id}");
        }
    }

    /// Emit an exec approval request event and await the user's decision.
    ///
    /// The request is keyed by `call_id` so matching responses are delivered
    /// to the correct in-flight turn. If the task is aborted, this returns the
    /// default `ReviewDecision` (`Denied`).
    #[allow(clippy::too_many_arguments)]
    pub async fn request_command_approval(
        &self,
        turn_context: &TurnContext,
        call_id: String,
        command: Vec<String>,
        cwd: PathBuf,
        reason: Option<String>,
        network_approval_context: Option<NetworkApprovalContext>,
        proposed_execpolicy_amendment: Option<ExecPolicyAmendment>,
    ) -> ReviewDecision {
        // Add the tx_approve callback to the map before sending the request.
        let (tx_approve, rx_approve) = oneshot::channel();
        let approval_id = call_id.clone();
        let prev_entry = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    let mut ts = at.turn_state.lock().await;
                    ts.insert_pending_approval(approval_id.clone(), tx_approve)
                }
                None => None,
            }
        };
        if prev_entry.is_some() {
            warn!("Overwriting existing pending approval for call_id: {approval_id}");
        }

        let parsed_cmd = parse_command(&command);
        let event = EventMsg::ExecApprovalRequest(ExecApprovalRequestEvent {
            call_id,
            turn_id: turn_context.sub_id.clone(),
            command,
            cwd,
            reason,
            network_approval_context,
            proposed_execpolicy_amendment,
            parsed_cmd,
        });
        self.send_event(turn_context, event).await;
        rx_approve.await.unwrap_or_default()
    }

    pub async fn request_patch_approval(
        &self,
        turn_context: &TurnContext,
        call_id: String,
        changes: HashMap<PathBuf, FileChange>,
        reason: Option<String>,
        grant_root: Option<PathBuf>,
    ) -> oneshot::Receiver<ReviewDecision> {
        // Add the tx_approve callback to the map before sending the request.
        let (tx_approve, rx_approve) = oneshot::channel();
        let approval_id = call_id.clone();
        let prev_entry = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    let mut ts = at.turn_state.lock().await;
                    ts.insert_pending_approval(approval_id.clone(), tx_approve)
                }
                None => None,
            }
        };
        if prev_entry.is_some() {
            warn!("Overwriting existing pending approval for call_id: {approval_id}");
        }

        let event = EventMsg::ApplyPatchApprovalRequest(ApplyPatchApprovalRequestEvent {
            call_id,
            turn_id: turn_context.sub_id.clone(),
            changes,
            reason,
            grant_root,
        });
        self.send_event(turn_context, event).await;
        rx_approve
    }

    pub async fn request_user_input(
        &self,
        turn_context: &TurnContext,
        call_id: String,
        args: RequestUserInputArgs,
    ) -> Option<RequestUserInputResponse> {
        let sub_id = turn_context.sub_id.clone();
        let (tx_response, rx_response) = oneshot::channel();
        let event_id = sub_id.clone();
        let prev_entry = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    let mut ts = at.turn_state.lock().await;
                    ts.insert_pending_user_input(sub_id, tx_response)
                }
                None => None,
            }
        };
        if prev_entry.is_some() {
            warn!("Overwriting existing pending user input for sub_id: {event_id}");
        }

        let event = EventMsg::RequestUserInput(RequestUserInputEvent {
            call_id,
            turn_id: turn_context.sub_id.clone(),
            questions: args.questions,
        });
        self.send_event(turn_context, event).await;
        rx_response.await.ok()
    }

    pub async fn notify_user_input_response(
        &self,
        sub_id: &str,
        response: RequestUserInputResponse,
    ) {
        let entry = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    let mut ts = at.turn_state.lock().await;
                    ts.remove_pending_user_input(sub_id)
                }
                None => None,
            }
        };
        match entry {
            Some(tx_response) => {
                tx_response.send(response).ok();
            }
            None => {
                warn!("No pending user input found for sub_id: {sub_id}");
            }
        }
    }

    pub async fn notify_dynamic_tool_response(&self, call_id: &str, response: DynamicToolResponse) {
        let entry = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    let mut ts = at.turn_state.lock().await;
                    ts.remove_pending_dynamic_tool(call_id)
                }
                None => None,
            }
        };
        match entry {
            Some(tx_response) => {
                tx_response.send(response).ok();
            }
            None => {
                warn!("No pending dynamic tool call found for call_id: {call_id}");
            }
        }
    }

    pub async fn notify_approval(&self, approval_id: &str, decision: ReviewDecision) {
        let entry = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    let mut ts = at.turn_state.lock().await;
                    ts.remove_pending_approval(approval_id)
                }
                None => None,
            }
        };
        match entry {
            Some(tx_approve) => {
                tx_approve.send(decision).ok();
            }
            None => {
                warn!("No pending approval found for call_id: {approval_id}");
            }
        }
    }

    pub async fn resolve_elicitation(
        &self,
        server_name: String,
        id: RequestId,
        response: ElicitationResponse,
    ) -> anyhow::Result<()> {
        self.services
            .mcp_connection_manager
            .read()
            .await
            .resolve_elicitation(server_name, id, response)
            .await
    }

    /// Records input items: always append to conversation history and
    /// persist these response items to rollout.
    pub(crate) async fn record_conversation_items(
        &self,
        turn_context: &TurnContext,
        items: &[ResponseItem],
    ) {
        self.record_into_history(items, turn_context).await;
        self.persist_rollout_response_items(items).await;
        self.send_raw_response_items(turn_context, items).await;
    }

    async fn reconstruct_history_from_rollout(
        &self,
        turn_context: &TurnContext,
        rollout_items: &[RolloutItem],
    ) -> Vec<ResponseItem> {
        let mut history = ContextManager::new();
        for item in rollout_items {
            match item {
                RolloutItem::ResponseItem(response_item) => {
                    history.record_items(
                        std::iter::once(response_item),
                        turn_context.truncation_policy,
                    );
                }
                RolloutItem::Compacted(compacted) => {
                    if let Some(replacement) = &compacted.replacement_history {
                        history.replace(replacement.clone());
                    } else {
                        let user_messages = collect_user_messages(history.raw_items());
                        let rebuilt = compact::build_compacted_history(
                            self.build_initial_context(turn_context).await,
                            &user_messages,
                            &compacted.message,
                        );
                        history.replace(rebuilt);
                    }
                }
                RolloutItem::EventMsg(EventMsg::ThreadRolledBack(rollback)) => {
                    history.drop_last_n_user_turns(rollback.num_turns);
                }
                _ => {}
            }
        }
        history.raw_items().to_vec()
    }

    pub(crate) async fn process_compacted_history(
        &self,
        turn_context: &TurnContext,
        compacted_history: Vec<ResponseItem>,
    ) -> Vec<ResponseItem> {
        let initial_context = self.build_initial_context(turn_context).await;
        compact::process_compacted_history(compacted_history, &initial_context)
    }

    /// Append ResponseItems to the in-memory conversation history only.
    pub(crate) async fn record_into_history(
        &self,
        items: &[ResponseItem],
        turn_context: &TurnContext,
    ) {
        let mut state = self.state.lock().await;
        state.record_items(items.iter(), turn_context.truncation_policy);
    }

    pub(crate) async fn record_model_warning(&self, message: impl Into<String>, ctx: &TurnContext) {
        self.services
            .otel_manager
            .counter("codex.model_warning", 1, &[]);
        let item = ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: format!("Warning: {}", message.into()),
            }],
            end_turn: None,
            phase: None,
        };

        self.record_conversation_items(ctx, &[item]).await;
    }

    pub(crate) async fn replace_history(&self, items: Vec<ResponseItem>) {
        let mut state = self.state.lock().await;
        state.replace_history(items);
    }

    pub(crate) async fn seed_initial_context_if_needed(&self, turn_context: &TurnContext) {
        {
            let mut state = self.state.lock().await;
            if state.initial_context_seeded {
                return;
            }
            state.initial_context_seeded = true;
        }

        let initial_context = self.build_initial_context(turn_context).await;
        self.record_conversation_items(turn_context, &initial_context)
            .await;
        self.flush_rollout().await;
    }

    async fn persist_rollout_response_items(&self, items: &[ResponseItem]) {
        let rollout_items: Vec<RolloutItem> = items
            .iter()
            .cloned()
            .map(RolloutItem::ResponseItem)
            .collect();
        self.persist_rollout_items(&rollout_items).await;
    }

    pub fn enabled(&self, feature: Feature) -> bool {
        self.features.enabled(feature)
    }

    pub(crate) fn features(&self) -> Features {
        self.features.clone()
    }

    pub(crate) async fn collaboration_mode(&self) -> CollaborationMode {
        let state = self.state.lock().await;
        state.session_configuration.collaboration_mode.clone()
    }

    async fn send_raw_response_items(&self, turn_context: &TurnContext, items: &[ResponseItem]) {
        for item in items {
            self.send_event(
                turn_context,
                EventMsg::RawResponseItem(RawResponseItemEvent { item: item.clone() }),
            )
            .await;
        }
    }

    pub(crate) async fn build_initial_context(
        &self,
        turn_context: &TurnContext,
    ) -> Vec<ResponseItem> {
        let mut items = Vec::<ResponseItem>::with_capacity(4);
        let shell = self.user_shell();
        if turn_context.config.include_permissions_instructions {
            items.push(
                DeveloperInstructions::from_policy(
                    &turn_context.sandbox_policy,
                    turn_context.approval_policy,
                    self.services.exec_policy.current().as_ref(),
                    self.features.enabled(Feature::RequestRule),
                    prompt_cwd(&turn_context.cwd).as_path(),
                )
                .into(),
            );
        }
        if let Some(developer_instructions) = turn_context.developer_instructions.as_deref() {
            items.push(DeveloperInstructions::new(developer_instructions.to_string()).into());
        }
        // Add developer instructions from durable per-agent context. This is intentionally
        // separate from runtime tail injection such as blackboard and peer snapshots.
        let (collaboration_mode, base_instructions, shared_blackboard_path, session_source) = {
            let state = self.state.lock().await;
            (
                state.session_configuration.collaboration_mode.clone(),
                state.session_configuration.base_instructions.clone(),
                state.shared_blackboard_path(),
                state.session_configuration.session_source.clone(),
            )
        };
        let agent_name = visible_agent_name_for_session(
            &self.services.agent_control,
            self.conversation_id,
            &session_source,
        );
        let agent_storage_id = durable_agent_id_for_session(
            &self.services.agent_control,
            self.conversation_id,
            &session_source,
        );
        if let Some(agent_context_prompt) = build_agent_context_developer_instructions_for_agent(
            &turn_context.config.codex_home,
            &agent_storage_id,
            &agent_name,
        )
        .await
        {
            items.push(DeveloperInstructions::new(agent_context_prompt).into());
        }
        // Add developer instructions for memories.
        if let Some(memory_prompt) =
            build_memory_tool_developer_instructions(&turn_context.config.codex_home).await
            && turn_context.features.enabled(Feature::MemoryTool)
        {
            items.push(DeveloperInstructions::new(memory_prompt).into());
        }
        // Add developer instructions from collaboration_mode if they exist and are non-empty
        if let Some(collab_instructions) =
            developer_instructions_for_collaboration_mode(&collaboration_mode, &session_source)
        {
            items.push(collab_instructions.into());
        }
        if collaboration_mode.mode == ModeKind::Swarm
            && let Some(blackboard_path) = shared_blackboard_path
        {
            items.push(
                DeveloperInstructions::new(render_swarm_blackboard_developer_instructions(
                    &agent_name,
                    &blackboard_path,
                ))
                .into(),
            );
        }
        if self.features.enabled(Feature::Personality)
            && let Some(personality) = turn_context.personality
        {
            let model_info = turn_context.model_info.clone();
            let has_baked_personality = model_info.supports_personality()
                && base_instructions == model_info.get_model_instructions(Some(personality));
            if !has_baked_personality
                && let Some(personality_message) =
                    Self::personality_message_for(&model_info, personality)
            {
                items.push(
                    DeveloperInstructions::personality_spec_message(personality_message).into(),
                );
            }
        }
        if turn_context.config.include_apps_instructions
            && turn_context.features.enabled(Feature::Apps)
        {
            items.push(DeveloperInstructions::new(render_apps_section()).into());
        }
        if let Some(user_instructions) = turn_context.user_instructions.as_deref() {
            items.push(
                UserInstructions {
                    text: user_instructions.to_string(),
                    directory: prompt_cwd(&turn_context.cwd).to_string_lossy().into_owned(),
                }
                .into(),
            );
        }
        if turn_context.config.include_environment_context {
            items.push(ResponseItem::from(EnvironmentContext::from_turn_context(
                turn_context,
                shell.as_ref(),
            )));
        }
        items
    }

    pub(crate) async fn persist_rollout_items(&self, items: &[RolloutItem]) {
        let recorder = {
            let guard = self.services.rollout.lock().await;
            guard.clone()
        };
        if let Some(rec) = recorder
            && let Err(e) = rec.record_items(items).await
        {
            error!("failed to record rollout items: {e:#}");
        }
    }

    pub(crate) async fn clone_history(&self) -> ContextManager {
        let state = self.state.lock().await;
        state.clone_history()
    }

    pub(crate) async fn update_token_usage_info(
        &self,
        turn_context: &TurnContext,
        token_usage: Option<&TokenUsage>,
    ) {
        {
            let mut state = self.state.lock().await;
            if let Some(token_usage) = token_usage {
                state
                    .update_token_info_from_usage(token_usage, turn_context.model_context_window());
            }
        }
        self.send_token_count_event(turn_context).await;
    }

    pub(crate) async fn recompute_token_usage(&self, turn_context: &TurnContext) {
        let history = self.clone_history().await;
        let base_instructions = self.get_base_instructions().await;
        let Some(estimated_total_tokens) =
            history.estimate_token_count_with_base_instructions(&base_instructions)
        else {
            return;
        };
        {
            let mut state = self.state.lock().await;
            let mut info = state.token_info().unwrap_or(TokenUsageInfo {
                total_token_usage: TokenUsage::default(),
                last_token_usage: TokenUsage::default(),
                model_context_window: None,
            });

            info.last_token_usage = TokenUsage {
                input_tokens: 0,
                cached_input_tokens: 0,
                output_tokens: 0,
                reasoning_output_tokens: 0,
                total_tokens: estimated_total_tokens.max(0),
            };

            if let Some(model_context_window) = turn_context.model_context_window() {
                info.model_context_window = Some(model_context_window);
            }

            state.set_token_info(Some(info));
        }
        self.send_token_count_event(turn_context).await;
    }

    pub(crate) async fn update_rate_limits(
        &self,
        turn_context: &TurnContext,
        new_rate_limits: RateLimitSnapshot,
    ) {
        {
            let mut state = self.state.lock().await;
            state.set_rate_limits(new_rate_limits);
        }
        self.send_token_count_event(turn_context).await;
    }

    pub(crate) async fn mcp_dependency_prompted(&self) -> HashSet<String> {
        let state = self.state.lock().await;
        state.mcp_dependency_prompted()
    }

    pub(crate) async fn record_mcp_dependency_prompted<I>(&self, names: I)
    where
        I: IntoIterator<Item = String>,
    {
        let mut state = self.state.lock().await;
        state.record_mcp_dependency_prompted(names);
    }

    pub async fn dependency_env(&self) -> HashMap<String, String> {
        let state = self.state.lock().await;
        state.dependency_env()
    }

    pub async fn set_dependency_env(&self, values: HashMap<String, String>) {
        let mut state = self.state.lock().await;
        state.set_dependency_env(values);
    }

    pub(crate) async fn set_server_reasoning_included(&self, included: bool) {
        let mut state = self.state.lock().await;
        state.set_server_reasoning_included(included);
    }

    async fn send_token_count_event(&self, turn_context: &TurnContext) {
        let (info, rate_limits) = {
            let state = self.state.lock().await;
            state.token_info_and_rate_limits()
        };
        let event = EventMsg::TokenCount(TokenCountEvent { info, rate_limits });
        self.send_event(turn_context, event).await;
    }

    pub(crate) async fn set_total_tokens_full(&self, turn_context: &TurnContext) {
        if let Some(context_window) = turn_context.model_context_window() {
            let mut state = self.state.lock().await;
            state.set_token_usage_full(context_window);
        }
        self.send_token_count_event(turn_context).await;
    }

    pub(crate) async fn record_response_item_and_emit_turn_item(
        &self,
        turn_context: &TurnContext,
        response_item: ResponseItem,
    ) {
        // Add to conversation history and persist response item to rollout.
        self.record_conversation_items(turn_context, std::slice::from_ref(&response_item))
            .await;

        // Derive a turn item and emit lifecycle events if applicable.
        if let Some(item) = parse_turn_item(&response_item) {
            self.emit_turn_item_started(turn_context, &item).await;
            self.emit_turn_item_completed(turn_context, item).await;
        }
    }

    pub(crate) async fn record_user_prompt_and_emit_turn_item(
        &self,
        turn_context: &TurnContext,
        input: &[UserInput],
        response_item: ResponseItem,
    ) {
        // Persist the user message to history, but emit the turn item from `UserInput` so
        // UI-only `text_elements` are preserved. `ResponseItem::Message` does not carry
        // those spans, and `record_response_item_and_emit_turn_item` would drop them.
        self.record_conversation_items(turn_context, std::slice::from_ref(&response_item))
            .await;
        let turn_item = TurnItem::UserMessage(UserMessageItem::new(input));
        self.emit_turn_item_started(turn_context, &turn_item).await;
        self.emit_turn_item_completed(turn_context, turn_item).await;
        self.ensure_rollout_materialized().await;
    }

    pub(crate) async fn notify_background_event(
        &self,
        turn_context: &TurnContext,
        message: impl Into<String>,
    ) {
        let event = EventMsg::BackgroundEvent(BackgroundEventEvent {
            message: message.into(),
            stage: None,
            elapsed_ms: None,
            timeout_ms: None,
            outcome: None,
        });
        self.send_event(turn_context, event).await;
    }

    pub(crate) async fn notify_stream_error(
        &self,
        turn_context: &TurnContext,
        message: impl Into<String>,
        codex_error: CodexErr,
    ) {
        let additional_details = codex_error.to_string();
        let codex_error_info = CodexErrorInfo::ResponseStreamDisconnected {
            http_status_code: codex_error.http_status_code_value(),
        };
        let event = EventMsg::StreamError(StreamErrorEvent {
            message: message.into(),
            codex_error_info: Some(codex_error_info),
            additional_details: Some(additional_details),
        });
        self.send_event(turn_context, event).await;
    }

    async fn maybe_start_ghost_snapshot(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        cancellation_token: CancellationToken,
    ) {
        if !self.enabled(Feature::GhostCommit) {
            return;
        }
        let token = match turn_context.tool_call_gate.subscribe().await {
            Ok(token) => token,
            Err(err) => {
                warn!("failed to subscribe to ghost snapshot readiness: {err}");
                return;
            }
        };

        info!("spawning ghost snapshot task");
        let task = GhostSnapshotTask::new(token);
        Arc::new(task)
            .run(
                Arc::new(SessionTaskContext::new(self.clone())),
                turn_context.clone(),
                Vec::new(),
                cancellation_token,
            )
            .await;
    }

    /// Inject additional user input into the currently active turn.
    ///
    /// Returns the active turn id when accepted.
    pub async fn steer_input(
        &self,
        input: Vec<UserInput>,
        expected_turn_id: Option<&str>,
    ) -> Result<String, SteerInputError> {
        if input.is_empty() {
            return Err(SteerInputError::EmptyInput);
        }

        let mut active = self.active_turn.lock().await;
        let Some(active_turn) = active.as_mut() else {
            return Err(SteerInputError::NoActiveTurn(input));
        };

        let Some((active_turn_id, _)) = active_turn.tasks.first() else {
            return Err(SteerInputError::NoActiveTurn(input));
        };

        if let Some(expected_turn_id) = expected_turn_id
            && expected_turn_id != active_turn_id
        {
            return Err(SteerInputError::ExpectedTurnMismatch {
                expected: expected_turn_id.to_string(),
                actual: active_turn_id.clone(),
            });
        }

        let mut turn_state = active_turn.turn_state.lock().await;
        turn_state.push_pending_input(input.into());
        Ok(active_turn_id.clone())
    }

    /// Returns the input if there was no task running to inject into
    pub async fn inject_response_items(
        &self,
        input: Vec<ResponseInputItem>,
    ) -> Result<(), Vec<ResponseInputItem>> {
        let mut active = self.active_turn.lock().await;
        match active.as_mut() {
            Some(at) => {
                let mut ts = at.turn_state.lock().await;
                for item in input {
                    ts.push_pending_input(item);
                }
                Ok(())
            }
            None => Err(input),
        }
    }

    pub async fn get_pending_input(&self) -> Vec<ResponseInputItem> {
        let mut active = self.active_turn.lock().await;
        match active.as_mut() {
            Some(at) => {
                let mut ts = at.turn_state.lock().await;
                ts.take_pending_input()
            }
            None => Vec::with_capacity(0),
        }
    }

    pub async fn has_pending_input(&self) -> bool {
        let active = self.active_turn.lock().await;
        match active.as_ref() {
            Some(at) => {
                let ts = at.turn_state.lock().await;
                ts.has_pending_input()
            }
            None => false,
        }
    }

    pub async fn list_resources(
        &self,
        server: &str,
        params: Option<PaginatedRequestParams>,
    ) -> anyhow::Result<ListResourcesResult> {
        self.services
            .mcp_connection_manager
            .read()
            .await
            .list_resources(server, params)
            .await
    }

    pub async fn list_resource_templates(
        &self,
        server: &str,
        params: Option<PaginatedRequestParams>,
    ) -> anyhow::Result<ListResourceTemplatesResult> {
        self.services
            .mcp_connection_manager
            .read()
            .await
            .list_resource_templates(server, params)
            .await
    }

    pub async fn read_resource(
        &self,
        server: &str,
        params: ReadResourceRequestParams,
    ) -> anyhow::Result<ReadResourceResult> {
        self.services
            .mcp_connection_manager
            .read()
            .await
            .read_resource(server, params)
            .await
    }

    pub async fn call_tool(
        &self,
        server: &str,
        tool: &str,
        arguments: Option<serde_json::Value>,
    ) -> anyhow::Result<CallToolResult> {
        self.services
            .mcp_connection_manager
            .read()
            .await
            .call_tool(server, tool, arguments)
            .await
    }

    pub(crate) async fn parse_mcp_tool_name(&self, tool_name: &str) -> Option<(String, String)> {
        self.services
            .mcp_connection_manager
            .read()
            .await
            .parse_tool_name(tool_name)
            .await
    }

    pub async fn interrupt_task(self: &Arc<Self>) {
        info!("interrupt received: abort current task, if any");
        let has_active_turn = { self.active_turn.lock().await.is_some() };
        if has_active_turn {
            self.abort_all_tasks(TurnAbortReason::Interrupted).await;
        } else {
            self.cancel_mcp_startup().await;
        }
    }

    pub(crate) fn hooks(&self) -> &Hooks {
        &self.services.hooks
    }

    pub(crate) fn user_shell(&self) -> Arc<shell::Shell> {
        Arc::clone(&self.services.user_shell)
    }

    async fn refresh_mcp_servers_inner(
        &self,
        turn_context: &TurnContext,
        mcp_servers: HashMap<String, McpServerConfig>,
        store_mode: OAuthCredentialsStoreMode,
    ) {
        let auth = self.services.auth_manager.auth().await;
        let config = self.get_config().await;
        let mcp_servers = with_codex_apps_mcp(
            mcp_servers,
            self.features.enabled(Feature::Apps),
            auth.as_ref(),
            config.as_ref(),
        );
        let auth_statuses = compute_auth_statuses(mcp_servers.iter(), store_mode).await;
        let sandbox_state = SandboxState {
            sandbox_policy: turn_context.sandbox_policy.clone(),
            codex_linux_sandbox_exe: turn_context.codex_linux_sandbox_exe.clone(),
            sandbox_cwd: turn_context.cwd.clone(),
            use_linux_sandbox_bwrap: turn_context.features.enabled(Feature::UseLinuxSandboxBwrap),
        };
        let cancel_token = self.reset_mcp_startup_cancellation_token().await;

        let mut refreshed_manager = McpConnectionManager::default();
        refreshed_manager
            .initialize(
                &mcp_servers,
                store_mode,
                auth_statuses,
                self.get_tx_event(),
                cancel_token,
                sandbox_state,
            )
            .await;

        let mut manager = self.services.mcp_connection_manager.write().await;
        *manager = refreshed_manager;
    }

    async fn refresh_mcp_servers_if_requested(&self, turn_context: &TurnContext) {
        let refresh_config = { self.pending_mcp_server_refresh_config.lock().await.take() };
        let Some(refresh_config) = refresh_config else {
            return;
        };

        let McpServerRefreshConfig {
            mcp_servers,
            mcp_oauth_credentials_store_mode,
        } = refresh_config;

        let mcp_servers =
            match serde_json::from_value::<HashMap<String, McpServerConfig>>(mcp_servers) {
                Ok(servers) => servers,
                Err(err) => {
                    warn!("failed to parse MCP server refresh config: {err}");
                    return;
                }
            };
        let store_mode = match serde_json::from_value::<OAuthCredentialsStoreMode>(
            mcp_oauth_credentials_store_mode,
        ) {
            Ok(mode) => mode,
            Err(err) => {
                warn!("failed to parse MCP OAuth refresh config: {err}");
                return;
            }
        };

        self.refresh_mcp_servers_inner(turn_context, mcp_servers, store_mode)
            .await;
    }

    pub(crate) async fn refresh_mcp_servers_now(
        &self,
        turn_context: &TurnContext,
        mcp_servers: HashMap<String, McpServerConfig>,
        store_mode: OAuthCredentialsStoreMode,
    ) {
        self.refresh_mcp_servers_inner(turn_context, mcp_servers, store_mode)
            .await;
    }

    async fn mcp_startup_cancellation_token(&self) -> CancellationToken {
        self.services
            .mcp_startup_cancellation_token
            .lock()
            .await
            .clone()
    }

    async fn reset_mcp_startup_cancellation_token(&self) -> CancellationToken {
        let mut guard = self.services.mcp_startup_cancellation_token.lock().await;
        guard.cancel();
        let cancel_token = CancellationToken::new();
        *guard = cancel_token.clone();
        cancel_token
    }

    fn show_raw_agent_reasoning(&self) -> bool {
        self.services.show_raw_agent_reasoning
    }

    async fn cancel_mcp_startup(&self) {
        self.services
            .mcp_startup_cancellation_token
            .lock()
            .await
            .cancel();
    }
}

async fn submission_loop(sess: Arc<Session>, config: Arc<Config>, rx_sub: Receiver<Submission>) {
    // Seed with context in case there is an OverrideTurnContext first.
    let mut previous_context: Option<Arc<TurnContext>> = Some(sess.new_default_turn().await);

    // To break out of this loop, send Op::Shutdown.
    while let Ok(sub) = rx_sub.recv().await {
        debug!(?sub, "Submission");
        match sub.op.clone() {
            Op::Interrupt => {
                handlers::interrupt(&sess).await;
            }
            Op::CleanBackgroundTerminals => {
                handlers::clean_background_terminals(&sess).await;
            }
            Op::OverrideTurnContext {
                cwd,
                approval_policy,
                sandbox_policy,
                windows_sandbox_level,
                model,
                effort,
                summary,
                collaboration_mode,
                personality,
            } => {
                let collaboration_mode = if let Some(collab_mode) = collaboration_mode {
                    collab_mode
                } else {
                    let state = sess.state.lock().await;
                    state.session_configuration.collaboration_mode.with_updates(
                        model.clone(),
                        effort,
                        None,
                    )
                };
                handlers::override_turn_context(
                    &sess,
                    sub.id.clone(),
                    SessionSettingsUpdate {
                        cwd,
                        approval_policy,
                        sandbox_policy,
                        windows_sandbox_level,
                        collaboration_mode: Some(collaboration_mode),
                        reasoning_summary: summary,
                        personality,
                        ..Default::default()
                    },
                )
                .await;
            }
            Op::UserInput { .. } | Op::UserTurn { .. } => {
                handlers::user_input_or_turn(&sess, sub.id.clone(), sub.op, &mut previous_context)
                    .await;
            }
            Op::ExecApproval {
                id: approval_id,
                turn_id,
                decision,
            } => {
                handlers::exec_approval(&sess, approval_id, turn_id, decision).await;
            }
            Op::PatchApproval { id, decision } => {
                handlers::patch_approval(&sess, id, decision).await;
            }
            Op::UserInputAnswer { id, response } => {
                handlers::request_user_input_response(&sess, id, response).await;
            }
            Op::DynamicToolResponse { id, response } => {
                handlers::dynamic_tool_response(&sess, id, response).await;
            }
            Op::AddToHistory { text } => {
                handlers::add_to_history(&sess, &config, text).await;
            }
            Op::GetHistoryEntryRequest { offset, log_id } => {
                handlers::get_history_entry_request(&sess, &config, sub.id.clone(), offset, log_id)
                    .await;
            }
            Op::ListMcpTools => {
                handlers::list_mcp_tools(&sess, &config, sub.id.clone()).await;
            }
            Op::RefreshMcpServers { config } => {
                handlers::refresh_mcp_servers(&sess, config).await;
            }
            Op::ReloadUserConfig => {
                handlers::reload_user_config(&sess).await;
            }
            Op::ListCustomPrompts => {
                handlers::list_custom_prompts(&sess, sub.id.clone()).await;
            }
            Op::ListSkills { cwds, force_reload } => {
                handlers::list_skills(&sess, sub.id.clone(), cwds, force_reload).await;
            }
            Op::ListRemoteSkills => {
                handlers::list_remote_skills(&sess, &config, sub.id.clone()).await;
            }
            Op::DownloadRemoteSkill {
                hazelnut_id,
                is_preload,
            } => {
                handlers::download_remote_skill(
                    &sess,
                    &config,
                    sub.id.clone(),
                    hazelnut_id,
                    is_preload,
                )
                .await;
            }
            Op::Undo => {
                handlers::undo(&sess, sub.id.clone()).await;
            }
            Op::Compact => {
                handlers::compact(&sess, sub.id.clone()).await;
            }
            Op::DropMemories => {
                handlers::drop_memories(&sess, &config, sub.id.clone()).await;
            }
            Op::UpdateMemories => {
                handlers::update_memories(&sess, &config, sub.id.clone()).await;
            }
            Op::ThreadRollback { num_turns } => {
                handlers::thread_rollback(&sess, sub.id.clone(), num_turns).await;
            }
            Op::SetThreadName { name } => {
                handlers::set_thread_name(&sess, sub.id.clone(), name).await;
            }
            Op::RunUserShellCommand { command } => {
                handlers::run_user_shell_command(
                    &sess,
                    sub.id.clone(),
                    command,
                    &mut previous_context,
                )
                .await;
            }
            Op::ResolveElicitation {
                server_name,
                request_id,
                decision,
            } => {
                handlers::resolve_elicitation(&sess, server_name, request_id, decision).await;
            }
            Op::Shutdown => {
                if handlers::shutdown(&sess, sub.id.clone()).await {
                    break;
                }
            }
            Op::Review { review_request } => {
                handlers::review(&sess, &config, sub.id.clone(), review_request).await;
            }
            _ => {} // Ignore unknown ops; enum is non_exhaustive to allow extensions.
        }
    }
    debug!("Agent loop exited");
}

/// Operation handlers
mod handlers {
    use crate::codex::Session;
    use crate::codex::SessionSettingsUpdate;
    use crate::codex::SteerInputError;
    use crate::codex::TurnContext;
    use crate::codex::render_recent_real_user_input;
    use crate::codex::should_track_recent_real_user_input;

    use crate::codex::spawn_review_thread;
    use crate::config::Config;

    use crate::mcp::auth::compute_auth_statuses;
    use crate::mcp::collect_mcp_snapshot_from_manager;
    use crate::mcp::effective_mcp_servers;
    use crate::review_prompts::resolve_review_request;
    use crate::rollout::session_index;
    use crate::tasks::CompactTask;
    use crate::tasks::UndoTask;
    use crate::tasks::UserShellCommandMode;
    use crate::tasks::UserShellCommandTask;
    use crate::tasks::execute_user_shell_command;
    use codex_protocol::custom_prompts::CustomPrompt;
    use codex_protocol::protocol::CodexErrorInfo;
    use codex_protocol::protocol::ErrorEvent;
    use codex_protocol::protocol::Event;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::ListCustomPromptsResponseEvent;
    use codex_protocol::protocol::ListRemoteSkillsResponseEvent;
    use codex_protocol::protocol::ListSkillsResponseEvent;
    use codex_protocol::protocol::McpServerRefreshConfig;
    use codex_protocol::protocol::Op;
    use codex_protocol::protocol::RemoteSkillDownloadedEvent;
    use codex_protocol::protocol::RemoteSkillSummary;
    use codex_protocol::protocol::ReviewDecision;
    use codex_protocol::protocol::ReviewRequest;
    use codex_protocol::protocol::SkillsListEntry;
    use codex_protocol::protocol::ThreadNameUpdatedEvent;
    use codex_protocol::protocol::ThreadRolledBackEvent;
    use codex_protocol::protocol::TurnAbortReason;
    use codex_protocol::protocol::UserInputOrigin;
    use codex_protocol::protocol::WarningEvent;
    use codex_protocol::request_user_input::RequestUserInputResponse;

    use crate::context_manager::is_user_turn_boundary;
    use codex_protocol::dynamic_tools::DynamicToolResponse;
    use codex_protocol::mcp::RequestId as ProtocolRequestId;
    use codex_protocol::user_input::UserInput;
    use codex_rmcp_client::ElicitationAction;
    use codex_rmcp_client::ElicitationResponse;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tracing::info;
    use tracing::warn;

    pub async fn interrupt(sess: &Arc<Session>) {
        sess.interrupt_task().await;
    }

    pub async fn clean_background_terminals(sess: &Arc<Session>) {
        sess.close_unified_exec_processes().await;
    }

    pub async fn override_turn_context(
        sess: &Session,
        sub_id: String,
        updates: SessionSettingsUpdate,
    ) {
        if let Err(err) = sess.update_settings(updates).await {
            sess.send_event_raw(Event {
                id: sub_id,
                msg: EventMsg::Error(ErrorEvent {
                    message: err.to_string(),
                    codex_error_info: Some(CodexErrorInfo::BadRequest),
                }),
            })
            .await;
        }
    }

    pub async fn user_input_or_turn(
        sess: &Arc<Session>,
        sub_id: String,
        op: Op,
        previous_context: &mut Option<Arc<TurnContext>>,
    ) {
        let (items, origin, updates) = match op {
            Op::UserTurn {
                cwd,
                approval_policy,
                sandbox_policy,
                model,
                effort,
                summary,
                final_output_json_schema,
                items,
                collaboration_mode,
                personality,
            } => {
                let collaboration_mode = if let Some(collaboration_mode) = collaboration_mode {
                    Some(collaboration_mode)
                } else {
                    let state = sess.state.lock().await;
                    Some(state.session_configuration.collaboration_mode.with_updates(
                        Some(model.clone()),
                        Some(effort),
                        None,
                    ))
                };
                (
                    items,
                    UserInputOrigin::User,
                    SessionSettingsUpdate {
                        cwd: Some(cwd),
                        approval_policy: Some(approval_policy),
                        sandbox_policy: Some(sandbox_policy),
                        windows_sandbox_level: None,
                        collaboration_mode,
                        reasoning_summary: Some(summary),
                        final_output_json_schema: Some(final_output_json_schema),
                        personality,
                    },
                )
            }
            Op::UserInput {
                items,
                origin,
                final_output_json_schema,
            } => (
                items,
                origin,
                SessionSettingsUpdate {
                    final_output_json_schema: Some(final_output_json_schema),
                    ..Default::default()
                },
            ),
            _ => unreachable!(),
        };

        let Ok(current_context) = sess.new_turn_with_sub_id(sub_id, updates).await else {
            // new_turn_with_sub_id already emits the error event.
            return;
        };
        if should_track_recent_real_user_input(&current_context.session_source, origin)
            && let Some(raw_input) = render_recent_real_user_input(&items)
        {
            sess.push_recent_real_user_input(raw_input).await;
        }
        current_context.otel_manager.user_prompt(&items);

        let steer_result = sess.steer_input(items, None).await;

        // Attempt to inject input into current task.
        if let Err(SteerInputError::NoActiveTurn(pending_items)) = steer_result {
            sess.seed_initial_context_if_needed(&current_context).await;
            let previous_model = sess.previous_model().await;
            let update_items = sess.build_settings_update_items(
                previous_context.as_ref(),
                previous_model.as_deref(),
                &current_context,
            );
            if !update_items.is_empty() {
                sess.record_conversation_items(&current_context, &update_items)
                    .await;
            }

            sess.refresh_mcp_servers_if_requested(&current_context)
                .await;
            let regular_task = sess.take_startup_regular_task().await.unwrap_or_default();
            sess.spawn_task(Arc::clone(&current_context), pending_items, regular_task)
                .await;
            *previous_context = Some(current_context);
        }
    }

    pub async fn run_user_shell_command(
        sess: &Arc<Session>,
        sub_id: String,
        command: String,
        previous_context: &mut Option<Arc<TurnContext>>,
    ) {
        if let Some((turn_context, cancellation_token)) =
            sess.active_turn_context_and_cancellation_token().await
        {
            let session = Arc::clone(sess);
            tokio::spawn(async move {
                execute_user_shell_command(
                    session,
                    turn_context,
                    command,
                    cancellation_token,
                    UserShellCommandMode::ActiveTurnAuxiliary,
                )
                .await;
            });
            return;
        }

        let turn_context = sess.new_default_turn_with_sub_id(sub_id).await;
        sess.spawn_task(
            Arc::clone(&turn_context),
            Vec::new(),
            UserShellCommandTask::new(command),
        )
        .await;
        *previous_context = Some(turn_context);
    }

    pub async fn resolve_elicitation(
        sess: &Arc<Session>,
        server_name: String,
        request_id: ProtocolRequestId,
        decision: codex_protocol::approvals::ElicitationAction,
    ) {
        let action = match decision {
            codex_protocol::approvals::ElicitationAction::Accept => ElicitationAction::Accept,
            codex_protocol::approvals::ElicitationAction::Decline => ElicitationAction::Decline,
            codex_protocol::approvals::ElicitationAction::Cancel => ElicitationAction::Cancel,
        };
        // When accepting, send an empty object as content to satisfy MCP servers
        // that expect non-null content on Accept. For Decline/Cancel, content is None.
        let content = match action {
            ElicitationAction::Accept => Some(serde_json::json!({})),
            ElicitationAction::Decline | ElicitationAction::Cancel => None,
        };
        let response = ElicitationResponse { action, content };
        let request_id = match request_id {
            ProtocolRequestId::String(value) => {
                rmcp::model::NumberOrString::String(std::sync::Arc::from(value))
            }
            ProtocolRequestId::Integer(value) => rmcp::model::NumberOrString::Number(value),
        };
        if let Err(err) = sess
            .resolve_elicitation(server_name, request_id, response)
            .await
        {
            warn!(
                error = %err,
                "failed to resolve elicitation request in session"
            );
        }
    }

    /// Propagate a user's exec approval decision to the session.
    /// Also optionally applies an execpolicy amendment.
    pub async fn exec_approval(
        sess: &Arc<Session>,
        approval_id: String,
        turn_id: Option<String>,
        decision: ReviewDecision,
    ) {
        let event_turn_id = turn_id.unwrap_or_else(|| approval_id.clone());
        if let ReviewDecision::ApprovedExecpolicyAmendment {
            proposed_execpolicy_amendment,
        } = &decision
        {
            match sess
                .persist_execpolicy_amendment(proposed_execpolicy_amendment)
                .await
            {
                Ok(()) => {
                    sess.record_execpolicy_amendment_message(
                        &event_turn_id,
                        proposed_execpolicy_amendment,
                    )
                    .await;
                }
                Err(err) => {
                    let message = format!("Failed to apply execpolicy amendment: {err}");
                    tracing::warn!("{message}");
                    let warning = EventMsg::Warning(WarningEvent { message });
                    sess.send_event_raw(Event {
                        id: event_turn_id.clone(),
                        msg: warning,
                    })
                    .await;
                }
            }
        }
        match decision {
            ReviewDecision::Abort => {
                sess.interrupt_task().await;
            }
            other => sess.notify_approval(&approval_id, other).await,
        }
    }

    pub async fn patch_approval(sess: &Arc<Session>, id: String, decision: ReviewDecision) {
        match decision {
            ReviewDecision::Abort => {
                sess.interrupt_task().await;
            }
            other => sess.notify_approval(&id, other).await,
        }
    }

    pub async fn request_user_input_response(
        sess: &Arc<Session>,
        id: String,
        response: RequestUserInputResponse,
    ) {
        sess.notify_user_input_response(&id, response).await;
    }

    pub async fn dynamic_tool_response(
        sess: &Arc<Session>,
        id: String,
        response: DynamicToolResponse,
    ) {
        sess.notify_dynamic_tool_response(&id, response).await;
    }

    pub async fn add_to_history(sess: &Arc<Session>, config: &Arc<Config>, text: String) {
        let id = sess.conversation_id;
        let config = Arc::clone(config);
        tokio::spawn(async move {
            if let Err(e) = crate::message_history::append_entry(&text, &id, &config).await {
                warn!("failed to append to message history: {e}");
            }
        });
    }

    pub async fn get_history_entry_request(
        sess: &Arc<Session>,
        config: &Arc<Config>,
        sub_id: String,
        offset: usize,
        log_id: u64,
    ) {
        let config = Arc::clone(config);
        let sess_clone = Arc::clone(sess);

        tokio::spawn(async move {
            // Run lookup in blocking thread because it does file IO + locking.
            let entry_opt = tokio::task::spawn_blocking(move || {
                crate::message_history::lookup(log_id, offset, &config)
            })
            .await
            .unwrap_or(None);

            let event = Event {
                id: sub_id,
                msg: EventMsg::GetHistoryEntryResponse(
                    crate::protocol::GetHistoryEntryResponseEvent {
                        offset,
                        log_id,
                        entry: entry_opt.map(|e| codex_protocol::message_history::HistoryEntry {
                            conversation_id: e.session_id,
                            ts: e.ts,
                            text: e.text,
                        }),
                    },
                ),
            };

            sess_clone.send_event_raw(event).await;
        });
    }

    pub async fn refresh_mcp_servers(sess: &Arc<Session>, refresh_config: McpServerRefreshConfig) {
        let mut guard = sess.pending_mcp_server_refresh_config.lock().await;
        *guard = Some(refresh_config);
    }

    pub async fn reload_user_config(sess: &Arc<Session>) {
        sess.reload_user_config_layer().await;
    }

    pub async fn list_mcp_tools(sess: &Session, config: &Arc<Config>, sub_id: String) {
        let mcp_connection_manager = sess.services.mcp_connection_manager.read().await;
        let auth = sess.services.auth_manager.auth().await;
        let mcp_servers = effective_mcp_servers(config, auth.as_ref());
        let snapshot = collect_mcp_snapshot_from_manager(
            &mcp_connection_manager,
            compute_auth_statuses(mcp_servers.iter(), config.mcp_oauth_credentials_store_mode)
                .await,
        )
        .await;
        let event = Event {
            id: sub_id,
            msg: EventMsg::McpListToolsResponse(snapshot),
        };
        sess.send_event_raw(event).await;
    }

    pub async fn list_custom_prompts(sess: &Session, sub_id: String) {
        let custom_prompts: Vec<CustomPrompt> =
            if let Some(dir) = crate::custom_prompts::default_prompts_dir() {
                crate::custom_prompts::discover_prompts_in(&dir).await
            } else {
                Vec::new()
            };

        let event = Event {
            id: sub_id,
            msg: EventMsg::ListCustomPromptsResponse(ListCustomPromptsResponseEvent {
                custom_prompts,
            }),
        };
        sess.send_event_raw(event).await;
    }

    pub async fn list_skills(
        sess: &Session,
        sub_id: String,
        cwds: Vec<PathBuf>,
        force_reload: bool,
    ) {
        let cwds = if cwds.is_empty() {
            let state = sess.state.lock().await;
            vec![state.session_configuration.cwd.clone()]
        } else {
            cwds
        };

        let skills_manager = &sess.services.skills_manager;
        let mut skills = Vec::new();
        for cwd in cwds {
            let outcome = skills_manager.skills_for_cwd(&cwd, force_reload).await;
            let errors = super::errors_to_info(&outcome.errors);
            let skills_metadata = super::skills_to_info(&outcome.skills, &outcome.disabled_paths);
            skills.push(SkillsListEntry {
                cwd,
                skills: skills_metadata,
                errors,
            });
        }

        let event = Event {
            id: sub_id,
            msg: EventMsg::ListSkillsResponse(ListSkillsResponseEvent { skills }),
        };
        sess.send_event_raw(event).await;
    }

    pub async fn list_remote_skills(sess: &Session, config: &Arc<Config>, sub_id: String) {
        let response = crate::skills::remote::list_remote_skills(config)
            .await
            .map(|skills| {
                skills
                    .into_iter()
                    .map(|skill| RemoteSkillSummary {
                        id: skill.id,
                        name: skill.name,
                        description: skill.description,
                    })
                    .collect::<Vec<_>>()
            });

        match response {
            Ok(skills) => {
                let event = Event {
                    id: sub_id,
                    msg: EventMsg::ListRemoteSkillsResponse(ListRemoteSkillsResponseEvent {
                        skills,
                    }),
                };
                sess.send_event_raw(event).await;
            }
            Err(err) => {
                let event = Event {
                    id: sub_id,
                    msg: EventMsg::Error(ErrorEvent {
                        message: format!("failed to list remote skills: {err}"),
                        codex_error_info: Some(CodexErrorInfo::Other),
                    }),
                };
                sess.send_event_raw(event).await;
            }
        }
    }

    pub async fn download_remote_skill(
        sess: &Session,
        config: &Arc<Config>,
        sub_id: String,
        hazelnut_id: String,
        is_preload: bool,
    ) {
        match crate::skills::remote::download_remote_skill(config, hazelnut_id.as_str(), is_preload)
            .await
        {
            Ok(result) => {
                let event = Event {
                    id: sub_id,
                    msg: EventMsg::RemoteSkillDownloaded(RemoteSkillDownloadedEvent {
                        id: result.id,
                        name: result.name,
                        path: result.path,
                    }),
                };
                sess.send_event_raw(event).await;
            }
            Err(err) => {
                let event = Event {
                    id: sub_id,
                    msg: EventMsg::Error(ErrorEvent {
                        message: format!("failed to download remote skill {hazelnut_id}: {err}"),
                        codex_error_info: Some(CodexErrorInfo::Other),
                    }),
                };
                sess.send_event_raw(event).await;
            }
        }
    }

    pub async fn undo(sess: &Arc<Session>, sub_id: String) {
        let turn_context = sess.new_default_turn_with_sub_id(sub_id).await;
        sess.spawn_task(turn_context, Vec::new(), UndoTask::new())
            .await;
    }

    pub async fn compact(sess: &Arc<Session>, sub_id: String) {
        let turn_context = sess.new_default_turn_with_sub_id(sub_id).await;

        sess.spawn_task(
            Arc::clone(&turn_context),
            vec![UserInput::Text {
                text: turn_context.compact_prompt().to_string(),
                // Compaction prompt is synthesized; no UI element ranges to preserve.
                text_elements: Vec::new(),
            }],
            CompactTask,
        )
        .await;
    }

    pub async fn drop_memories(sess: &Arc<Session>, config: &Arc<Config>, sub_id: String) {
        let mut errors = Vec::new();

        if let Some(state_db) = sess.services.state_db.as_deref() {
            if let Err(err) = state_db.clear_memory_data().await {
                errors.push(format!("failed clearing memory rows from state db: {err}"));
            }
        } else {
            errors.push("state db unavailable; memory rows were not cleared".to_string());
        }

        let memory_root = crate::memories::memory_root(&config.codex_home);
        if let Err(err) = tokio::fs::remove_dir_all(&memory_root).await
            && err.kind() != std::io::ErrorKind::NotFound
        {
            errors.push(format!(
                "failed removing memory directory {}: {err}",
                memory_root.display()
            ));
        }

        if errors.is_empty() {
            sess.send_event_raw(Event {
                id: sub_id,
                msg: EventMsg::Warning(WarningEvent {
                    message: format!(
                        "Dropped memories at {} and cleared memory rows from state db.",
                        memory_root.display()
                    ),
                }),
            })
            .await;
            return;
        }

        sess.send_event_raw(Event {
            id: sub_id,
            msg: EventMsg::Error(ErrorEvent {
                message: format!("Memory drop completed with errors: {}", errors.join("; ")),
                codex_error_info: Some(CodexErrorInfo::Other),
            }),
        })
        .await;
    }

    pub async fn update_memories(sess: &Arc<Session>, config: &Arc<Config>, sub_id: String) {
        let session_source = {
            let state = sess.state.lock().await;
            state.session_configuration.session_source.clone()
        };

        crate::memories::start_memories_startup_task(sess, Arc::clone(config), &session_source);

        sess.send_event_raw(Event {
            id: sub_id.clone(),
            msg: EventMsg::Warning(WarningEvent {
                message: "Memory update triggered.".to_string(),
            }),
        })
        .await;
    }

    pub async fn thread_rollback(sess: &Arc<Session>, sub_id: String, num_turns: u32) {
        if num_turns == 0 {
            sess.send_event_raw(Event {
                id: sub_id,
                msg: EventMsg::Error(ErrorEvent {
                    message: "num_turns must be >= 1".to_string(),
                    codex_error_info: Some(CodexErrorInfo::ThreadRollbackFailed),
                }),
            })
            .await;
            return;
        }

        let has_active_turn = { sess.active_turn.lock().await.is_some() };
        if has_active_turn {
            sess.send_event_raw(Event {
                id: sub_id,
                msg: EventMsg::Error(ErrorEvent {
                    message: "Cannot rollback while a turn is in progress.".to_string(),
                    codex_error_info: Some(CodexErrorInfo::ThreadRollbackFailed),
                }),
            })
            .await;
            return;
        }

        let turn_context = sess.new_default_turn_with_sub_id(sub_id).await;
        sess.set_previous_model(Some(turn_context.model_info.slug.clone()))
            .await;

        let mut history = sess.clone_history().await;
        history.drop_last_n_user_turns(num_turns);

        // Replace with the raw items. We don't want to replace with a normalized
        // version of the history.
        sess.replace_history(history.raw_items().to_vec()).await;
        sess.recompute_token_usage(turn_context.as_ref()).await;

        sess.send_event_raw_flushed(Event {
            id: turn_context.sub_id.clone(),
            msg: EventMsg::ThreadRolledBack(ThreadRolledBackEvent { num_turns }),
        })
        .await;
    }

    /// Persists the thread name in the session index, updates in-memory state, and emits
    /// a `ThreadNameUpdated` event on success.
    ///
    /// This appends the name to `CODEX_HOME/sessions_index.jsonl` via `session_index::append_thread_name` for the
    /// current `thread_id`, then updates `SessionConfiguration::thread_name`.
    ///
    /// Returns an error event if the name is empty or session persistence is disabled.
    pub async fn set_thread_name(sess: &Arc<Session>, sub_id: String, name: String) {
        let Some(name) = crate::util::normalize_thread_name(&name) else {
            let event = Event {
                id: sub_id,
                msg: EventMsg::Error(ErrorEvent {
                    message: "Thread name cannot be empty.".to_string(),
                    codex_error_info: Some(CodexErrorInfo::BadRequest),
                }),
            };
            sess.send_event_raw(event).await;
            return;
        };

        let persistence_enabled = {
            let rollout = sess.services.rollout.lock().await;
            rollout.is_some()
        };
        if !persistence_enabled {
            let event = Event {
                id: sub_id,
                msg: EventMsg::Error(ErrorEvent {
                    message: "Session persistence is disabled; cannot rename thread.".to_string(),
                    codex_error_info: Some(CodexErrorInfo::Other),
                }),
            };
            sess.send_event_raw(event).await;
            return;
        };

        let codex_home = sess.codex_home().await;
        if let Err(e) =
            session_index::append_thread_name(&codex_home, sess.conversation_id, &name).await
        {
            let event = Event {
                id: sub_id,
                msg: EventMsg::Error(ErrorEvent {
                    message: format!("Failed to set thread name: {e}"),
                    codex_error_info: Some(CodexErrorInfo::Other),
                }),
            };
            sess.send_event_raw(event).await;
            return;
        }

        {
            let mut state = sess.state.lock().await;
            state.session_configuration.thread_name = Some(name.clone());
        }

        sess.send_event_raw(Event {
            id: sub_id,
            msg: EventMsg::ThreadNameUpdated(ThreadNameUpdatedEvent {
                thread_id: sess.conversation_id,
                thread_name: Some(name),
            }),
        })
        .await;
    }

    pub async fn shutdown(sess: &Arc<Session>, sub_id: String) -> bool {
        sess.abort_all_tasks(TurnAbortReason::Interrupted).await;
        sess.services
            .unified_exec_manager
            .terminate_all_processes()
            .await;
        info!("Shutting down Codex instance");
        let history = sess.clone_history().await;
        let turn_count = history
            .raw_items()
            .iter()
            .filter(|item| is_user_turn_boundary(item))
            .count();
        sess.services.otel_manager.counter(
            "codex.conversation.turn.count",
            i64::try_from(turn_count).unwrap_or(0),
            &[],
        );

        // Gracefully flush and shutdown rollout recorder on session end so tests
        // that inspect the rollout file do not race with the background writer.
        let recorder_opt = {
            let mut guard = sess.services.rollout.lock().await;
            guard.take()
        };
        if let Some(rec) = recorder_opt
            && let Err(e) = rec.shutdown().await
        {
            warn!("failed to shutdown rollout recorder: {e}");
            let event = Event {
                id: sub_id.clone(),
                msg: EventMsg::Error(ErrorEvent {
                    message: "Failed to shutdown rollout recorder".to_string(),
                    codex_error_info: Some(CodexErrorInfo::Other),
                }),
            };
            sess.send_event_raw(event).await;
        }

        let event = Event {
            id: sub_id,
            msg: EventMsg::ShutdownComplete,
        };
        sess.send_event_raw(event).await;
        true
    }

    pub async fn review(
        sess: &Arc<Session>,
        config: &Arc<Config>,
        sub_id: String,
        review_request: ReviewRequest,
    ) {
        let turn_context = sess.new_default_turn_with_sub_id(sub_id.clone()).await;
        sess.refresh_mcp_servers_if_requested(&turn_context).await;
        match resolve_review_request(review_request, turn_context.cwd.as_path()) {
            Ok(resolved) => {
                spawn_review_thread(
                    Arc::clone(sess),
                    Arc::clone(config),
                    turn_context.clone(),
                    sub_id,
                    resolved,
                )
                .await;
            }
            Err(err) => {
                let event = Event {
                    id: sub_id,
                    msg: EventMsg::Error(ErrorEvent {
                        message: err.to_string(),
                        codex_error_info: Some(CodexErrorInfo::Other),
                    }),
                };
                sess.send_event(&turn_context, event.msg).await;
            }
        }
    }
}

fn should_track_recent_real_user_input(
    session_source: &SessionSource,
    origin: UserInputOrigin,
) -> bool {
    !matches!(session_source, SessionSource::SubAgent(_)) && matches!(origin, UserInputOrigin::User)
}

fn render_recent_real_user_input(items: &[UserInput]) -> Option<String> {
    let rendered = items
        .iter()
        .map(|item| match item {
            UserInput::Text { text, .. } => text.clone(),
            UserInput::Mention { name, .. } => format!("@{name}"),
            UserInput::Skill { name, .. } => format!("[Skill: {name}]"),
            UserInput::Image { image_url } => format!("[Image: {image_url}]"),
            UserInput::LocalImage { path } => {
                format!("[Local image: {}]", path.display())
            }
            _ => "[Input]".to_string(),
        })
        .collect::<Vec<_>>()
        .join("");

    let trimmed = rendered.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Spawn a review thread using the given prompt.
async fn spawn_review_thread(
    sess: Arc<Session>,
    config: Arc<Config>,
    parent_turn_context: Arc<TurnContext>,
    sub_id: String,
    resolved: crate::review_prompts::ResolvedReviewRequest,
) {
    let model = config
        .review_model
        .clone()
        .unwrap_or_else(|| parent_turn_context.model_info.slug.clone());
    let review_model_info = sess
        .services
        .models_manager
        .get_model_info(&model, &config)
        .await;
    // For reviews, disable web_search and view_image regardless of global settings.
    let mut review_features = sess.features.clone();
    review_features
        .disable(crate::features::Feature::WebSearchRequest)
        .disable(crate::features::Feature::WebSearchCached);
    let review_web_search_mode = WebSearchMode::Disabled;
    let tools_config = ToolsConfig::new(&ToolsConfigParams {
        model_info: &review_model_info,
        features: &review_features,
        web_search_mode: Some(review_web_search_mode),
        mode_kind: parent_turn_context.collaboration_mode.mode,
    });

    let review_prompt = resolved.prompt.clone();
    let provider = parent_turn_context.provider.clone();
    let auth_manager = parent_turn_context.auth_manager.clone();
    let model_info = review_model_info.clone();

    // Build per‑turn client with the requested model/family.
    let mut per_turn_config = (*config).clone();
    per_turn_config.model = Some(model.clone());
    per_turn_config.features = review_features.clone();
    if let Err(err) = per_turn_config.web_search_mode.set(review_web_search_mode) {
        let fallback_value = per_turn_config.web_search_mode.value();
        tracing::warn!(
            error = %err,
            ?review_web_search_mode,
            ?fallback_value,
            "review web_search_mode is disallowed by requirements; keeping constrained value"
        );
    }

    let otel_manager = parent_turn_context
        .otel_manager
        .clone()
        .with_model(model.as_str(), review_model_info.slug.as_str());
    let auth_manager_for_context = auth_manager.clone();
    let provider_for_context = provider.clone();
    let otel_manager_for_context = otel_manager.clone();
    let reasoning_effort = per_turn_config.model_reasoning_effort;
    let reasoning_summary = per_turn_config.model_reasoning_summary;
    let session_source = parent_turn_context.session_source.clone();

    let per_turn_config = Arc::new(per_turn_config);
    let review_turn_id = sub_id.to_string();
    let turn_metadata_state = Arc::new(TurnMetadataState::new(
        review_turn_id.clone(),
        parent_turn_context.cwd.clone(),
        &parent_turn_context.sandbox_policy,
        parent_turn_context.windows_sandbox_level,
        parent_turn_context
            .features
            .enabled(Feature::UseLinuxSandboxBwrap),
    ));

    let review_turn_context = TurnContext {
        sub_id: review_turn_id,
        config: per_turn_config,
        auth_manager: auth_manager_for_context,
        model_info: model_info.clone(),
        otel_manager: otel_manager_for_context,
        provider: provider_for_context,
        reasoning_effort,
        reasoning_summary,
        session_source,
        tools_config,
        features: parent_turn_context.features.clone(),
        ghost_snapshot: parent_turn_context.ghost_snapshot.clone(),
        developer_instructions: None,
        user_instructions: None,
        compact_prompt: parent_turn_context.compact_prompt.clone(),
        collaboration_mode: parent_turn_context.collaboration_mode.clone(),
        personality: parent_turn_context.personality,
        approval_policy: parent_turn_context.approval_policy,
        sandbox_policy: parent_turn_context.sandbox_policy.clone(),
        network: parent_turn_context.network.clone(),
        windows_sandbox_level: parent_turn_context.windows_sandbox_level,
        shell_environment_policy: parent_turn_context.shell_environment_policy.clone(),
        cwd: parent_turn_context.cwd.clone(),
        final_output_json_schema: None,
        codex_linux_sandbox_exe: parent_turn_context.codex_linux_sandbox_exe.clone(),
        tool_call_gate: Arc::new(ReadinessFlag::new()),
        js_repl: Arc::clone(&sess.js_repl),
        dynamic_tools: parent_turn_context.dynamic_tools.clone(),
        truncation_policy: model_info.truncation_policy.into(),
        turn_metadata_state,
    };

    // Seed the child task with the review prompt as the initial user message.
    let input: Vec<UserInput> = vec![UserInput::Text {
        text: review_prompt,
        // Review prompt is synthesized; no UI element ranges to preserve.
        text_elements: Vec::new(),
    }];
    let tc = Arc::new(review_turn_context);
    tc.turn_metadata_state.spawn_git_enrichment_task();
    sess.spawn_task(tc.clone(), input, ReviewTask::new()).await;

    // Announce entering review mode so UIs can switch modes.
    let review_request = ReviewRequest {
        target: resolved.target,
        user_facing_hint: Some(resolved.user_facing_hint),
    };
    sess.send_event(&tc, EventMsg::EnteredReviewMode(review_request))
        .await;
}

fn skills_to_info(
    skills: &[SkillMetadata],
    disabled_paths: &HashSet<PathBuf>,
) -> Vec<ProtocolSkillMetadata> {
    skills
        .iter()
        .map(|skill| ProtocolSkillMetadata {
            name: skill.name.clone(),
            description: skill.description.clone(),
            short_description: skill.short_description.clone(),
            interface: skill
                .interface
                .clone()
                .map(|interface| ProtocolSkillInterface {
                    display_name: interface.display_name,
                    short_description: interface.short_description,
                    icon_small: interface.icon_small,
                    icon_large: interface.icon_large,
                    brand_color: interface.brand_color,
                    default_prompt: interface.default_prompt,
                }),
            dependencies: skill.dependencies.clone().map(|dependencies| {
                ProtocolSkillDependencies {
                    tools: dependencies
                        .tools
                        .into_iter()
                        .map(|tool| ProtocolSkillToolDependency {
                            r#type: tool.r#type,
                            value: tool.value,
                            description: tool.description,
                            transport: tool.transport,
                            command: tool.command,
                            url: tool.url,
                        })
                        .collect(),
                }
            }),
            path: skill.path.clone(),
            scope: skill.scope,
            enabled: !disabled_paths.contains(&skill.path),
        })
        .collect()
}

fn errors_to_info(errors: &[SkillError]) -> Vec<SkillErrorInfo> {
    errors
        .iter()
        .map(|err| SkillErrorInfo {
            path: err.path.clone(),
            message: err.message.clone(),
        })
        .collect()
}

/// Takes a user message as input and runs a loop where, at each sampling request, the model
/// replies with either:
///
/// - requested function calls
/// - an assistant message
///
/// While it is possible for the model to return multiple of these items in a
/// single sampling request, in practice, we generally one item per sampling request:
///
/// - If the model requests a function call, we execute it and send the output
///   back to the model in the next sampling request.
/// - If the model sends only an assistant message, we record it in the
///   conversation history and consider the turn complete.
///
pub(crate) async fn run_turn(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    input: Vec<UserInput>,
    prewarmed_client_session: Option<ModelClientSession>,
    cancellation_token: CancellationToken,
) -> Option<String> {
    if input.is_empty() {
        return None;
    }

    let model_info = turn_context.model_info.clone();
    let auto_compact_limit = model_info.auto_compact_token_limit().unwrap_or(i64::MAX);

    let event = EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: turn_context.sub_id.clone(),
        model_context_window: turn_context.model_context_window(),
        collaboration_mode_kind: turn_context.collaboration_mode.mode,
    });
    sess.send_event(&turn_context, event).await;
    if run_pre_sampling_compact(&sess, &turn_context)
        .await
        .is_err()
    {
        error!("Failed to run pre-sampling compact");
        return None;
    }

    let skills_outcome = Some(
        sess.services
            .skills_manager
            .skills_for_cwd(&turn_context.cwd, false)
            .await,
    );

    let available_connectors = if turn_context.config.features.enabled(Feature::Apps) {
        let mcp_tools = match sess
            .services
            .mcp_connection_manager
            .read()
            .await
            .list_all_tools()
            .or_cancel(&cancellation_token)
            .await
        {
            Ok(mcp_tools) => mcp_tools,
            Err(_) => return None,
        };
        connectors::with_app_enabled_state(
            connectors::accessible_connectors_from_mcp_tools(&mcp_tools),
            &turn_context.config,
        )
    } else {
        Vec::new()
    };
    let connector_slug_counts = build_connector_slug_counts(&available_connectors);
    let skill_name_counts_lower = skills_outcome
        .as_ref()
        .map_or_else(HashMap::new, |outcome| {
            build_skill_name_counts(&outcome.skills, &outcome.disabled_paths).1
        });
    let mentioned_skills = skills_outcome.as_ref().map_or_else(Vec::new, |outcome| {
        collect_explicit_skill_mentions(
            &input,
            &outcome.skills,
            &outcome.disabled_paths,
            &connector_slug_counts,
        )
    });
    let config = turn_context.config.clone();
    if config
        .features
        .enabled(Feature::SkillEnvVarDependencyPrompt)
    {
        let env_var_dependencies = collect_env_var_dependencies(&mentioned_skills);
        resolve_skill_dependencies_for_turn(&sess, &turn_context, &env_var_dependencies).await;
    }

    maybe_prompt_and_install_mcp_dependencies(
        sess.as_ref(),
        turn_context.as_ref(),
        &cancellation_token,
        &mentioned_skills,
    )
    .await;

    let otel_manager = turn_context.otel_manager.clone();
    let thread_id = sess.conversation_id.to_string();
    let tracking = build_track_events_context(
        turn_context.model_info.slug.clone(),
        thread_id,
        turn_context.sub_id.clone(),
    );
    let SkillInjections {
        items: skill_items,
        warnings: skill_warnings,
    } = build_skill_injections(
        &mentioned_skills,
        Some(&otel_manager),
        &sess.services.analytics_events_client,
        tracking.clone(),
    )
    .await;

    for message in skill_warnings {
        sess.send_event(&turn_context, EventMsg::Warning(WarningEvent { message }))
            .await;
    }

    let mut explicitly_enabled_connectors = collect_explicit_app_ids(&input);
    explicitly_enabled_connectors.extend(collect_explicit_app_ids_from_skill_items(
        &skill_items,
        &available_connectors,
        &skill_name_counts_lower,
    ));
    let connector_names_by_id = available_connectors
        .iter()
        .map(|connector| (connector.id.as_str(), connector.name.as_str()))
        .collect::<HashMap<&str, &str>>();
    let mentioned_app_invocations = explicitly_enabled_connectors
        .iter()
        .map(|connector_id| AppInvocation {
            connector_id: Some(connector_id.clone()),
            app_name: connector_names_by_id
                .get(connector_id.as_str())
                .map(|name| (*name).to_string()),
            invoke_type: Some("explicit".to_string()),
        })
        .collect::<Vec<_>>();
    sess.services
        .analytics_events_client
        .track_app_mentioned(tracking.clone(), mentioned_app_invocations);
    sess.merge_connector_selection(explicitly_enabled_connectors.clone())
        .await;

    let initial_input_for_turn: ResponseInputItem = ResponseInputItem::from(input.clone());
    let response_item: ResponseItem = initial_input_for_turn.clone().into();
    sess.record_user_prompt_and_emit_turn_item(turn_context.as_ref(), &input, response_item)
        .await;

    if !skill_items.is_empty() {
        sess.record_conversation_items(&turn_context, &skill_items)
            .await;
    }

    sess.maybe_start_ghost_snapshot(Arc::clone(&turn_context), cancellation_token.child_token())
        .await;
    let mut last_agent_message: Option<String> = None;
    // Although from the perspective of codex.rs, TurnDiffTracker has the lifecycle of a Task which contains
    // many turns, from the perspective of the user, it is a single turn.
    let turn_diff_tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));

    // `ModelClientSession` is turn-scoped and caches WebSocket + sticky routing state, so we reuse
    // one instance across retries within this turn.
    let mut client_session =
        prewarmed_client_session.unwrap_or_else(|| sess.services.model_client.new_session());

    loop {
        maybe_inject_primary_completion_notice_for_subagent(&sess, &turn_context).await;

        // Note that pending_input would be something like a message the user
        // submitted through the UI while the model was running. Though the UI
        // may support this, the model might not.
        let pending_response_items = sess
            .get_pending_input()
            .await
            .into_iter()
            .map(ResponseItem::from)
            .collect::<Vec<ResponseItem>>();

        if !pending_response_items.is_empty() {
            for response_item in pending_response_items {
                if let Some(TurnItem::UserMessage(user_message)) = parse_turn_item(&response_item) {
                    // todo(aibrahim): move pending input to be UserInput only to keep TextElements. context: https://github.com/openai/codex/pull/10656#discussion_r2765522480
                    sess.record_user_prompt_and_emit_turn_item(
                        turn_context.as_ref(),
                        &user_message.content,
                        response_item,
                    )
                    .await;
                } else {
                    sess.record_conversation_items(
                        &turn_context,
                        std::slice::from_ref(&response_item),
                    )
                    .await;
                }
            }
        }

        // Construct the input that we will send to the model.
        let mut sampling_request_input: Vec<ResponseItem> = sess
            .clone_history()
            .await
            .for_prompt(&turn_context.model_info.input_modalities);
        append_user_input_complexity_guidance(&mut sampling_request_input);
        if turn_context.collaboration_mode.mode == ModeKind::Swarm {
            append_swarm_latest_pair_and_blackboard_messages(
                sess.as_ref(),
                turn_context.as_ref(),
                &mut sampling_request_input,
            )
            .await;
        }

        let sampling_request_input_messages = sampling_request_input
            .iter()
            .filter_map(|item| match parse_turn_item(item) {
                Some(TurnItem::UserMessage(user_message)) => Some(user_message),
                _ => None,
            })
            .map(|user_message| user_message.message())
            .collect::<Vec<String>>();
        let debug_trace_event = if let Some(output_root) =
            debug_trace::trace_output_root(turn_context.config.codex_home.as_path())
        {
            let shared_blackboard_path = {
                let state = sess.state.lock().await;
                state.shared_blackboard_path()
            };
            let trace_context = debug_trace::DebugTraceContext {
                conversation_id: sess.conversation_id,
                turn_id: turn_context.sub_id.clone(),
                session_source: turn_context.session_source.clone(),
                collaboration_mode_kind: turn_context.collaboration_mode.mode,
                reasoning_effort: turn_context.reasoning_effort,
                base_instructions: None,
                initial_context_items: None,
                developer_instructions: turn_context.developer_instructions.clone(),
                user_instructions: turn_context.user_instructions.clone(),
                agent_name: sess
                    .services
                    .agent_control
                    .agent_name_for_thread(sess.conversation_id),
                shared_blackboard_path,
            };
            debug_trace::record_request_context(
                output_root.as_path(),
                &trace_context,
                &sampling_request_input,
            )
        } else {
            None
        };
        if let Some(snapshot_event) = debug_trace_event {
            sess.send_event_raw(Event {
                id: turn_context.sub_id.clone(),
                msg: EventMsg::DebugTraceSnapshot(snapshot_event),
            })
            .await;
        }
        let turn_metadata_header = turn_context.turn_metadata_state.current_header_value();
        match run_sampling_request(
            Arc::clone(&sess),
            Arc::clone(&turn_context),
            Arc::clone(&turn_diff_tracker),
            &mut client_session,
            turn_metadata_header.as_deref(),
            sampling_request_input,
            &explicitly_enabled_connectors,
            skills_outcome.as_ref(),
            cancellation_token.child_token(),
        )
        .await
        {
            Ok(sampling_request_output) => {
                let SamplingRequestResult {
                    needs_follow_up,
                    last_agent_message: sampling_request_last_agent_message,
                } = sampling_request_output;
                let total_usage_tokens = sess.get_total_token_usage().await;
                let token_limit_reached = total_usage_tokens >= auto_compact_limit;

                let estimated_token_count =
                    sess.get_estimated_token_count(turn_context.as_ref()).await;

                trace!(
                    turn_id = %turn_context.sub_id,
                    total_usage_tokens,
                    estimated_token_count = ?estimated_token_count,
                    auto_compact_limit,
                    token_limit_reached,
                    needs_follow_up,
                    "post sampling token usage"
                );

                // as long as compaction works well in getting us way below the token limit, we shouldn't worry about being in an infinite loop.
                if token_limit_reached && needs_follow_up {
                    if run_auto_compact(&sess, &turn_context).await.is_err() {
                        return None;
                    }
                    continue;
                }

                if !needs_follow_up {
                    match tokio::time::timeout(
                        TERMINATION_JUDGE_TIMEOUT,
                        maybe_run_termination_judge(
                            &sess,
                            &turn_context,
                            &sampling_request_last_agent_message,
                        ),
                    )
                    .await
                    {
                        Ok(Ok(TerminationJudgeDecision::Continue)) => {
                            if enqueue_continue_user_message(&sess).await {
                                continue;
                            }
                            warn!(
                                turn_id = %turn_context.sub_id,
                                "termination judge requested continue but pending input injection failed; finalizing turn"
                            );
                        }
                        Ok(Ok(TerminationJudgeDecision::Finalize { user_next_steps })) => {
                            if let Some(message) = user_next_steps
                                && !message.trim().is_empty()
                            {
                                sess.send_event(
                                    &turn_context,
                                    EventMsg::StreamInfo(StreamInfoEvent { message }),
                                )
                                .await;
                            }
                        }
                        Ok(Err(err)) => {
                            warn!(
                                turn_id = %turn_context.sub_id,
                                "termination judge failed, finalizing turn normally: {err:#}"
                            );
                        }
                        Err(_) => {
                            warn!(
                                turn_id = %turn_context.sub_id,
                                timeout_ms = TERMINATION_JUDGE_TIMEOUT.as_millis(),
                                "termination judge timed out, finalizing turn normally"
                            );
                        }
                    }

                    if maybe_enqueue_swarm_pending_required_reply_finalization_guard(
                        &sess,
                        &turn_context,
                        &sampling_request_last_agent_message,
                    )
                    .await
                    {
                        continue;
                    }

                    if maybe_enqueue_swarm_required_reply_reminder(&sess, &turn_context).await {
                        continue;
                    }

                    maybe_clear_required_reply_obligations_after_primary_completion(
                        &sess,
                        &turn_context,
                        &sampling_request_last_agent_message,
                    )
                    .await;

                    maybe_queue_primary_completion_notice_for_running_agents(
                        &sess,
                        &turn_context,
                        &sampling_request_last_agent_message,
                    )
                    .await;
                    last_agent_message = sampling_request_last_agent_message;
                    sess.hooks()
                        .dispatch(HookPayload {
                            session_id: sess.conversation_id,
                            cwd: turn_context.cwd.clone(),
                            triggered_at: chrono::Utc::now(),
                            hook_event: HookEvent::AfterAgent {
                                event: HookEventAfterAgent {
                                    thread_id: sess.conversation_id,
                                    turn_id: turn_context.sub_id.clone(),
                                    input_messages: sampling_request_input_messages,
                                    last_assistant_message: last_agent_message.clone(),
                                },
                            },
                        })
                        .await;
                    break;
                }
                continue;
            }
            Err(CodexErr::TurnAborted) => {
                // Aborted turn is reported via a different event.
                break;
            }
            Err(CodexErr::InvalidImageRequest()) => {
                let mut state = sess.state.lock().await;
                error_or_panic(
                    "Invalid image detected; sanitizing tool output to prevent poisoning",
                );
                if state.history.replace_last_turn_images("Invalid image") {
                    continue;
                }
                let event = EventMsg::Error(ErrorEvent {
                    message: "Invalid image in your last message. Please remove it and try again."
                        .to_string(),
                    codex_error_info: Some(CodexErrorInfo::BadRequest),
                });
                sess.send_event(&turn_context, event).await;
                break;
            }
            Err(e) => {
                info!("Turn error: {e:#}");
                let event = EventMsg::Error(e.to_error_event(None));
                sess.send_event(&turn_context, event).await;
                // let the user continue the conversation
                break;
            }
        }
    }

    last_agent_message
}

async fn run_pre_sampling_compact(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
) -> CodexResult<()> {
    let total_usage_tokens_before_compaction = sess.get_total_token_usage().await;
    maybe_run_previous_model_inline_compact(
        sess,
        turn_context,
        total_usage_tokens_before_compaction,
    )
    .await?;
    let total_usage_tokens = sess.get_total_token_usage().await;
    let auto_compact_limit = turn_context
        .model_info
        .auto_compact_token_limit()
        .unwrap_or(i64::MAX);
    // Compact if the total usage tokens are greater than the auto compact limit
    if total_usage_tokens >= auto_compact_limit {
        run_auto_compact(sess, turn_context).await?;
    }
    Ok(())
}

/// Runs pre-sampling compaction against the previous model when switching to a smaller
/// context-window model.
///
/// Returns `Ok(())` when compaction either completed successfully or was skipped because the
/// model/context-window preconditions were not met. Returns `Err(_)` only when compaction was
/// attempted and failed.
async fn maybe_run_previous_model_inline_compact(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    total_usage_tokens: i64,
) -> CodexResult<()> {
    let Some(previous_model) = sess.previous_model().await else {
        return Ok(());
    };
    let previous_turn_context = Arc::new(
        turn_context
            .with_model(previous_model, &sess.services.models_manager)
            .await,
    );

    let Some(old_context_window) = previous_turn_context.model_context_window() else {
        return Ok(());
    };
    let Some(new_context_window) = turn_context.model_context_window() else {
        return Ok(());
    };
    let new_auto_compact_limit = turn_context
        .model_info
        .auto_compact_token_limit()
        .unwrap_or(i64::MAX);
    let should_run = total_usage_tokens > new_auto_compact_limit
        && previous_turn_context.model_info.slug != turn_context.model_info.slug
        && old_context_window > new_context_window;
    if should_run {
        run_auto_compact(sess, &previous_turn_context).await?;
    }
    Ok(())
}

async fn run_auto_compact(sess: &Arc<Session>, turn_context: &Arc<TurnContext>) -> CodexResult<()> {
    if should_use_remote_compact_task(&turn_context.provider) {
        run_inline_remote_auto_compact_task(Arc::clone(sess), Arc::clone(turn_context)).await?;
    } else {
        run_inline_auto_compact_task(Arc::clone(sess), Arc::clone(turn_context)).await?;
    }
    Ok(())
}

fn collect_explicit_app_ids_from_skill_items(
    skill_items: &[ResponseItem],
    connectors: &[connectors::AppInfo],
    skill_name_counts_lower: &HashMap<String, usize>,
) -> HashSet<String> {
    if skill_items.is_empty() || connectors.is_empty() {
        return HashSet::new();
    }

    let skill_messages = skill_items
        .iter()
        .filter_map(|item| match item {
            ResponseItem::Message { content, .. } => {
                content.iter().find_map(|content_item| match content_item {
                    ContentItem::InputText { text } => Some(text.clone()),
                    _ => None,
                })
            }
            _ => None,
        })
        .collect::<Vec<String>>();
    if skill_messages.is_empty() {
        return HashSet::new();
    }

    let mentions = collect_tool_mentions_from_messages(&skill_messages);
    let mention_names_lower = mentions
        .plain_names
        .iter()
        .map(|name| name.to_ascii_lowercase())
        .collect::<HashSet<String>>();
    let mut connector_ids = mentions
        .paths
        .iter()
        .filter(|path| tool_kind_for_path(path) == ToolMentionKind::App)
        .filter_map(|path| app_id_from_path(path).map(str::to_string))
        .collect::<HashSet<String>>();

    let connector_slug_counts = build_connector_slug_counts(connectors);
    for connector in connectors {
        let slug = connectors::connector_mention_slug(connector);
        let connector_count = connector_slug_counts.get(&slug).copied().unwrap_or(0);
        let skill_count = skill_name_counts_lower.get(&slug).copied().unwrap_or(0);
        if connector_count == 1 && skill_count == 0 && mention_names_lower.contains(&slug) {
            connector_ids.insert(connector.id.clone());
        }
    }

    connector_ids
}

fn filter_connectors_for_input(
    connectors: &[connectors::AppInfo],
    input: &[ResponseItem],
    explicitly_enabled_connectors: &HashSet<String>,
    skill_name_counts_lower: &HashMap<String, usize>,
) -> Vec<connectors::AppInfo> {
    let connectors: Vec<connectors::AppInfo> = connectors
        .iter()
        .filter(|connector| connector.is_enabled)
        .cloned()
        .collect::<Vec<_>>();
    if connectors.is_empty() {
        return Vec::new();
    }

    let user_messages = collect_user_messages(input);
    if user_messages.is_empty() && explicitly_enabled_connectors.is_empty() {
        return Vec::new();
    }

    let mentions = collect_tool_mentions_from_messages(&user_messages);
    let mention_names_lower = mentions
        .plain_names
        .iter()
        .map(|name| name.to_ascii_lowercase())
        .collect::<HashSet<String>>();

    let connector_slug_counts = build_connector_slug_counts(&connectors);
    let mut allowed_connector_ids = explicitly_enabled_connectors.clone();
    for path in mentions
        .paths
        .iter()
        .filter(|path| tool_kind_for_path(path) == ToolMentionKind::App)
    {
        if let Some(connector_id) = app_id_from_path(path) {
            allowed_connector_ids.insert(connector_id.to_string());
        }
    }

    connectors
        .into_iter()
        .filter(|connector| {
            connector_inserted_in_messages(
                connector,
                &mention_names_lower,
                &allowed_connector_ids,
                &connector_slug_counts,
                skill_name_counts_lower,
            )
        })
        .collect()
}

fn connector_inserted_in_messages(
    connector: &connectors::AppInfo,
    mention_names_lower: &HashSet<String>,
    allowed_connector_ids: &HashSet<String>,
    connector_slug_counts: &HashMap<String, usize>,
    skill_name_counts_lower: &HashMap<String, usize>,
) -> bool {
    if allowed_connector_ids.contains(&connector.id) {
        return true;
    }

    let mention_slug = connectors::connector_mention_slug(connector);
    let connector_count = connector_slug_counts
        .get(&mention_slug)
        .copied()
        .unwrap_or(0);
    let skill_count = skill_name_counts_lower
        .get(&mention_slug)
        .copied()
        .unwrap_or(0);
    connector_count == 1 && skill_count == 0 && mention_names_lower.contains(&mention_slug)
}

fn filter_codex_apps_mcp_tools(
    mcp_tools: &HashMap<String, crate::mcp_connection_manager::ToolInfo>,
    connectors: &[connectors::AppInfo],
) -> HashMap<String, crate::mcp_connection_manager::ToolInfo> {
    let allowed: HashSet<&str> = connectors
        .iter()
        .map(|connector| connector.id.as_str())
        .collect();

    mcp_tools
        .iter()
        .filter(|(_, tool)| {
            if tool.server_name != CODEX_APPS_MCP_SERVER_NAME {
                return true;
            }
            let Some(connector_id) = codex_apps_connector_id(tool) else {
                return false;
            };
            allowed.contains(connector_id)
        })
        .map(|(name, tool)| (name.clone(), tool.clone()))
        .collect()
}

fn codex_apps_connector_id(tool: &crate::mcp_connection_manager::ToolInfo) -> Option<&str> {
    tool.connector_id.as_deref()
}

const WEBSOCKET_RECONNECT_NOTIFICATION_DELAY: Duration = Duration::from_secs(20);
const INITIAL_STREAM_START_MAX_ATTEMPTS: u64 = 8;
const INITIAL_STREAM_START_RETRY_DELAYS: [Duration; 7] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(15),
    Duration::from_secs(30),
    Duration::from_secs(45),
];
const SWARM_COLLAB_CONTEXT_ASSISTANT_ACK: &str = "I understand the status of the agents collaborating with me, as well as the content in the shared blackboard. I will continue the task based on this information.";
const MAX_COLLAB_SUMMARIES_PER_AGENT: usize = 40;
const USER_INPUT_COMPLEXITY_GUIDANCE: &str = "First, analyze the core objective and complexity of the task. For simple tasks, use a single agent; for complex ones, recommend clear, non-overlapping multi-agent collaboration. **Specifically, if in Debug Mode, prioritize a multi-agent exploration strategy to ensure multi-dimensional troubleshooting and a guaranteed fix.** For coding tasks, you must thoroughly review the relevant codebase before deciding on the agent structure.";
const PRIMARY_AGENT_COMPLETED_NOTICE: &str = "Primary agent has already completed the user-facing response for this task and stopped. Before entering another loop, decide whether you truly need to send more input to the primary agent. Unless there is a clear error in prior output or another critical issue, stop now instead of continuing.";

fn render_swarm_blackboard_developer_instructions(
    agent_name: &str,
    blackboard_path: &Path,
) -> String {
    let lock_path = blackboard::blackboard_lock_path(blackboard_path);
    format!(
        "<swarm_shared_blackboard>\nShared blackboard is enabled for this session.\n- Your agent name: {agent_name}\n- Blackboard file path: {}\n- Blackboard lock file path: {}\nRules:\n1. For blackboard writes or modifications, use shell commands directly.\n2. Always hold an exclusive lock on the lock file while mutating the blackboard file.\n3. Every written line MUST use this exact format: [agent_name]：message_content\n4. Replace `agent_name` with your own injected name exactly (`{agent_name}`).\n5. Never hardcode another agent's name (especially `wmj-assistant`) unless it is exactly your own name.\n6. Preferred append pattern: flock -x \"{}\" -c 'printf \"%s\\n\" \"[{agent_name}]：your message\" >> \"{}\"'\n</swarm_shared_blackboard>",
        blackboard_path.display(),
        lock_path.display(),
        lock_path.display(),
        blackboard_path.display()
    )
}

async fn append_swarm_latest_pair_and_blackboard_messages(
    session: &Session,
    turn_context: &TurnContext,
    input: &mut Vec<ResponseItem>,
) {
    if turn_context.collaboration_mode.mode != ModeKind::Swarm {
        return;
    }

    let statuses = session
        .services
        .agent_control
        .other_agents_work_status(session.conversation_id);

    let mut collaborator_thread_ids = session
        .services
        .agent_control
        .list_agent_ids()
        .await
        .unwrap_or_else(|_| statuses.iter().map(|status| status.thread_id).collect());
    collaborator_thread_ids.retain(|thread_id| *thread_id != session.conversation_id);
    for status in &statuses {
        if !collaborator_thread_ids.contains(&status.thread_id) {
            collaborator_thread_ids.push(status.thread_id);
        }
    }

    if !matches!(turn_context.session_source, SessionSource::SubAgent(_))
        && collaborator_thread_ids.is_empty()
    {
        // Do not inject this context for the primary agent when no sub-agent exists.
        return;
    }

    let mut status_by_thread = HashMap::new();
    for status in &statuses {
        status_by_thread.insert(status.thread_id, status);
    }
    let mut collaborators = collaborator_thread_ids
        .into_iter()
        .map(|thread_id| {
            let agent_name = visible_agent_name(&session.services.agent_control, thread_id);
            let summaries = status_by_thread
                .get(&thread_id)
                .map(|status| {
                    status
                        .entries
                        .iter()
                        .take(MAX_COLLAB_SUMMARIES_PER_AGENT)
                        .map(|entry| format_collab_summary_entry(&entry.summary, entry.recorded_at))
                        .filter(|summary| !summary.is_empty())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            (agent_name, summaries)
        })
        .collect::<Vec<_>>();
    collaborators.sort_unstable_by(|left, right| {
        left.0
            .to_ascii_lowercase()
            .cmp(&right.0.to_ascii_lowercase())
            .then_with(|| left.0.cmp(&right.0))
    });
    let collaborators_text = if collaborators.is_empty() {
        "- (no collaborating agents currently active)".to_string()
    } else {
        collaborators
            .iter()
            .map(|(agent_name, summaries)| {
                if summaries.is_empty() {
                    format!("- {agent_name}: []")
                } else {
                    format!("- {agent_name}: [{}]", summaries.join(" | "))
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let shared_blackboard_path = {
        let state = session.state.lock().await;
        state.shared_blackboard_path()
    };
    let blackboard_snapshot = match shared_blackboard_path {
        Some(path) => match blackboard::read_blackboard_snapshot(
            &path,
            DEFAULT_BLACKBOARD_SNAPSHOT_CHAR_LIMIT,
        )
        .await
        {
            Ok(text) if text.trim().is_empty() => {
                "(shared blackboard is currently empty)".to_string()
            }
            Ok(text) => text,
            Err(err) => format!("(failed to read shared blackboard content: {err})"),
        },
        None => "(shared blackboard is unavailable)".to_string(),
    };

    let now_utc = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC");

    input.push(ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: format!(
                "Below are the agents collaborating with you. You can use the `call` tool to communicate and collaborate with them.\nCurrent time: {now_utc}\n\nCollaborating agents and summary lists (first {MAX_COLLAB_SUMMARIES_PER_AGENT} summaries per agent):\n{collaborators_text}\n\nBelow is the shared blackboard content:\n{blackboard_snapshot}"
            ),
        }],
        end_turn: None,
        phase: None,
    });
    input.push(ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: SWARM_COLLAB_CONTEXT_ASSISTANT_ACK.to_string(),
        }],
        end_turn: None,
        phase: None,
    });
}

fn normalize_single_line_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn format_collab_summary_entry(summary: &str, recorded_at: i64) -> String {
    let summary = normalize_single_line_text(summary);
    if summary.is_empty() {
        return summary;
    }
    format!(
        "{summary} [{}]",
        format_work_entry_minute_second(recorded_at)
    )
}

fn format_work_entry_minute_second(timestamp: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(timestamp, 0)
        .map(|dt| dt.format("%M:%S").to_string())
        .unwrap_or_else(|| timestamp.to_string())
}

fn append_user_input_complexity_guidance(input: &mut [ResponseItem]) {
    for item in input {
        let ResponseItem::Message { role, content, .. } = item else {
            continue;
        };
        if role != "user" {
            continue;
        }

        let already_appended = content.iter().any(|content_item| {
            matches!(
                content_item,
                ContentItem::InputText { text } if text.contains(USER_INPUT_COMPLEXITY_GUIDANCE)
            )
        });
        if already_appended {
            continue;
        }

        if let Some(ContentItem::InputText { text }) = content
            .iter_mut()
            .rev()
            .find(|content_item| matches!(content_item, ContentItem::InputText { .. }))
        {
            if !text.trim_end().is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(USER_INPUT_COMPLEXITY_GUIDANCE);
            continue;
        }

        content.push(ContentItem::InputText {
            text: USER_INPUT_COMPLEXITY_GUIDANCE.to_string(),
        });
    }
}

async fn maybe_inject_primary_completion_notice_for_subagent(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
) {
    if !matches!(turn_context.session_source, SessionSource::SubAgent(_)) {
        return;
    }

    let Some(notice) = sess
        .services
        .agent_control
        .take_primary_completion_notice(sess.conversation_id)
    else {
        return;
    };

    let _ = sess
        .inject_response_items(vec![ResponseInputItem::Message {
            role: "user".to_string(),
            content: vec![ContentItem::InputText { text: notice }],
        }])
        .await;
}

async fn maybe_queue_primary_completion_notice_for_running_agents(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    last_agent_message: &Option<String>,
) {
    if matches!(turn_context.session_source, SessionSource::SubAgent(_)) {
        return;
    }
    if last_agent_message.is_none() {
        return;
    }

    let Ok(thread_ids) = sess.services.agent_control.list_agent_ids().await else {
        return;
    };

    for thread_id in thread_ids {
        if thread_id == sess.conversation_id {
            continue;
        }
        if !matches!(
            sess.services.agent_control.get_status(thread_id).await,
            AgentStatus::Running
        ) {
            continue;
        }
        sess.services
            .agent_control
            .queue_primary_completion_notice(thread_id, PRIMARY_AGENT_COMPLETED_NOTICE.to_string());
    }
}

async fn maybe_clear_required_reply_obligations_after_primary_completion(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    last_agent_message: &Option<String>,
) {
    if turn_context.collaboration_mode.mode != ModeKind::Swarm {
        return;
    }
    if matches!(turn_context.session_source, SessionSource::SubAgent(_)) {
        return;
    }
    if last_agent_message.is_none() {
        return;
    }

    let mut total_cleared = 0usize;
    total_cleared = total_cleared.saturating_add(
        collab_inbox::clear_required_reply_obligations_for_source_thread(sess.conversation_id),
    );

    let mut thread_ids = sess
        .services
        .agent_control
        .list_agent_ids()
        .await
        .unwrap_or_else(|_| vec![sess.conversation_id]);
    if !thread_ids.contains(&sess.conversation_id) {
        thread_ids.push(sess.conversation_id);
    }

    for thread_id in thread_ids {
        total_cleared =
            total_cleared.saturating_add(collab_inbox::clear_required_reply_obligations(thread_id));
    }

    debug!(
        thread_id = %sess.conversation_id,
        cleared_required_reply_obligations = total_cleared,
        "cleared required-reply obligations after primary completion"
    );
}

async fn maybe_enqueue_swarm_pending_required_reply_finalization_guard(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    last_agent_message: &Option<String>,
) -> bool {
    const MAX_FINALIZATION_GUARD_REMINDERS: u8 = 1;

    if turn_context.collaboration_mode.mode != ModeKind::Swarm {
        return false;
    }
    if matches!(turn_context.session_source, SessionSource::SubAgent(_)) {
        return false;
    }
    if last_agent_message.is_none() {
        return false;
    }

    let reminders = collab_inbox::claim_required_reply_reminders_from_source_thread(
        sess.conversation_id,
        MAX_FINALIZATION_GUARD_REMINDERS,
    );
    if reminders.is_empty() {
        return false;
    }

    let mut lines = vec![
        "Swarm finalization guard: you are about to finish while `call` requests you sent with `need_reply: true` are still unresolved.".to_string(),
        "Before finalizing or refusing, choose one explicit convergence action:".to_string(),
        "- use `wait` if a requested reply is still relevant and likely to arrive".to_string(),
        "- use `read_agent_status` if you need to know whether a target is still active".to_string(),
        "- proceed only if you explicitly name each unresolved `message_id` and explain why its reply can no longer affect the final answer, refusal, or verifier-facing acceptance".to_string(),
        "".to_string(),
        "Unresolved replies you are waiting for:".to_string(),
    ];
    for pending in &reminders {
        let target_agent_name = pending
            .receiver_thread_id
            .and_then(|thread_id| sess.services.agent_control.agent_name_for_thread(thread_id))
            .unwrap_or_else(|| pending.receiver_inbox_id.clone());
        let indented_content = pending
            .obligation
            .content
            .lines()
            .map(|line| format!("    {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        lines.push(format!(
            "- target_agent_name: {target_agent_name}\n  target_inbox_id: {}\n  message_id: {}\n  original_call_content:\n{}",
            pending.receiver_inbox_id, pending.obligation.message_id, indented_content
        ));
    }

    sess.inject_response_items(vec![ResponseInputItem::Message {
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: lines.join("\n"),
        }],
    }])
    .await
    .is_ok()
}

async fn maybe_enqueue_swarm_required_reply_reminder(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
) -> bool {
    const MAX_REQUIRED_REPLY_REMINDERS: u8 = 1;

    if turn_context.collaboration_mode.mode != ModeKind::Swarm {
        return false;
    }

    let reminders = collab_inbox::claim_unobserved_required_reply_reminders(
        sess.conversation_id,
        MAX_REQUIRED_REPLY_REMINDERS,
    );
    if reminders.is_empty() {
        return false;
    }

    let mut lines = vec![
        "Swarm required-reply reminder: you still owe replies for `call` requests marked `need_reply: true`.".to_string(),
        "Reply with one `call` per unresolved message using:".to_string(),
        "- `target_agent_name`: the source agent for that message".to_string(),
        "- `message_id`: a new message id for your reply".to_string(),
        "- `reply_to_message_id`: the unresolved message id shown below".to_string(),
        "- `content`: your response".to_string(),
        "".to_string(),
        "Unresolved required replies:".to_string(),
    ];
    for obligation in &reminders {
        let indented_content = obligation
            .content
            .lines()
            .map(|line| format!("    {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        lines.push(format!(
            "- source_agent_name: {}\n  source_thread_id: {}\n  message_id: {}\n  original_call_content:\n{}",
            obligation.source_agent_name,
            obligation.source_thread_id,
            obligation.message_id,
            indented_content
        ));
    }
    let text = lines.join("\n");
    sess.inject_response_items(vec![ResponseInputItem::Message {
        role: "user".to_string(),
        content: vec![ContentItem::InputText { text }],
    }])
    .await
    .is_ok()
}

async fn enqueue_continue_user_message(sess: &Arc<Session>) -> bool {
    sess.inject_response_items(vec![ResponseInputItem::Message {
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "continue".to_string(),
        }],
    }])
    .await
    .is_ok()
}

async fn maybe_run_termination_judge(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    last_agent_message: &Option<String>,
) -> CodexResult<TerminationJudgeDecision> {
    let recent_assistant_messages =
        recent_assistant_messages(sess, TERMINATION_JUDGE_RECENT_ASSISTANT_LIMIT).await;

    if recent_assistant_messages.is_empty() {
        return Ok(TerminationJudgeDecision::Finalize {
            user_next_steps: None,
        });
    }

    let mut client_session = sess.services.model_client.new_session();
    let prompt = Prompt {
        input: vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: build_termination_judge_input(&recent_assistant_messages, last_agent_message),
            }],
            end_turn: None,
            phase: None,
        }],
        tools: Vec::new(),
        parallel_tool_calls: false,
        base_instructions: BaseInstructions {
            text: termination_judge_instructions().to_string(),
        },
        personality: None,
        output_schema: Some(termination_judge_output_schema()),
    };

    let mut stream = client_session
        .stream(
            &prompt,
            &turn_context.model_info,
            &turn_context.otel_manager,
            None,
            codex_protocol::config_types::ReasoningSummary::None,
            None,
        )
        .await?;

    let mut result = String::new();
    while let Some(event) = stream.next().await.transpose()? {
        match event {
            ResponseEvent::OutputTextDelta(delta) => result.push_str(&delta),
            ResponseEvent::OutputItemDone(ResponseItem::Message { role, content, .. })
                if role == "assistant" && result.trim().is_empty() =>
            {
                let text = content
                    .iter()
                    .filter_map(|content_item| match content_item {
                        ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                            Some(text.as_str())
                        }
                        ContentItem::InputImage { .. } => None,
                    })
                    .collect::<Vec<_>>()
                    .join(
                        "
",
                    );
                result.push_str(&text);
            }
            _ => {}
        }
    }

    let parsed: TerminationJudgeOutput = serde_json::from_str(result.trim())?;
    let decision = parsed.decision.trim().to_ascii_lowercase();
    if decision == "continue" {
        return Ok(TerminationJudgeDecision::Continue);
    }

    Ok(TerminationJudgeDecision::Finalize {
        user_next_steps: parsed
            .user_next_steps
            .map(|message| message.trim().to_string())
            .filter(|message| !message.is_empty()),
    })
}

fn termination_judge_instructions() -> &'static str {
    "Judge whether the agent is actually done. Return JSON only. Use decision=continue only when the assistant itself should take another autonomous step right now. If the assistant is asking the user for clarification, requesting a goal/task, or waiting for user input before it can proceed, use decision=finalize instead. Use finalize when the assistant has handed control back to the user. Keep user_next_steps short, useful, and lightly playful. user_next_steps must stay under 50 words."
}

fn termination_judge_output_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "decision": { "type": "string", "enum": ["finalize", "continue"] },
            "user_next_steps": { "type": ["string", "null"] }
        },
        "required": ["decision", "user_next_steps"],
        "additionalProperties": false
    })
}

fn build_termination_judge_input(
    recent_assistant_messages: &[String],
    last_agent_message: &Option<String>,
) -> String {
    let mut lines = Vec::new();
    lines.push("Recent assistant messages, oldest to newest:".to_string());
    for (idx, message) in recent_assistant_messages.iter().enumerate() {
        lines.push(format!("[{}] {}", idx + 1, message));
    }
    if let Some(last_agent_message) = last_agent_message
        && !last_agent_message.trim().is_empty()
    {
        lines.push(String::new());
        lines.push(format!("last_agent_message: {}", last_agent_message.trim()));
    }
    lines.join(
        "

",
    )
}

async fn recent_assistant_messages(sess: &Arc<Session>, limit: usize) -> Vec<String> {
    let history = sess.clone_history().await;
    history
        .raw_items()
        .iter()
        .filter_map(response_item_text_if_assistant_message)
        .filter(|message| !message.trim().is_empty())
        .map(|message| message.trim().to_string())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .take(limit)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

fn response_item_text_if_assistant_message(item: &ResponseItem) -> Option<String> {
    let ResponseItem::Message { role, content, .. } = item else {
        return None;
    };
    if role != "assistant" {
        return None;
    }
    let text = content
        .iter()
        .filter_map(|content_item| match content_item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                Some(text.as_str())
            }
            ContentItem::InputImage { .. } => None,
        })
        .collect::<Vec<_>>()
        .join(
            "
",
        );
    Some(text)
}

#[allow(clippy::too_many_arguments)]
#[instrument(level = "trace",
    skip_all,
    fields(
        turn_id = %turn_context.sub_id,
        model = %turn_context.model_info.slug,
        cwd = %turn_context.cwd.display()
    )
)]
async fn run_sampling_request(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    turn_diff_tracker: SharedTurnDiffTracker,
    client_session: &mut ModelClientSession,
    turn_metadata_header: Option<&str>,
    input: Vec<ResponseItem>,
    explicitly_enabled_connectors: &HashSet<String>,
    skills_outcome: Option<&SkillLoadOutcome>,
    cancellation_token: CancellationToken,
) -> CodexResult<SamplingRequestResult> {
    let router = built_tools(
        sess.as_ref(),
        turn_context.as_ref(),
        &input,
        explicitly_enabled_connectors,
        skills_outcome,
        &cancellation_token,
    )
    .await?;

    let model_supports_parallel = turn_context.model_info.supports_parallel_tool_calls;

    let tools =
        crate::tools::spec::filter_tools_for_model(router.specs(), &turn_context.tools_config);
    let base_instructions = sess.get_base_instructions().await;

    let prompt = Prompt {
        input,
        tools,
        parallel_tool_calls: model_supports_parallel,
        base_instructions,
        personality: turn_context.personality,
        output_schema: turn_context.final_output_json_schema.clone(),
    };

    let mut retries = 0;
    let mut initial_stream_start_attempts = 1;
    let mut websocket_reconnect_wait = Duration::ZERO;
    loop {
        let err = match try_run_sampling_request(
            Arc::clone(&router),
            Arc::clone(&sess),
            Arc::clone(&turn_context),
            client_session,
            turn_metadata_header,
            Arc::clone(&turn_diff_tracker),
            &prompt,
            cancellation_token.child_token(),
        )
        .await
        {
            Ok(output) => {
                return Ok(output);
            }
            Err(CodexErr::ContextWindowExceeded) => {
                sess.set_total_tokens_full(&turn_context).await;
                return Err(CodexErr::ContextWindowExceeded);
            }
            Err(CodexErr::UsageLimitReached(e)) => {
                let rate_limits = e.rate_limits.clone();
                if let Some(rate_limits) = rate_limits {
                    sess.update_rate_limits(&turn_context, *rate_limits).await;
                }
                return Err(CodexErr::UsageLimitReached(e));
            }
            Err(err) => err,
        };

        if is_retryable_initial_stream_start_failure(&err)
            && initial_stream_start_attempts < INITIAL_STREAM_START_MAX_ATTEMPTS
        {
            initial_stream_start_attempts += 1;
            let delay = initial_stream_start_retry_delay(initial_stream_start_attempts);
            warn!(
                "model response stream failed to start - retrying sampling request ({initial_stream_start_attempts}/{INITIAL_STREAM_START_MAX_ATTEMPTS} in {delay:?})...",
            );
            sess.notify_stream_error(
                &turn_context,
                format!(
                    "Retrying model response stream start... {initial_stream_start_attempts}/{INITIAL_STREAM_START_MAX_ATTEMPTS}"
                ),
                err,
            )
            .await;
            tokio::time::sleep(delay).await;
            continue;
        }

        if !err.is_retryable() {
            return Err(err);
        }

        // Use the configured provider-specific stream retry budget.
        let max_retries = turn_context.provider.stream_max_retries();
        if retries >= max_retries
            && client_session
                .try_switch_fallback_transport(&turn_context.otel_manager, &turn_context.model_info)
        {
            sess.send_event(
                &turn_context,
                EventMsg::Warning(WarningEvent {
                    message: format!("Falling back from WebSockets to HTTPS transport. {err:#}"),
                }),
            )
            .await;
            retries = 0;
            continue;
        }
        if retries < max_retries {
            retries += 1;
            let delay = match &err {
                CodexErr::Stream(_, requested_delay) => {
                    requested_delay.unwrap_or_else(|| backoff(retries))
                }
                _ => backoff(retries),
            };
            warn!(
                "stream disconnected - retrying sampling request ({retries}/{max_retries} in {delay:?})...",
            );

            let websocket_enabled = sess
                .services
                .model_client
                .responses_websocket_enabled(&turn_context.model_info);
            if websocket_enabled {
                websocket_reconnect_wait = websocket_reconnect_wait.saturating_add(delay);
            }

            let report_error = !websocket_enabled
                || websocket_reconnect_wait >= WEBSOCKET_RECONNECT_NOTIFICATION_DELAY;

            if report_error {
                // Surface retry information to any UI/front‑end once a disconnect has lasted
                // long enough to feel user-visible, so the screen does not look stuck.
                sess.notify_stream_error(
                    &turn_context,
                    format!("Reconnecting... {retries}/{max_retries}"),
                    err,
                )
                .await;
            }
            tokio::time::sleep(delay).await;
        } else {
            return Err(err);
        }
    }
}

pub(crate) async fn build_prompt_debug_input(
    sess: Arc<Session>,
    input: Vec<UserInput>,
) -> CodexResult<Vec<ResponseItem>> {
    let turn_context = sess.new_default_turn().await;

    let skills_outcome = Some(
        sess.services
            .skills_manager
            .skills_for_cwd(&turn_context.cwd, false)
            .await,
    );

    let available_connectors = if turn_context.config.features.enabled(Feature::Apps) {
        let mcp_tools = sess
            .services
            .mcp_connection_manager
            .read()
            .await
            .list_all_tools()
            .await;
        connectors::with_app_enabled_state(
            connectors::accessible_connectors_from_mcp_tools(&mcp_tools),
            &turn_context.config,
        )
    } else {
        Vec::new()
    };
    let connector_slug_counts = build_connector_slug_counts(&available_connectors);
    let skill_name_counts_lower = skills_outcome
        .as_ref()
        .map_or_else(HashMap::new, |outcome| {
            build_skill_name_counts(&outcome.skills, &outcome.disabled_paths).1
        });
    let mentioned_skills = skills_outcome.as_ref().map_or_else(Vec::new, |outcome| {
        collect_explicit_skill_mentions(
            &input,
            &outcome.skills,
            &outcome.disabled_paths,
            &connector_slug_counts,
        )
    });

    let otel_manager = turn_context.otel_manager.clone();
    let thread_id = sess.conversation_id.to_string();
    let tracking = build_track_events_context(
        turn_context.model_info.slug.clone(),
        thread_id,
        turn_context.sub_id.clone(),
    );
    let SkillInjections {
        items: skill_items,
        warnings: _skill_warnings,
    } = build_skill_injections(
        &mentioned_skills,
        Some(&otel_manager),
        &sess.services.analytics_events_client,
        tracking,
    )
    .await;

    let mut explicitly_enabled_connectors = collect_explicit_app_ids(&input);
    explicitly_enabled_connectors.extend(collect_explicit_app_ids_from_skill_items(
        &skill_items,
        &available_connectors,
        &skill_name_counts_lower,
    ));
    sess.merge_connector_selection(explicitly_enabled_connectors.clone())
        .await;

    let initial_input_for_turn: ResponseInputItem = ResponseInputItem::from(input.clone());
    let response_item: ResponseItem = initial_input_for_turn.into();
    sess.record_user_prompt_and_emit_turn_item(turn_context.as_ref(), &input, response_item)
        .await;

    if !skill_items.is_empty() {
        sess.record_conversation_items(&turn_context, &skill_items)
            .await;
    }

    let pending_response_items = sess
        .get_pending_input()
        .await
        .into_iter()
        .map(ResponseItem::from)
        .collect::<Vec<ResponseItem>>();

    if !pending_response_items.is_empty() {
        for response_item in pending_response_items {
            if let Some(TurnItem::UserMessage(user_message)) = parse_turn_item(&response_item) {
                sess.record_user_prompt_and_emit_turn_item(
                    turn_context.as_ref(),
                    &user_message.content,
                    response_item,
                )
                .await;
            } else {
                sess.record_conversation_items(&turn_context, std::slice::from_ref(&response_item))
                    .await;
            }
        }
    }

    let mut sampling_request_input: Vec<ResponseItem> = sess
        .clone_history()
        .await
        .for_prompt(&turn_context.model_info.input_modalities);
    append_user_input_complexity_guidance(&mut sampling_request_input);
    if turn_context.collaboration_mode.mode == ModeKind::Swarm {
        append_swarm_latest_pair_and_blackboard_messages(
            sess.as_ref(),
            turn_context.as_ref(),
            &mut sampling_request_input,
        )
        .await;
    }

    let router = built_tools(
        sess.as_ref(),
        turn_context.as_ref(),
        &sampling_request_input,
        &explicitly_enabled_connectors,
        skills_outcome.as_ref(),
        &CancellationToken::new(),
    )
    .await?;

    let model_supports_parallel = turn_context.model_info.supports_parallel_tool_calls;
    let tools =
        crate::tools::spec::filter_tools_for_model(router.specs(), &turn_context.tools_config);
    let base_instructions = sess.get_base_instructions().await;

    let prompt = Prompt {
        input: sampling_request_input,
        tools,
        parallel_tool_calls: model_supports_parallel,
        base_instructions,
        personality: turn_context.personality,
        output_schema: turn_context.final_output_json_schema.clone(),
    };

    Ok(prompt.get_formatted_input())
}

async fn built_tools(
    sess: &Session,
    turn_context: &TurnContext,
    input: &[ResponseItem],
    explicitly_enabled_connectors: &HashSet<String>,
    skills_outcome: Option<&SkillLoadOutcome>,
    cancellation_token: &CancellationToken,
) -> CodexResult<Arc<ToolRouter>> {
    let mut mcp_tools = sess
        .services
        .mcp_connection_manager
        .read()
        .await
        .list_all_tools()
        .or_cancel(cancellation_token)
        .await?;

    let mut effective_explicitly_enabled_connectors = explicitly_enabled_connectors.clone();
    effective_explicitly_enabled_connectors.extend(sess.get_connector_selection().await);

    let connectors = if turn_context.features.enabled(Feature::Apps) {
        Some(connectors::with_app_enabled_state(
            connectors::accessible_connectors_from_mcp_tools(&mcp_tools),
            &turn_context.config,
        ))
    } else {
        None
    };

    if let Some(connectors) = connectors.as_ref() {
        let skill_name_counts_lower = skills_outcome.map_or_else(HashMap::new, |outcome| {
            build_skill_name_counts(&outcome.skills, &outcome.disabled_paths).1
        });

        let explicitly_enabled = filter_connectors_for_input(
            connectors,
            input,
            &effective_explicitly_enabled_connectors,
            &skill_name_counts_lower,
        );

        let mut selected_mcp_tools =
            if let Some(selected_tools) = sess.get_mcp_tool_selection().await {
                filter_mcp_tools_by_name(&mcp_tools, &selected_tools)
            } else {
                HashMap::new()
            };

        let apps_mcp_tools =
            filter_codex_apps_mcp_tools_only(&mcp_tools, explicitly_enabled.as_ref());
        selected_mcp_tools.extend(apps_mcp_tools);

        mcp_tools = selected_mcp_tools;
    }

    let app_tools = connectors
        .as_ref()
        .map(|connectors| filter_codex_apps_mcp_tools(&mcp_tools, connectors));

    Ok(Arc::new(ToolRouter::from_config(
        &turn_context.tools_config,
        Some(
            mcp_tools
                .into_iter()
                .map(|(name, tool)| (name, tool.tool))
                .collect(),
        ),
        app_tools,
        turn_context.dynamic_tools.as_slice(),
    )))
}

#[derive(Debug)]
struct SamplingRequestResult {
    needs_follow_up: bool,
    last_agent_message: Option<String>,
}

/// Ephemeral per-response state for streaming a single proposed plan.
/// This is intentionally not persisted or stored in session/state since it
/// only exists while a response is actively streaming. The final plan text
/// is extracted from the completed assistant message.
/// Tracks a single proposed plan item across a streaming response.
struct ProposedPlanItemState {
    item_id: String,
    started: bool,
    completed: bool,
}

/// Per-item plan parsers so we can buffer text while detecting `<proposed_plan>`
/// tags without ever mixing buffered lines across item ids.
struct PlanParsers {
    assistant: HashMap<String, ProposedPlanParser>,
}

impl PlanParsers {
    fn new() -> Self {
        Self {
            assistant: HashMap::new(),
        }
    }

    fn assistant_parser_mut(&mut self, item_id: &str) -> &mut ProposedPlanParser {
        self.assistant
            .entry(item_id.to_string())
            .or_insert_with(ProposedPlanParser::new)
    }

    fn take_assistant_parser(&mut self, item_id: &str) -> Option<ProposedPlanParser> {
        self.assistant.remove(item_id)
    }

    fn drain_assistant_parsers(&mut self) -> Vec<(String, ProposedPlanParser)> {
        self.assistant.drain().collect()
    }
}

/// Aggregated state used only while streaming a plan-mode response.
/// Includes per-item parsers, deferred agent message bookkeeping, and the plan item lifecycle.
struct PlanModeStreamState {
    /// Per-item parsers for assistant streams in plan mode.
    plan_parsers: PlanParsers,
    /// Agent message items started by the model but deferred until we see non-plan text.
    pending_agent_message_items: HashMap<String, TurnItem>,
    /// Agent message items whose start notification has been emitted.
    started_agent_message_items: HashSet<String>,
    /// Leading whitespace buffered until we see non-whitespace text for an item.
    leading_whitespace_by_item: HashMap<String, String>,
    /// Tracks plan item lifecycle while streaming plan output.
    plan_item_state: ProposedPlanItemState,
}

impl PlanModeStreamState {
    fn new(turn_id: &str) -> Self {
        Self {
            plan_parsers: PlanParsers::new(),
            pending_agent_message_items: HashMap::new(),
            started_agent_message_items: HashSet::new(),
            leading_whitespace_by_item: HashMap::new(),
            plan_item_state: ProposedPlanItemState::new(turn_id),
        }
    }
}

impl ProposedPlanItemState {
    fn new(turn_id: &str) -> Self {
        Self {
            item_id: format!("{turn_id}-plan"),
            started: false,
            completed: false,
        }
    }

    async fn start(&mut self, sess: &Session, turn_context: &TurnContext) {
        if self.started || self.completed {
            return;
        }
        self.started = true;
        let item = TurnItem::Plan(PlanItem {
            id: self.item_id.clone(),
            text: String::new(),
        });
        sess.emit_turn_item_started(turn_context, &item).await;
    }

    async fn push_delta(&mut self, sess: &Session, turn_context: &TurnContext, delta: &str) {
        if self.completed {
            return;
        }
        if delta.is_empty() {
            return;
        }
        let event = PlanDeltaEvent {
            thread_id: sess.conversation_id.to_string(),
            turn_id: turn_context.sub_id.clone(),
            item_id: self.item_id.clone(),
            delta: delta.to_string(),
        };
        sess.send_event(turn_context, EventMsg::PlanDelta(event))
            .await;
    }

    async fn complete_with_text(
        &mut self,
        sess: &Session,
        turn_context: &TurnContext,
        text: String,
    ) {
        if self.completed || !self.started {
            return;
        }
        self.completed = true;
        let item = TurnItem::Plan(PlanItem {
            id: self.item_id.clone(),
            text,
        });
        sess.emit_turn_item_completed(turn_context, item).await;
    }
}

/// In plan mode we defer agent message starts until the parser emits non-plan
/// text. The parser buffers each line until it can rule out a tag prefix, so
/// plan-only outputs never show up as empty assistant messages.
async fn maybe_emit_pending_agent_message_start(
    sess: &Session,
    turn_context: &TurnContext,
    state: &mut PlanModeStreamState,
    item_id: &str,
) {
    if state.started_agent_message_items.contains(item_id) {
        return;
    }
    if let Some(item) = state.pending_agent_message_items.remove(item_id) {
        sess.emit_turn_item_started(turn_context, &item).await;
        state
            .started_agent_message_items
            .insert(item_id.to_string());
    }
}

/// Agent messages are text-only today; concatenate all text entries.
fn agent_message_text(item: &codex_protocol::items::AgentMessageItem) -> String {
    item.content
        .iter()
        .map(|entry| match entry {
            codex_protocol::items::AgentMessageContent::Text { text } => text.as_str(),
        })
        .collect()
}

/// Split the stream into normal assistant text vs. proposed plan content.
/// Normal text becomes AgentMessage deltas; plan content becomes PlanDelta +
/// TurnItem::Plan.
async fn handle_plan_segments(
    sess: &Session,
    turn_context: &TurnContext,
    state: &mut PlanModeStreamState,
    item_id: &str,
    segments: Vec<ProposedPlanSegment>,
) {
    for segment in segments {
        match segment {
            ProposedPlanSegment::Normal(delta) => {
                if delta.is_empty() {
                    continue;
                }
                let has_non_whitespace = delta.chars().any(|ch| !ch.is_whitespace());
                if !has_non_whitespace && !state.started_agent_message_items.contains(item_id) {
                    let entry = state
                        .leading_whitespace_by_item
                        .entry(item_id.to_string())
                        .or_default();
                    entry.push_str(&delta);
                    continue;
                }
                let delta = if !state.started_agent_message_items.contains(item_id) {
                    if let Some(prefix) = state.leading_whitespace_by_item.remove(item_id) {
                        format!("{prefix}{delta}")
                    } else {
                        delta
                    }
                } else {
                    delta
                };
                maybe_emit_pending_agent_message_start(sess, turn_context, state, item_id).await;

                let event = AgentMessageContentDeltaEvent {
                    thread_id: sess.conversation_id.to_string(),
                    turn_id: turn_context.sub_id.clone(),
                    item_id: item_id.to_string(),
                    delta,
                };
                sess.send_event(turn_context, EventMsg::AgentMessageContentDelta(event))
                    .await;
            }
            ProposedPlanSegment::ProposedPlanStart => {
                if !state.plan_item_state.completed {
                    state.plan_item_state.start(sess, turn_context).await;
                }
            }
            ProposedPlanSegment::ProposedPlanDelta(delta) => {
                if !state.plan_item_state.completed {
                    if !state.plan_item_state.started {
                        state.plan_item_state.start(sess, turn_context).await;
                    }
                    state
                        .plan_item_state
                        .push_delta(sess, turn_context, &delta)
                        .await;
                }
            }
            ProposedPlanSegment::ProposedPlanEnd => {}
        }
    }
}

/// Flush any buffered proposed-plan segments when a specific assistant message ends.
async fn flush_proposed_plan_segments_for_item(
    sess: &Session,
    turn_context: &TurnContext,
    state: &mut PlanModeStreamState,
    item_id: &str,
) {
    let Some(mut parser) = state.plan_parsers.take_assistant_parser(item_id) else {
        return;
    };
    let segments = parser.finish();
    if segments.is_empty() {
        return;
    }
    handle_plan_segments(sess, turn_context, state, item_id, segments).await;
}

/// Flush any remaining assistant plan parsers when the response completes.
async fn flush_proposed_plan_segments_all(
    sess: &Session,
    turn_context: &TurnContext,
    state: &mut PlanModeStreamState,
) {
    for (item_id, mut parser) in state.plan_parsers.drain_assistant_parsers() {
        let segments = parser.finish();
        if segments.is_empty() {
            continue;
        }
        handle_plan_segments(sess, turn_context, state, &item_id, segments).await;
    }
}

/// Emit completion for plan items by parsing the finalized assistant message.
async fn maybe_complete_plan_item_from_message(
    sess: &Session,
    turn_context: &TurnContext,
    state: &mut PlanModeStreamState,
    item: &ResponseItem,
) {
    if let ResponseItem::Message { role, content, .. } = item
        && role == "assistant"
    {
        let mut text = String::new();
        for entry in content {
            if let ContentItem::OutputText { text: chunk } = entry {
                text.push_str(chunk);
            }
        }
        if let Some(plan_text) = extract_proposed_plan_text(&text) {
            if !state.plan_item_state.started {
                state.plan_item_state.start(sess, turn_context).await;
            }
            state
                .plan_item_state
                .complete_with_text(sess, turn_context, plan_text)
                .await;
        }
    }
}

/// Emit a completed agent message in plan mode, respecting deferred starts.
async fn emit_agent_message_in_plan_mode(
    sess: &Session,
    turn_context: &TurnContext,
    agent_message: codex_protocol::items::AgentMessageItem,
    state: &mut PlanModeStreamState,
) {
    let agent_message_id = agent_message.id.clone();
    let text = agent_message_text(&agent_message);
    if text.trim().is_empty() {
        state.pending_agent_message_items.remove(&agent_message_id);
        state.started_agent_message_items.remove(&agent_message_id);
        return;
    }

    maybe_emit_pending_agent_message_start(sess, turn_context, state, &agent_message_id).await;

    if !state
        .started_agent_message_items
        .contains(&agent_message_id)
    {
        let start_item = state
            .pending_agent_message_items
            .remove(&agent_message_id)
            .unwrap_or_else(|| {
                TurnItem::AgentMessage(codex_protocol::items::AgentMessageItem {
                    id: agent_message_id.clone(),
                    content: Vec::new(),
                    phase: None,
                })
            });
        sess.emit_turn_item_started(turn_context, &start_item).await;
        state
            .started_agent_message_items
            .insert(agent_message_id.clone());
    }

    sess.emit_turn_item_completed(turn_context, TurnItem::AgentMessage(agent_message))
        .await;
    state.started_agent_message_items.remove(&agent_message_id);
}

/// Emit completion for a plan-mode turn item, handling agent messages specially.
async fn emit_turn_item_in_plan_mode(
    sess: &Session,
    turn_context: &TurnContext,
    turn_item: TurnItem,
    previously_active_item: Option<&TurnItem>,
    state: &mut PlanModeStreamState,
) {
    match turn_item {
        TurnItem::AgentMessage(agent_message) => {
            emit_agent_message_in_plan_mode(sess, turn_context, agent_message, state).await;
        }
        _ => {
            if previously_active_item.is_none() {
                sess.emit_turn_item_started(turn_context, &turn_item).await;
            }
            sess.emit_turn_item_completed(turn_context, turn_item).await;
        }
    }
}

/// Handle a completed assistant response item in plan mode, returning true if handled.
async fn handle_assistant_item_done_in_plan_mode(
    sess: &Session,
    turn_context: &TurnContext,
    item: &ResponseItem,
    state: &mut PlanModeStreamState,
    previously_active_item: Option<&TurnItem>,
    last_agent_message: &mut Option<String>,
) -> bool {
    if let ResponseItem::Message { role, .. } = item
        && role == "assistant"
    {
        maybe_complete_plan_item_from_message(sess, turn_context, state, item).await;

        if let Some(turn_item) = handle_non_tool_response_item(item, true).await {
            emit_turn_item_in_plan_mode(
                sess,
                turn_context,
                turn_item,
                previously_active_item,
                state,
            )
            .await;
        }

        sess.record_conversation_items(turn_context, std::slice::from_ref(item))
            .await;
        if let Some(agent_message) = last_assistant_message_from_item(item, true) {
            *last_agent_message = Some(agent_message);
        }
        return true;
    }
    false
}

async fn drain_in_flight(
    in_flight: &mut FuturesOrdered<BoxFuture<'static, CodexResult<ResponseInputItem>>>,
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
) -> CodexResult<()> {
    while let Some(res) = in_flight.next().await {
        match res {
            Ok(response_input) => {
                sess.record_conversation_items(&turn_context, &[response_input.into()])
                    .await;
            }
            Err(err) => {
                error_or_panic(format!("in-flight tool future failed during drain: {err}"));
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
#[instrument(level = "trace",
    skip_all,
    fields(
        turn_id = %turn_context.sub_id,
        model = %turn_context.model_info.slug
    )
)]
async fn try_run_sampling_request(
    router: Arc<ToolRouter>,
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    client_session: &mut ModelClientSession,
    turn_metadata_header: Option<&str>,
    turn_diff_tracker: SharedTurnDiffTracker,
    prompt: &Prompt,
    cancellation_token: CancellationToken,
) -> CodexResult<SamplingRequestResult> {
    let collaboration_mode = sess.current_collaboration_mode().await;
    let rollout_item =
        RolloutItem::TurnContext(turn_context.to_turn_context_item(collaboration_mode));

    feedback_tags!(
        model = turn_context.model_info.slug.clone(),
        approval_policy = turn_context.approval_policy,
        sandbox_policy = turn_context.sandbox_policy,
        effort = turn_context.reasoning_effort,
        auth_mode = sess.services.auth_manager.auth_mode(),
        features = sess.features.enabled_features(),
    );

    sess.persist_rollout_items(&[rollout_item]).await;
    let initial_response_timeout = initial_stream_response_timeout(&turn_context.provider);
    let mut stream = await_initial_response_phase(
        client_session
            .stream(
                prompt,
                &turn_context.model_info,
                &turn_context.otel_manager,
                turn_context.reasoning_effort,
                turn_context.reasoning_summary,
                turn_metadata_header,
            )
            .instrument(trace_span!("stream_request")),
        &sess,
        &turn_context,
        &cancellation_token,
        initial_response_timeout,
        "Still waiting for model response stream to start...",
        "timed out waiting for model response stream to start",
    )
    .await?
    .map_err(remap_initial_response_start_error)?;

    let tool_runtime = ToolCallRuntime::new(
        Arc::clone(&router),
        Arc::clone(&sess),
        Arc::clone(&turn_context),
        Arc::clone(&turn_diff_tracker),
    );
    let mut in_flight: FuturesOrdered<BoxFuture<'static, CodexResult<ResponseInputItem>>> =
        FuturesOrdered::new();
    let mut needs_follow_up = false;
    let mut last_agent_message: Option<String> = None;
    let mut active_item: Option<TurnItem> = None;
    let mut should_emit_turn_diff = false;
    let plan_mode = turn_context.collaboration_mode.mode == ModeKind::Plan;
    let mut plan_mode_state = plan_mode.then(|| PlanModeStreamState::new(&turn_context.sub_id));
    let receiving_span = trace_span!("receiving_stream");
    let mut saw_first_response_event = false;
    let outcome: CodexResult<SamplingRequestResult> = loop {
        let handle_responses = trace_span!(
            parent: &receiving_span,
            "handle_responses",
            otel.name = field::Empty,
            tool_name = field::Empty,
            from = field::Empty,
        );

        let event = if saw_first_response_event {
            match stream
                .next()
                .instrument(trace_span!(parent: &handle_responses, "receiving"))
                .or_cancel(&cancellation_token)
                .await
            {
                Ok(event) => event,
                Err(codex_async_utils::CancelErr::Cancelled) => break Err(CodexErr::TurnAborted),
            }
        } else {
            await_initial_response_phase(
                stream
                    .next()
                    .instrument(trace_span!(parent: &handle_responses, "receiving")),
                &sess,
                &turn_context,
                &cancellation_token,
                initial_response_timeout,
                "Still waiting for first model response event...",
                "timed out waiting for first model response event",
            )
            .await?
        };

        let event = match event {
            Some(Ok(event)) => event,
            Some(Err(err)) => {
                break Err(remap_initial_response_stream_error(
                    err,
                    saw_first_response_event,
                ));
            }
            None => {
                let message = if saw_first_response_event {
                    "stream closed before response.completed"
                } else {
                    "stream closed before first model response event"
                };
                let err = if saw_first_response_event {
                    CodexErr::Stream(message.into(), None)
                } else {
                    CodexErr::InitialResponseFailed(message.into())
                };
                break Err(err);
            }
        };

        sess.services
            .otel_manager
            .record_responses(&handle_responses, &event);

        if counts_as_first_model_response_event(&event) {
            saw_first_response_event = true;
        }

        match event {
            ResponseEvent::Created => {}
            ResponseEvent::OutputItemDone(item) => {
                let previously_active_item = active_item.take();
                if let Some(state) = plan_mode_state.as_mut() {
                    if let Some(previous) = previously_active_item.as_ref() {
                        let item_id = previous.id();
                        if matches!(previous, TurnItem::AgentMessage(_)) {
                            flush_proposed_plan_segments_for_item(
                                &sess,
                                &turn_context,
                                state,
                                &item_id,
                            )
                            .await;
                        }
                    }
                    if handle_assistant_item_done_in_plan_mode(
                        &sess,
                        &turn_context,
                        &item,
                        state,
                        previously_active_item.as_ref(),
                        &mut last_agent_message,
                    )
                    .await
                    {
                        continue;
                    }
                }

                let mut ctx = HandleOutputCtx {
                    sess: sess.clone(),
                    turn_context: turn_context.clone(),
                };

                let output_result = handle_output_item_done(&mut ctx, item, previously_active_item)
                    .instrument(handle_responses)
                    .await?;
                if let Some(tool_call) = output_result.tool_call {
                    let is_wait_barrier = tool_call.tool_name == "wait";
                    if is_wait_barrier {
                        drain_in_flight(&mut in_flight, sess.clone(), turn_context.clone()).await?;
                    }

                    let tool_future: BoxFuture<'static, CodexResult<ResponseInputItem>> = Box::pin(
                        tool_runtime
                            .clone()
                            .handle_tool_call(tool_call, cancellation_token.child_token()),
                    );
                    in_flight.push_back(tool_future);

                    if is_wait_barrier {
                        drain_in_flight(&mut in_flight, sess.clone(), turn_context.clone()).await?;
                    }
                }
                if let Some(agent_message) = output_result.last_agent_message {
                    last_agent_message = Some(agent_message);
                }
                needs_follow_up |= output_result.needs_follow_up;
            }
            ResponseEvent::OutputItemAdded(item) => {
                if let Some(turn_item) = handle_non_tool_response_item(&item, plan_mode).await {
                    if let Some(state) = plan_mode_state.as_mut()
                        && matches!(turn_item, TurnItem::AgentMessage(_))
                    {
                        let item_id = turn_item.id();
                        state
                            .pending_agent_message_items
                            .insert(item_id, turn_item.clone());
                    } else {
                        sess.emit_turn_item_started(&turn_context, &turn_item).await;
                    }
                    active_item = Some(turn_item);
                }
            }
            ResponseEvent::ServerReasoningIncluded(included) => {
                sess.set_server_reasoning_included(included).await;
            }
            ResponseEvent::RateLimits(snapshot) => {
                // Update internal state with latest rate limits, but defer sending until
                // token usage is available to avoid duplicate TokenCount events.
                sess.update_rate_limits(&turn_context, snapshot).await;
            }
            ResponseEvent::ModelsEtag(etag) => {
                // Update internal state with latest models etag
                let config = sess.get_config().await;
                sess.services
                    .models_manager
                    .refresh_if_new_etag(etag, &config)
                    .await;
            }
            ResponseEvent::Completed {
                response_id: _,
                token_usage,
                can_append: _,
            } => {
                if let Some(state) = plan_mode_state.as_mut() {
                    flush_proposed_plan_segments_all(&sess, &turn_context, state).await;
                }
                sess.update_token_usage_info(&turn_context, token_usage.as_ref())
                    .await;
                should_emit_turn_diff = true;

                needs_follow_up |= sess.has_pending_input().await;

                break Ok(SamplingRequestResult {
                    needs_follow_up,
                    last_agent_message,
                });
            }
            ResponseEvent::OutputTextDelta(delta) => {
                // In review child threads, suppress assistant text deltas; the
                // UI will show a selection popup from the final ReviewOutput.
                if let Some(active) = active_item.as_ref() {
                    let item_id = active.id();
                    if let Some(state) = plan_mode_state.as_mut()
                        && matches!(active, TurnItem::AgentMessage(_))
                    {
                        let segments = state
                            .plan_parsers
                            .assistant_parser_mut(&item_id)
                            .parse(&delta);
                        handle_plan_segments(&sess, &turn_context, state, &item_id, segments).await;
                    } else {
                        let event = AgentMessageContentDeltaEvent {
                            thread_id: sess.conversation_id.to_string(),
                            turn_id: turn_context.sub_id.clone(),
                            item_id,
                            delta,
                        };
                        sess.send_event(&turn_context, EventMsg::AgentMessageContentDelta(event))
                            .await;
                    }
                } else {
                    error_or_panic("OutputTextDelta without active item".to_string());
                }
            }
            ResponseEvent::ReasoningSummaryDelta {
                delta,
                summary_index,
            } => {
                if let Some(active) = active_item.as_ref() {
                    let event = ReasoningContentDeltaEvent {
                        thread_id: sess.conversation_id.to_string(),
                        turn_id: turn_context.sub_id.clone(),
                        item_id: active.id(),
                        delta,
                        summary_index,
                    };
                    sess.send_event(&turn_context, EventMsg::ReasoningContentDelta(event))
                        .await;
                } else {
                    error_or_panic("ReasoningSummaryDelta without active item".to_string());
                }
            }
            ResponseEvent::ReasoningSummaryPartAdded { summary_index } => {
                if let Some(active) = active_item.as_ref() {
                    let event =
                        EventMsg::AgentReasoningSectionBreak(AgentReasoningSectionBreakEvent {
                            item_id: active.id(),
                            summary_index,
                        });
                    sess.send_event(&turn_context, event).await;
                } else {
                    error_or_panic("ReasoningSummaryPartAdded without active item".to_string());
                }
            }
            ResponseEvent::ReasoningContentDelta {
                delta,
                content_index,
            } => {
                if let Some(active) = active_item.as_ref() {
                    let event = ReasoningRawContentDeltaEvent {
                        thread_id: sess.conversation_id.to_string(),
                        turn_id: turn_context.sub_id.clone(),
                        item_id: active.id(),
                        delta,
                        content_index,
                    };
                    sess.send_event(&turn_context, EventMsg::ReasoningRawContentDelta(event))
                        .await;
                } else {
                    error_or_panic("ReasoningRawContentDelta without active item".to_string());
                }
            }
        }
    };

    drain_in_flight(&mut in_flight, sess.clone(), turn_context.clone()).await?;

    if should_emit_turn_diff {
        let unified_diff = {
            let mut tracker = turn_diff_tracker.lock().await;
            tracker.get_unified_diff()
        };
        if let Ok(Some(unified_diff)) = unified_diff {
            let msg = EventMsg::TurnDiff(TurnDiffEvent { unified_diff });
            sess.clone().send_event(&turn_context, msg).await;
        }
    }

    outcome
}

const INITIAL_STREAM_RESPONSE_TIMEOUT_CAP: Duration = Duration::from_secs(80);
const INITIAL_RESPONSE_WAIT_NOTICE_DELAY: Duration = Duration::from_secs(15);
const INITIAL_STREAM_START_TIMEOUT_MESSAGE: &str =
    "timed out waiting for model response stream to start";

fn initial_stream_response_timeout(provider: &ModelProviderInfo) -> Duration {
    provider
        .stream_idle_timeout()
        .min(INITIAL_STREAM_RESPONSE_TIMEOUT_CAP)
}

fn initial_response_notice_delay(timeout: Duration) -> Option<Duration> {
    let notice_delay = INITIAL_RESPONSE_WAIT_NOTICE_DELAY.min(timeout);
    (notice_delay < timeout).then_some(notice_delay)
}

fn remap_initial_response_start_error(err: CodexErr) -> CodexErr {
    remap_initial_response_error(err, INITIAL_STREAM_START_TIMEOUT_MESSAGE)
}

fn is_retryable_initial_stream_start_failure(err: &CodexErr) -> bool {
    match err {
        CodexErr::InitialResponseFailed(message) => {
            message == INITIAL_STREAM_START_TIMEOUT_MESSAGE
                || is_initial_response_body_decode_eof(message)
        }
        _ => false,
    }
}

fn is_initial_response_body_decode_eof(message: &str) -> bool {
    message.contains("Transport error: network error")
        && message.contains("error decoding response body")
        && message.contains("unexpected EOF during chunk size line")
}

fn initial_stream_start_retry_delay(attempt: u64) -> Duration {
    let retry_index = attempt.saturating_sub(2) as usize;
    INITIAL_STREAM_START_RETRY_DELAYS
        .get(retry_index)
        .copied()
        .unwrap_or_else(|| {
            *INITIAL_STREAM_START_RETRY_DELAYS
                .last()
                .expect("stream-start retry delays are configured")
        })
}

fn remap_initial_response_stream_error(err: CodexErr, saw_first_response_event: bool) -> CodexErr {
    if saw_first_response_event {
        return err;
    }

    remap_initial_response_error(err, "timed out waiting for first model response event")
}

fn remap_initial_response_error(err: CodexErr, timeout_message: &'static str) -> CodexErr {
    match err {
        CodexErr::Stream(message, _) if message.contains("idle timeout waiting for SSE") => {
            CodexErr::InitialResponseFailed(timeout_message.into())
        }
        CodexErr::Stream(message, _) => CodexErr::InitialResponseFailed(message),
        CodexErr::Timeout => CodexErr::InitialResponseFailed(timeout_message.into()),
        other => other,
    }
}

fn counts_as_first_model_response_event(event: &ResponseEvent) -> bool {
    !matches!(
        event,
        ResponseEvent::RateLimits(_)
            | ResponseEvent::ModelsEtag(_)
            | ResponseEvent::ServerReasoningIncluded(_)
    )
}

fn duration_ms_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn initial_response_wait_stage(waiting_message: &str) -> &'static str {
    match waiting_message {
        "Still waiting for model response stream to start..." => "stream_start_wait",
        "Still waiting for first model response event..." => "first_response_event_wait",
        _ => "initial_response_wait",
    }
}

fn initial_response_ready_stage(waiting_message: &str) -> &'static str {
    match waiting_message {
        "Still waiting for model response stream to start..." => "stream_start_ready",
        "Still waiting for first model response event..." => "first_response_event_ready",
        _ => "initial_response_ready",
    }
}

fn initial_response_timeout_stage(waiting_message: &str) -> &'static str {
    match waiting_message {
        "Still waiting for model response stream to start..." => "stream_start_timeout",
        "Still waiting for first model response event..." => "first_response_event_timeout",
        _ => "initial_response_timeout",
    }
}

fn initial_response_ready_message(waiting_message: &str) -> &'static str {
    match waiting_message {
        "Still waiting for model response stream to start..." => "Model response stream started.",
        "Still waiting for first model response event..." => "First model response event received.",
        _ => "Initial model response phase completed.",
    }
}

fn initial_response_timeout_notice(timeout_message: &str) -> String {
    let suffix = timeout_message
        .strip_prefix("timed out waiting for ")
        .unwrap_or(timeout_message);
    format!("Timed out waiting for {suffix}.")
}

async fn await_initial_response_phase<F, T>(
    future: F,
    sess: &Session,
    turn_context: &TurnContext,
    cancellation_token: &CancellationToken,
    timeout: Duration,
    waiting_message: &'static str,
    timeout_message: &'static str,
) -> CodexResult<T>
where
    F: futures::Future<Output = T>,
{
    let notice_delay = initial_response_notice_delay(timeout);
    let wait_started_at = std::time::Instant::now();
    let timeout_ms = duration_ms_u64(timeout);
    tokio::pin!(future);
    let deadline = sleep(timeout);
    tokio::pin!(deadline);
    let notice = notice_delay.map(sleep);
    tokio::pin!(notice);
    let mut notice_sent = false;

    loop {
        tokio::select! {
            _ = cancellation_token.cancelled() => return Err(CodexErr::TurnAborted),
            _ = &mut deadline => {
                sess.send_event(
                    turn_context,
                    EventMsg::BackgroundEvent(BackgroundEventEvent {
                        message: initial_response_timeout_notice(timeout_message),
                        stage: Some(initial_response_timeout_stage(waiting_message).to_string()),
                        elapsed_ms: Some(duration_ms_u64(wait_started_at.elapsed())),
                        timeout_ms: Some(timeout_ms),
                        outcome: Some("timeout".to_string()),
                    }),
                ).await;
                return Err(CodexErr::InitialResponseFailed(timeout_message.into()));
            }
            _ = async {
                match notice.as_mut().as_pin_mut() {
                    Some(notice) => notice.await,
                    None => futures::future::pending::<()>().await,
                }
            }, if !notice_sent => {
                notice_sent = true;
                sess.send_event(
                    turn_context,
                    EventMsg::BackgroundEvent(BackgroundEventEvent {
                        message: waiting_message.to_string(),
                        stage: Some(initial_response_wait_stage(waiting_message).to_string()),
                        elapsed_ms: Some(duration_ms_u64(wait_started_at.elapsed())),
                        timeout_ms: Some(timeout_ms),
                        outcome: Some("waiting".to_string()),
                    }),
                ).await;
            }
            output = &mut future => {
                if notice_sent {
                    sess.send_event(
                        turn_context,
                        EventMsg::BackgroundEvent(BackgroundEventEvent {
                            message: initial_response_ready_message(waiting_message).to_string(),
                            stage: Some(initial_response_ready_stage(waiting_message).to_string()),
                            elapsed_ms: Some(duration_ms_u64(wait_started_at.elapsed())),
                            timeout_ms: Some(timeout_ms),
                            outcome: Some("ready".to_string()),
                        }),
                    ).await;
                }
                return Ok(output)
            },
        }
    }
}

pub(super) fn get_last_assistant_message_from_turn(responses: &[ResponseItem]) -> Option<String> {
    responses.iter().rev().find_map(|item| {
        if let ResponseItem::Message { role, content, .. } = item {
            if role == "assistant" {
                content.iter().rev().find_map(|ci| {
                    if let ContentItem::OutputText { text } = ci {
                        Some(text.clone())
                    } else {
                        None
                    }
                })
            } else {
                None
            }
        } else {
            None
        }
    })
}

use crate::memories::prompts::build_memory_tool_developer_instructions;
#[cfg(test)]
pub(crate) use tests::make_session_and_context;
#[cfg(test)]
pub(crate) use tests::make_session_and_context_with_rx;
#[cfg(test)]
pub(crate) use tests::make_session_configuration_for_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CodexAuth;
    use crate::ThreadManager;
    use crate::built_in_model_providers;
    use crate::config::ConfigBuilder;
    use crate::config::test_config;
    use crate::config_loader::ConfigLayerStack;
    use crate::config_loader::ConfigLayerStackOrdering;
    use crate::config_loader::NetworkConstraints;
    use crate::config_loader::RequirementSource;
    use crate::config_loader::Sourced;
    use crate::exec::ExecToolCallOutput;
    use crate::function_tool::FunctionCallError;
    use crate::mcp_connection_manager::ToolInfo;
    use crate::models_manager::model_info;
    use crate::shell::default_user_shell;
    use crate::tools::format_exec_output_str;

    use codex_protocol::ThreadId;
    use codex_protocol::models::FunctionCallOutputBody;
    use codex_protocol::models::FunctionCallOutputPayload;

    use crate::protocol::CompactedItem;
    use crate::protocol::CreditsSnapshot;
    use crate::protocol::InitialHistory;
    use crate::protocol::RateLimitSnapshot;
    use crate::protocol::RateLimitWindow;
    use crate::protocol::ResumedHistory;
    use crate::protocol::SpawnedAgentType;
    use crate::protocol::TokenCountEvent;
    use crate::protocol::TokenUsage;
    use crate::protocol::TokenUsageInfo;
    use crate::state::TaskKind;
    use crate::tasks::SessionTask;
    use crate::tasks::SessionTaskContext;
    use crate::tools::ToolRouter;
    use crate::tools::context::ToolInvocation;
    use crate::tools::context::ToolOutput;
    use crate::tools::context::ToolPayload;
    use crate::tools::handlers::ShellHandler;
    use crate::tools::handlers::UnifiedExecHandler;
    use crate::tools::handlers::collab_inbox;
    use crate::tools::registry::ToolHandler;
    use serial_test::serial;
    use std::env;
    use std::ffi::OsString;

    #[test]
    fn initial_stream_start_retry_delay_uses_longer_ladder_with_cap() {
        let delays: Vec<Duration> = (2..=10).map(initial_stream_start_retry_delay).collect();

        pretty_assertions::assert_eq!(
            delays,
            vec![
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(8),
                Duration::from_secs(15),
                Duration::from_secs(30),
                Duration::from_secs(45),
                Duration::from_secs(45),
                Duration::from_secs(45),
            ]
        );
    }
    use crate::tools::router::ToolCallSource;
    use crate::turn_diff_tracker::TurnDiffTracker;
    use codex_app_server_protocol::AppInfo;
    use codex_otel::TelemetryAuthMode;
    use codex_protocol::models::BaseInstructions;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::ResponseInputItem;
    use codex_protocol::models::ResponseItem;
    use codex_protocol::openai_models::ModelsResponse;
    use std::path::Path;
    use std::time::Duration;
    use tokio::time::sleep;

    use codex_protocol::mcp::CallToolResult as McpCallToolResult;
    use pretty_assertions::assert_eq;
    use rmcp::model::JsonObject;
    use rmcp::model::Tool;
    use serde::Deserialize;
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration as StdDuration;

    #[test]
    fn initial_stream_response_timeout_caps_default_provider_idle_timeout_at_eighty_seconds() {
        let provider = ModelProviderInfo::create_openai_provider();

        assert_eq!(
            initial_stream_response_timeout(&provider),
            Duration::from_secs(80)
        );
    }

    #[test]
    fn initial_stream_response_timeout_caps_long_explicit_provider_idle_timeout_at_eighty_seconds()
    {
        let mut provider = ModelProviderInfo::create_kimi_moonshot_provider();
        provider.stream_idle_timeout_ms = Some(300_000);

        assert_eq!(
            initial_stream_response_timeout(&provider),
            Duration::from_secs(80)
        );
    }

    #[test]
    fn initial_stream_response_timeout_honors_shorter_explicit_provider_idle_timeout() {
        let mut provider = ModelProviderInfo::create_kimi_moonshot_provider();
        provider.stream_idle_timeout_ms = Some(2_000);

        assert_eq!(
            initial_stream_response_timeout(&provider),
            Duration::from_secs(2)
        );
    }

    struct InstructionsTestCase {
        slug: &'static str,
        expects_apply_patch_instructions: bool,
    }

    fn user_message(text: &str) -> ResponseItem {
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: text.to_string(),
            }],
            end_turn: None,
            phase: None,
        }
    }

    fn assistant_message(text: &str) -> ResponseItem {
        ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: text.to_string(),
            }],
            end_turn: None,
            phase: None,
        }
    }

    fn skill_message(text: &str) -> ResponseItem {
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: text.to_string(),
            }],
            end_turn: None,
            phase: None,
        }
    }

    fn make_connector(id: &str, name: &str) -> AppInfo {
        AppInfo {
            id: id.to_string(),
            name: name.to_string(),
            description: None,
            logo_url: None,
            logo_url_dark: None,
            distribution_channel: None,
            install_url: None,
            is_accessible: true,
            is_enabled: true,
        }
    }

    fn make_mcp_tool(
        server_name: &str,
        tool_name: &str,
        connector_id: Option<&str>,
        connector_name: Option<&str>,
    ) -> ToolInfo {
        ToolInfo {
            server_name: server_name.to_string(),
            tool_name: tool_name.to_string(),
            tool: Tool {
                name: tool_name.to_string().into(),
                title: None,
                description: Some(format!("Test tool: {tool_name}").into()),
                input_schema: Arc::new(JsonObject::default()),
                output_schema: None,
                annotations: None,
                execution: None,
                icons: None,
                meta: None,
            },
            connector_id: connector_id.map(str::to_string),
            connector_name: connector_name.map(str::to_string),
        }
    }

    #[tokio::test]
    async fn build_initial_context_swarm_includes_blackboard_prompt_rules_and_path() {
        let (session, turn_context) = make_session_and_context().await;
        session
            .services
            .agent_control
            .register_agent_name(session.conversation_id, crate::agent::PRIMARY_AGENT_NAME)
            .expect("register primary");

        let expected_blackboard_path = {
            let mut state = session.state.lock().await;
            state.session_configuration.collaboration_mode.mode = ModeKind::Swarm;
            state
                .shared_blackboard_path()
                .expect("shared blackboard path set")
        };

        let initial_context = session.build_initial_context(&turn_context).await;
        let developer_text = initial_context
            .iter()
            .filter_map(|item| {
                let ResponseItem::Message { role, content, .. } = item else {
                    return None;
                };
                if role != "developer" {
                    return None;
                }
                let text = content
                    .iter()
                    .filter_map(|chunk| match chunk {
                        ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                            Some(text.as_str())
                        }
                        ContentItem::InputImage { .. } => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Some(text)
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(developer_text.contains("<swarm_shared_blackboard>"));
        assert!(developer_text.contains(&expected_blackboard_path.display().to_string()));
        assert!(developer_text.contains(crate::agent::PRIMARY_AGENT_NAME));
        assert!(developer_text.contains("[agent_name]：message_content"));
        assert!(developer_text.contains(&format!(
            "[{}]：your message",
            crate::agent::PRIMARY_AGENT_NAME
        )));
        assert!(!developer_text.contains("CODEX_AGENT_NAME"));
    }

    #[tokio::test]
    async fn build_initial_context_swarm_subagent_blackboard_uses_name_hint() {
        let (session, mut turn_context) = make_session_and_context().await;
        let session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: ThreadId::new(),
            depth: 1,
            agent_type: None,
            agent_name_hint: Some("Dakota-worker".to_string()),
        });

        {
            let mut state = session.state.lock().await;
            state.session_configuration.collaboration_mode.mode = ModeKind::Swarm;
            state.session_configuration.session_source = session_source.clone();
        }
        turn_context.collaboration_mode.mode = ModeKind::Swarm;
        turn_context.session_source = session_source;

        let initial_context = session.build_initial_context(&turn_context).await;
        let developer_text = initial_context
            .iter()
            .filter_map(|item| {
                let ResponseItem::Message { role, content, .. } = item else {
                    return None;
                };
                if role != "developer" {
                    return None;
                }
                let text = content
                    .iter()
                    .filter_map(|chunk| match chunk {
                        ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                            Some(text.as_str())
                        }
                        ContentItem::InputImage { .. } => None,
                    })
                    .collect::<Vec<_>>()
                    .join(
                        "
",
                    );
                Some(text)
            })
            .collect::<Vec<_>>()
            .join(
                "
",
            );

        assert!(developer_text.contains("<swarm_shared_blackboard>"));
        assert!(developer_text.contains("Dakota-worker"));
        assert!(!developer_text.contains(crate::agent::UNNAMED_AGENT_NAME));
    }

    #[tokio::test]
    async fn build_initial_context_swarm_subagent_uses_swarm_sub_prompt() {
        let (session, mut turn_context) = make_session_and_context().await;
        let session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: ThreadId::new(),
            depth: 1,
            agent_type: None,
            agent_name_hint: None,
        });

        {
            let mut state = session.state.lock().await;
            state.session_configuration.collaboration_mode.mode = ModeKind::Swarm;
            state.session_configuration.session_source = session_source.clone();
            state
                .session_configuration
                .collaboration_mode
                .settings
                .developer_instructions = Some("SHOULD_NOT_APPEAR".to_string());
        }
        turn_context.collaboration_mode.mode = ModeKind::Swarm;
        turn_context
            .collaboration_mode
            .settings
            .developer_instructions = Some("SHOULD_NOT_APPEAR".to_string());
        turn_context.session_source = session_source;

        let initial_context = session.build_initial_context(&turn_context).await;
        let developer_text = initial_context
            .iter()
            .filter_map(|item| {
                let ResponseItem::Message { role, content, .. } = item else {
                    return None;
                };
                if role != "developer" {
                    return None;
                }
                let text = content
                    .iter()
                    .filter_map(|chunk| match chunk {
                        ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                            Some(text.as_str())
                        }
                        ContentItem::InputImage { .. } => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Some(text)
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(developer_text.contains("Focused Execution Mandate"));
        assert!(!developer_text.contains("The Parallelism Mandate"));
        assert!(!developer_text.contains("SHOULD_NOT_APPEAR"));
    }

    #[tokio::test]
    async fn build_initial_context_swarm_complex_subagent_uses_swarm_sub_complex_prompt() {
        let (session, mut turn_context) = make_session_and_context().await;
        let session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: ThreadId::new(),
            depth: 1,
            agent_type: None,
            agent_name_hint: None,
        });

        {
            let mut state = session.state.lock().await;
            state.session_configuration.collaboration_mode.mode = ModeKind::Swarm;
            state.session_configuration.session_source = session_source.clone();
            state
                .session_configuration
                .collaboration_mode
                .settings
                .developer_instructions =
                Some(crate::swarm::complex_root_swarm_developer_instructions().to_string());
        }
        turn_context.collaboration_mode.mode = ModeKind::Swarm;
        turn_context
            .collaboration_mode
            .settings
            .developer_instructions =
            Some(crate::swarm::complex_root_swarm_developer_instructions().to_string());
        turn_context.session_source = session_source;

        let initial_context = session.build_initial_context(&turn_context).await;
        let developer_text = initial_context
            .iter()
            .filter_map(|item| {
                let ResponseItem::Message { role, content, .. } = item else {
                    return None;
                };
                if role != "developer" {
                    return None;
                }
                let text = content
                    .iter()
                    .filter_map(|chunk| match chunk {
                        ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                            Some(text.as_str())
                        }
                        ContentItem::InputImage { .. } => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Some(text)
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(developer_text.contains("# Collaboration Mode: Swarm"));
        assert!(developer_text.contains("Focused Execution Mandate"));
        assert!(!developer_text.contains("The Parallelism Mandate"));
    }

    #[tokio::test]
    async fn build_initial_context_swarm_complex_worker_subagent_uses_typed_worker_prompt() {
        let (session, mut turn_context) = make_session_and_context().await;
        let session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: ThreadId::new(),
            depth: 1,
            agent_type: Some(SpawnedAgentType::Worker),
            agent_name_hint: None,
        });

        {
            let mut state = session.state.lock().await;
            state.session_configuration.collaboration_mode.mode = ModeKind::Swarm;
            state.session_configuration.session_source = session_source.clone();
            state
                .session_configuration
                .collaboration_mode
                .settings
                .developer_instructions =
                Some(crate::swarm::complex_root_swarm_developer_instructions().to_string());
        }
        turn_context.collaboration_mode.mode = ModeKind::Swarm;
        turn_context
            .collaboration_mode
            .settings
            .developer_instructions =
            Some(crate::swarm::complex_root_swarm_developer_instructions().to_string());
        turn_context.session_source = session_source;

        let initial_context = session.build_initial_context(&turn_context).await;
        let developer_text = initial_context
            .iter()
            .filter_map(|item| {
                let ResponseItem::Message { role, content, .. } = item else {
                    return None;
                };
                if role != "developer" {
                    return None;
                }
                let text = content
                    .iter()
                    .filter_map(|chunk| match chunk {
                        ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                            Some(text.as_str())
                        }
                        ContentItem::InputImage { .. } => None,
                    })
                    .collect::<Vec<_>>()
                    .join(
                        "
",
                    );
                Some(text)
            })
            .collect::<Vec<_>>()
            .join(
                "
",
            );

        assert!(developer_text.contains("# Collaboration Mode: Swarm Complex"));
        assert!(developer_text.contains("Role: Focused Execution Worker in Swarm Complex"));
        assert!(!developer_text.contains("Verification Status Vocabulary"));
    }

    #[tokio::test]
    async fn build_initial_context_swarm_complex_verifier_subagent_uses_typed_verifier_prompt() {
        let (session, mut turn_context) = make_session_and_context().await;
        let session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: ThreadId::new(),
            depth: 1,
            agent_type: Some(SpawnedAgentType::Verifier),
            agent_name_hint: None,
        });

        {
            let mut state = session.state.lock().await;
            state.session_configuration.collaboration_mode.mode = ModeKind::Swarm;
            state.session_configuration.session_source = session_source.clone();
            state
                .session_configuration
                .collaboration_mode
                .settings
                .developer_instructions =
                Some(crate::swarm::complex_root_swarm_developer_instructions().to_string());
        }
        turn_context.collaboration_mode.mode = ModeKind::Swarm;
        turn_context
            .collaboration_mode
            .settings
            .developer_instructions =
            Some(crate::swarm::complex_root_swarm_developer_instructions().to_string());
        turn_context.session_source = session_source;

        let initial_context = session.build_initial_context(&turn_context).await;
        let developer_text = initial_context
            .iter()
            .filter_map(|item| {
                let ResponseItem::Message { role, content, .. } = item else {
                    return None;
                };
                if role != "developer" {
                    return None;
                }
                let text = content
                    .iter()
                    .filter_map(|chunk| match chunk {
                        ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                            Some(text.as_str())
                        }
                        ContentItem::InputImage { .. } => None,
                    })
                    .collect::<Vec<_>>()
                    .join(
                        "
",
                    );
                Some(text)
            })
            .collect::<Vec<_>>()
            .join(
                "
",
            );

        assert!(developer_text.contains("# Collaboration Mode: Swarm Complex"));
        assert!(developer_text.contains("Verification Status Vocabulary"));
        assert!(developer_text.contains("Role: Verification Agent in Swarm Complex"));
        assert!(!developer_text.contains("The Parallelism Mandate"));
    }

    #[tokio::test]
    async fn build_initial_context_swarm_uses_primary_agent_name_without_lazy_registration() {
        let (session, turn_context) = make_session_and_context().await;
        {
            let mut state = session.state.lock().await;
            state.session_configuration.collaboration_mode.mode = ModeKind::Swarm;
        }

        let initial_context = session.build_initial_context(&turn_context).await;
        let developer_text = initial_context
            .iter()
            .filter_map(|item| {
                let ResponseItem::Message { role, content, .. } = item else {
                    return None;
                };
                if role != "developer" {
                    return None;
                }
                let text = content
                    .iter()
                    .filter_map(|chunk| match chunk {
                        ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                            Some(text.as_str())
                        }
                        ContentItem::InputImage { .. } => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Some(text)
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(developer_text.contains(&format!(
            "- Your agent name: {}",
            crate::agent::PRIMARY_AGENT_NAME
        )));
        assert!(!developer_text.contains(&session.conversation_id.to_string()));
        assert!(!developer_text.contains("CODEX_AGENT_NAME"));
    }

    #[tokio::test]
    async fn build_initial_context_includes_agent_durable_prompts_as_developer_message() {
        let (session, turn_context) = make_session_and_context().await;
        let store = crate::agent::context::AgentContextStore::new(
            turn_context.config.codex_home.clone(),
            crate::agent::PRIMARY_AGENT_NAME,
            crate::agent::PRIMARY_AGENT_NAME,
        );
        let paths = store.ensure_layout().await.expect("agent context layout");
        tokio::fs::write(
            &paths.manual_system_prompt_file,
            "manual identity prompt from human\n",
        )
        .await
        .expect("write manual prompt");
        tokio::fs::write(
            &paths.automatic_prompt_file,
            "stable owner boundary from automatic memory\n",
        )
        .await
        .expect("write automatic prompt");

        let initial_context = session.build_initial_context(&turn_context).await;
        let messages = initial_context
            .iter()
            .filter_map(|item| {
                let ResponseItem::Message { role, content, .. } = item else {
                    return None;
                };
                let text = content
                    .iter()
                    .filter_map(|chunk| match chunk {
                        ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                            Some(text.as_str())
                        }
                        ContentItem::InputImage { .. } => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Some((role.as_str(), text))
            })
            .collect::<Vec<_>>();
        let durable_index = messages
            .iter()
            .position(|(role, text)| {
                *role == "developer" && text.contains("<agent_durable_context")
            })
            .expect("durable context developer message");
        let durable_text = &messages[durable_index].1;

        assert!(durable_text.contains("<manual_permanent_system_prompt"));
        assert!(durable_text.contains("manual identity prompt from human"));
        assert!(durable_text.contains("<automatic_updated_prompt"));
        assert!(durable_text.contains("stable owner boundary from automatic memory"));
        assert!(!durable_text.contains("<runtime_tail_injection"));

        let first_user_index = messages
            .iter()
            .position(|(role, _)| *role == "user")
            .expect("first user context message");
        assert!(
            durable_index < first_user_index,
            "durable agent prompt must stay in developer context before user/environment context"
        );
    }

    #[tokio::test]
    async fn build_initial_context_subagent_durable_prompt_uses_name_hint_and_not_runtime_tail() {
        let (session, mut turn_context) = make_session_and_context().await;
        turn_context.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: ThreadId::new(),
            depth: 1,
            agent_type: Some(codex_protocol::protocol::SpawnedAgentType::Worker),
            agent_name_hint: Some("Dakota-worker".to_string()),
        });
        {
            let mut state = session.state.lock().await;
            state.session_configuration.session_source = turn_context.session_source.clone();
        }
        let store = crate::agent::context::AgentContextStore::new(
            turn_context.config.codex_home.clone(),
            "Dakota-worker",
            "Dakota-worker",
        );
        let paths = store.ensure_layout().await.expect("agent context layout");
        tokio::fs::write(&paths.manual_system_prompt_file, "subagent manual prompt\n")
            .await
            .expect("write manual prompt");

        let initial_context = session.build_initial_context(&turn_context).await;
        let durable_text = initial_context
            .iter()
            .filter_map(|item| {
                let ResponseItem::Message { role, content, .. } = item else {
                    return None;
                };
                if role != "developer" {
                    return None;
                }
                let text = content
                    .iter()
                    .filter_map(|chunk| match chunk {
                        ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                            Some(text.as_str())
                        }
                        ContentItem::InputImage { .. } => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                text.contains("<agent_durable_context").then_some(text)
            })
            .next()
            .expect("durable context message");

        assert!(durable_text.contains("display_name=\"Dakota-worker\""));
        assert!(durable_text.contains("subagent manual prompt"));
        assert!(!durable_text.contains(crate::agent::UNNAMED_AGENT_NAME));
        assert!(!durable_text.contains("Below are the agents collaborating with you."));
    }

    #[tokio::test]
    async fn append_swarm_latest_pair_and_blackboard_messages_includes_summary_and_snapshot() {
        let (session, mut turn_context) = make_session_and_context().await;
        turn_context.collaboration_mode.mode = ModeKind::Swarm;

        session
            .services
            .agent_control
            .register_agent_name(session.conversation_id, crate::agent::PRIMARY_AGENT_NAME)
            .expect("register primary");
        let peer_thread = ThreadId::new();
        session
            .services
            .agent_control
            .register_agent_name(peer_thread, "mikel-worker")
            .expect("register peer");
        session
            .services
            .agent_control
            .record_agent_work_summary(peer_thread, "prepared parser patch".to_string());

        let blackboard_path = {
            let state = session.state.lock().await;
            state
                .shared_blackboard_path()
                .expect("shared blackboard path set")
        };
        blackboard::append_blackboard_entry(
            &blackboard_path,
            crate::agent::PRIMARY_AGENT_NAME,
            "stabilize parser in one pass",
        )
        .await
        .expect("append blackboard");

        let mut input = vec![
            user_message("please fix parser"),
            assistant_message("working"),
        ];
        append_swarm_latest_pair_and_blackboard_messages(&session, &turn_context, &mut input).await;

        assert_eq!(input.len(), 4);
        let ResponseItem::Message {
            role: injected_user_role,
            content: injected_user_content,
            ..
        } = &input[2]
        else {
            panic!("expected injected user message");
        };
        assert_eq!(injected_user_role, "user");
        assert!(matches!(
            injected_user_content.as_slice(),
            [ContentItem::InputText { text }]
                if text.contains("Below are the agents collaborating with you.")
                    && text.contains("Current time:")
                    && text.contains("mikel-worker")
                    && text.contains("prepared parser patch [")
                    && text.contains("Below is the shared blackboard content:")
                    && text.contains("stabilize parser in one pass")
                    && !text.contains("Shared blackboard file:")
        ));

        let ResponseItem::Message {
            role: injected_assistant_role,
            content: injected_assistant_content,
            ..
        } = &input[3]
        else {
            panic!("expected injected assistant message");
        };
        assert_eq!(injected_assistant_role, "assistant");
        assert_eq!(
            injected_assistant_content,
            &vec![ContentItem::OutputText {
                text: SWARM_COLLAB_CONTEXT_ASSISTANT_ACK.to_string(),
            }]
        );
    }

    #[test]
    fn format_collab_summary_entry_appends_minute_second_after_summary() {
        let recorded_at = chrono::DateTime::parse_from_rfc3339("2026-03-30T06:07:45Z")
            .expect("parse timestamp")
            .timestamp();

        assert_eq!(
            format_collab_summary_entry("prepared parser patch", recorded_at),
            "prepared parser patch [07:45]"
        );
    }

    #[tokio::test]
    async fn append_swarm_latest_pair_and_blackboard_messages_skips_for_primary_without_spawned_agents()
     {
        let (session, mut turn_context) = make_session_and_context().await;
        turn_context.collaboration_mode.mode = ModeKind::Swarm;

        let mut input = vec![
            user_message("please fix parser"),
            assistant_message("working"),
        ];
        append_swarm_latest_pair_and_blackboard_messages(&session, &turn_context, &mut input).await;

        assert_eq!(input.len(), 2);
    }

    #[test]
    fn debug_history_latest_files_are_parseable_when_available() {
        let debug_root = Path::new("/media/wmj/BC0739C74EA78EEA/debug");
        if !debug_root.exists() {
            return;
        }

        let mut parsed_count = 0usize;
        let entries = std::fs::read_dir(debug_root).expect("read debug root");
        for entry in entries.flatten() {
            let history_path = entry.path().join("history.latest.json");
            if !history_path.is_file() {
                continue;
            }

            let content = std::fs::read_to_string(&history_path)
                .unwrap_or_else(|err| panic!("read {}: {err}", history_path.display()));
            let parsed: serde_json::Value = serde_json::from_str(&content)
                .unwrap_or_else(|err| panic!("parse {}: {err}", history_path.display()));
            assert!(parsed.get("schema_version").is_some());
            assert!(parsed.get("entries").is_some());
            parsed_count += 1;
            if parsed_count >= 5 {
                break;
            }
        }

        assert!(
            parsed_count > 0,
            "expected at least one history.latest.json under {}",
            debug_root.display()
        );
    }

    #[test]
    fn append_user_input_complexity_guidance_appends_to_user_messages_only() {
        let mut input = vec![
            ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "hello".to_string(),
                }],
                end_turn: None,
                phase: None,
            },
            ResponseItem::Message {
                id: None,
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText {
                    text: "ok".to_string(),
                }],
                end_turn: None,
                phase: None,
            },
        ];

        append_user_input_complexity_guidance(&mut input);

        let ResponseItem::Message { content, .. } = &input[0] else {
            panic!("expected user message");
        };
        assert!(content.iter().any(|content_item| {
            matches!(
                content_item,
                ContentItem::InputText { text } if text.contains("hello") && text.contains(USER_INPUT_COMPLEXITY_GUIDANCE)
            )
        }));
        let ResponseItem::Message {
            content: assistant_content,
            ..
        } = &input[1]
        else {
            panic!("expected assistant message");
        };
        assert_eq!(
            assistant_content,
            &vec![ContentItem::OutputText {
                text: "ok".to_string(),
            }]
        );
    }

    #[test]
    fn append_user_input_complexity_guidance_does_not_duplicate_when_present() {
        let mut input = vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![
                ContentItem::InputText {
                    text: "hello".to_string(),
                },
                ContentItem::InputText {
                    text: USER_INPUT_COMPLEXITY_GUIDANCE.to_string(),
                },
            ],
            end_turn: None,
            phase: None,
        }];

        append_user_input_complexity_guidance(&mut input);

        let ResponseItem::Message { content, .. } = &input[0] else {
            panic!("expected user message");
        };
        let count = content
            .iter()
            .filter(|content_item| {
                matches!(
                    content_item,
                    ContentItem::InputText { text } if text.contains(USER_INPUT_COMPLEXITY_GUIDANCE)
                )
            })
            .count();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn get_base_instructions_no_user_content() {
        let prompt_with_apply_patch_instructions =
            include_str!("../prompt_with_apply_patch_instructions.md");
        let models_response: ModelsResponse =
            serde_json::from_str(include_str!("../models.json")).expect("valid models.json");
        let model_info_for_slug = |slug: &str, config: &Config| {
            let model = models_response
                .models
                .iter()
                .find(|candidate| candidate.slug == slug)
                .cloned()
                .unwrap_or_else(|| panic!("model slug {slug} is missing from models.json"));
            model_info::with_config_overrides(model, config)
        };
        let test_cases = vec![
            InstructionsTestCase {
                slug: "gpt-5",
                expects_apply_patch_instructions: false,
            },
            InstructionsTestCase {
                slug: "gpt-5.1",
                expects_apply_patch_instructions: false,
            },
            InstructionsTestCase {
                slug: "gpt-5.1-codex",
                expects_apply_patch_instructions: false,
            },
            InstructionsTestCase {
                slug: "gpt-5.1-codex-max",
                expects_apply_patch_instructions: false,
            },
        ];

        let (session, _turn_context) = make_session_and_context().await;
        let config = test_config();

        for test_case in test_cases {
            let model_info = model_info_for_slug(test_case.slug, &config);
            if test_case.expects_apply_patch_instructions {
                assert_eq!(
                    model_info.base_instructions.as_str(),
                    prompt_with_apply_patch_instructions
                );
            }

            {
                let mut state = session.state.lock().await;
                state.session_configuration.base_instructions =
                    model_info.base_instructions.clone();
            }

            let base_instructions = session.get_base_instructions().await;
            assert_eq!(base_instructions.text, model_info.base_instructions);
        }
    }

    #[tokio::test]
    async fn reload_user_config_layer_updates_effective_apps_config() {
        let (session, _turn_context) = make_session_and_context().await;
        let codex_home = session.codex_home().await;
        std::fs::create_dir_all(&codex_home).expect("create codex home");
        let config_toml_path = codex_home.join(CONFIG_TOML_FILE);
        std::fs::write(
            &config_toml_path,
            "[apps.calendar]\nenabled = false\ndisabled_reason = \"user\"\n",
        )
        .expect("write user config");

        session.reload_user_config_layer().await;

        let config = session.get_config().await;
        let apps_toml = config
            .config_layer_stack
            .effective_config()
            .as_table()
            .and_then(|table| table.get("apps"))
            .cloned()
            .expect("apps table");
        let apps = crate::config::types::AppsConfigToml::deserialize(apps_toml)
            .expect("deserialize apps config");
        let app = apps
            .apps
            .get("calendar")
            .expect("calendar app config exists");

        assert!(!app.enabled);
        assert_eq!(
            app.disabled_reason,
            Some(crate::config::types::AppDisabledReason::User)
        );
    }

    #[test]
    fn filter_connectors_for_input_skips_duplicate_slug_mentions() {
        let connectors = vec![
            make_connector("one", "Foo Bar"),
            make_connector("two", "Foo-Bar"),
        ];
        let input = vec![user_message("use $foo-bar")];
        let explicitly_enabled_connectors = HashSet::new();
        let skill_name_counts_lower = HashMap::new();

        let selected = filter_connectors_for_input(
            &connectors,
            &input,
            &explicitly_enabled_connectors,
            &skill_name_counts_lower,
        );

        assert_eq!(selected, Vec::new());
    }

    #[test]
    fn filter_connectors_for_input_skips_when_skill_name_conflicts() {
        let connectors = vec![make_connector("one", "Todoist")];
        let input = vec![user_message("use $todoist")];
        let explicitly_enabled_connectors = HashSet::new();
        let skill_name_counts_lower = HashMap::from([("todoist".to_string(), 1)]);

        let selected = filter_connectors_for_input(
            &connectors,
            &input,
            &explicitly_enabled_connectors,
            &skill_name_counts_lower,
        );

        assert_eq!(selected, Vec::new());
    }

    #[test]
    fn filter_connectors_for_input_skips_disabled_connectors() {
        let mut connector = make_connector("calendar", "Calendar");
        connector.is_enabled = false;
        let input = vec![user_message("use $calendar")];
        let explicitly_enabled_connectors = HashSet::new();
        let selected = filter_connectors_for_input(
            &[connector],
            &input,
            &explicitly_enabled_connectors,
            &HashMap::new(),
        );

        assert_eq!(selected, Vec::new());
    }

    #[test]
    fn collect_explicit_app_ids_from_skill_items_includes_linked_mentions() {
        let connectors = vec![make_connector("calendar", "Calendar")];
        let skill_items = vec![skill_message(
            "<skill>\n<name>demo</name>\n<path>/tmp/skills/demo/SKILL.md</path>\nuse [$calendar](app://calendar)\n</skill>",
        )];

        let connector_ids =
            collect_explicit_app_ids_from_skill_items(&skill_items, &connectors, &HashMap::new());

        assert_eq!(connector_ids, HashSet::from(["calendar".to_string()]));
    }

    #[test]
    fn collect_explicit_app_ids_from_skill_items_resolves_unambiguous_plain_mentions() {
        let connectors = vec![make_connector("calendar", "Calendar")];
        let skill_items = vec![skill_message(
            "<skill>\n<name>demo</name>\n<path>/tmp/skills/demo/SKILL.md</path>\nuse $calendar\n</skill>",
        )];

        let connector_ids =
            collect_explicit_app_ids_from_skill_items(&skill_items, &connectors, &HashMap::new());

        assert_eq!(connector_ids, HashSet::from(["calendar".to_string()]));
    }

    #[test]
    fn collect_explicit_app_ids_from_skill_items_skips_plain_mentions_with_skill_conflicts() {
        let connectors = vec![make_connector("calendar", "Calendar")];
        let skill_items = vec![skill_message(
            "<skill>\n<name>demo</name>\n<path>/tmp/skills/demo/SKILL.md</path>\nuse $calendar\n</skill>",
        )];
        let skill_name_counts_lower = HashMap::from([("calendar".to_string(), 1)]);

        let connector_ids = collect_explicit_app_ids_from_skill_items(
            &skill_items,
            &connectors,
            &skill_name_counts_lower,
        );

        assert_eq!(connector_ids, HashSet::<String>::new());
    }

    #[test]
    fn search_tool_selection_keeps_codex_apps_tools_without_mentions() {
        let selected_tool_names = vec![
            "mcp__codex_apps__calendar_create_event".to_string(),
            "mcp__rmcp__echo".to_string(),
        ];
        let mcp_tools = HashMap::from([
            (
                "mcp__codex_apps__calendar_create_event".to_string(),
                make_mcp_tool(
                    CODEX_APPS_MCP_SERVER_NAME,
                    "calendar_create_event",
                    Some("calendar"),
                    Some("Calendar"),
                ),
            ),
            (
                "mcp__rmcp__echo".to_string(),
                make_mcp_tool("rmcp", "echo", None, None),
            ),
        ]);

        let mut selected_mcp_tools = filter_mcp_tools_by_name(&mcp_tools, &selected_tool_names);
        let connectors = connectors::accessible_connectors_from_mcp_tools(&mcp_tools);
        let explicitly_enabled_connectors = HashSet::new();
        let connectors = filter_connectors_for_input(
            &connectors,
            &[user_message("run the selected tools")],
            &explicitly_enabled_connectors,
            &HashMap::new(),
        );
        let apps_mcp_tools = filter_codex_apps_mcp_tools_only(&mcp_tools, &connectors);
        selected_mcp_tools.extend(apps_mcp_tools);

        let mut tool_names: Vec<String> = selected_mcp_tools.into_keys().collect();
        tool_names.sort();
        assert_eq!(
            tool_names,
            vec![
                "mcp__codex_apps__calendar_create_event".to_string(),
                "mcp__rmcp__echo".to_string(),
            ]
        );
    }

    #[test]
    fn apps_mentions_add_codex_apps_tools_to_search_selected_set() {
        let selected_tool_names = vec!["mcp__rmcp__echo".to_string()];
        let mcp_tools = HashMap::from([
            (
                "mcp__codex_apps__calendar_create_event".to_string(),
                make_mcp_tool(
                    CODEX_APPS_MCP_SERVER_NAME,
                    "calendar_create_event",
                    Some("calendar"),
                    Some("Calendar"),
                ),
            ),
            (
                "mcp__rmcp__echo".to_string(),
                make_mcp_tool("rmcp", "echo", None, None),
            ),
        ]);

        let mut selected_mcp_tools = filter_mcp_tools_by_name(&mcp_tools, &selected_tool_names);
        let connectors = connectors::accessible_connectors_from_mcp_tools(&mcp_tools);
        let explicitly_enabled_connectors = HashSet::new();
        let connectors = filter_connectors_for_input(
            &connectors,
            &[user_message("use $calendar and then echo the response")],
            &explicitly_enabled_connectors,
            &HashMap::new(),
        );
        let apps_mcp_tools = filter_codex_apps_mcp_tools_only(&mcp_tools, &connectors);
        selected_mcp_tools.extend(apps_mcp_tools);

        let mut tool_names: Vec<String> = selected_mcp_tools.into_keys().collect();
        tool_names.sort();
        assert_eq!(
            tool_names,
            vec![
                "mcp__codex_apps__calendar_create_event".to_string(),
                "mcp__rmcp__echo".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn reconstruct_history_matches_live_compactions() {
        let (session, turn_context) = make_session_and_context().await;
        let (rollout_items, expected) = sample_rollout(&session, &turn_context).await;

        let reconstruction_turn = session.new_default_turn().await;
        let reconstructed = session
            .reconstruct_history_from_rollout(reconstruction_turn.as_ref(), &rollout_items)
            .await;

        assert_eq!(expected, reconstructed);
    }

    #[tokio::test]
    async fn reconstruct_history_uses_replacement_history_verbatim() {
        let (session, turn_context) = make_session_and_context().await;
        let summary_item = ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "summary".to_string(),
            }],
            end_turn: None,
            phase: None,
        };
        let replacement_history = vec![
            summary_item.clone(),
            ResponseItem::Message {
                id: None,
                role: "developer".to_string(),
                content: vec![ContentItem::InputText {
                    text: "stale developer instructions".to_string(),
                }],
                end_turn: None,
                phase: None,
            },
        ];
        let rollout_items = vec![RolloutItem::Compacted(CompactedItem {
            message: String::new(),
            replacement_history: Some(replacement_history.clone()),
        })];

        let reconstructed = session
            .reconstruct_history_from_rollout(&turn_context, &rollout_items)
            .await;

        assert_eq!(reconstructed, replacement_history);
    }

    #[tokio::test]
    async fn record_initial_history_reconstructs_resumed_transcript() {
        let (session, turn_context) = make_session_and_context().await;
        let (rollout_items, expected) = sample_rollout(&session, &turn_context).await;

        session
            .record_initial_history(InitialHistory::Resumed(ResumedHistory {
                conversation_id: ThreadId::default(),
                history: rollout_items,
                rollout_path: PathBuf::from("/tmp/resume.jsonl"),
            }))
            .await;

        let history = session.state.lock().await.clone_history();
        assert_eq!(expected, history.raw_items());
    }

    #[tokio::test]
    async fn record_initial_history_resumed_hydrates_previous_model() {
        let (session, turn_context) = make_session_and_context().await;
        let previous_model = "previous-rollout-model";
        let rollout_items = vec![RolloutItem::TurnContext(TurnContextItem {
            turn_id: Some(turn_context.sub_id.clone()),
            cwd: turn_context.cwd.clone(),
            approval_policy: turn_context.approval_policy,
            sandbox_policy: turn_context.sandbox_policy.clone(),
            network: None,
            model: previous_model.to_string(),
            personality: turn_context.personality,
            collaboration_mode: Some(turn_context.collaboration_mode.clone()),
            effort: turn_context.reasoning_effort,
            summary: turn_context.reasoning_summary,
            user_instructions: None,
            developer_instructions: None,
            final_output_json_schema: None,
            truncation_policy: Some(turn_context.truncation_policy.into()),
        })];

        session
            .record_initial_history(InitialHistory::Resumed(ResumedHistory {
                conversation_id: ThreadId::default(),
                history: rollout_items,
                rollout_path: PathBuf::from("/tmp/resume.jsonl"),
            }))
            .await;

        assert_eq!(
            session.previous_model().await,
            Some(previous_model.to_string())
        );
    }

    #[tokio::test]
    async fn resumed_history_seeds_initial_context_on_first_turn_only() {
        let (session, turn_context) = make_session_and_context().await;
        let (rollout_items, mut expected) = sample_rollout(&session, &turn_context).await;

        session
            .record_initial_history(InitialHistory::Resumed(ResumedHistory {
                conversation_id: ThreadId::default(),
                history: rollout_items,
                rollout_path: PathBuf::from("/tmp/resume.jsonl"),
            }))
            .await;

        let history_before_seed = session.state.lock().await.clone_history();
        assert_eq!(expected, history_before_seed.raw_items());

        session.seed_initial_context_if_needed(&turn_context).await;
        expected.extend(session.build_initial_context(&turn_context).await);
        let history_after_seed = session.clone_history().await;
        assert_eq!(expected, history_after_seed.raw_items());

        session.seed_initial_context_if_needed(&turn_context).await;
        let history_after_second_seed = session.clone_history().await;
        assert_eq!(expected, history_after_second_seed.raw_items());
    }

    #[tokio::test]
    async fn record_initial_history_seeds_token_info_from_rollout() {
        let (session, turn_context) = make_session_and_context().await;
        let (mut rollout_items, _expected) = sample_rollout(&session, &turn_context).await;

        let info1 = TokenUsageInfo {
            total_token_usage: TokenUsage {
                input_tokens: 10,
                cached_input_tokens: 0,
                output_tokens: 20,
                reasoning_output_tokens: 0,
                total_tokens: 30,
            },
            last_token_usage: TokenUsage {
                input_tokens: 3,
                cached_input_tokens: 0,
                output_tokens: 4,
                reasoning_output_tokens: 0,
                total_tokens: 7,
            },
            model_context_window: Some(1_000),
        };
        let info2 = TokenUsageInfo {
            total_token_usage: TokenUsage {
                input_tokens: 100,
                cached_input_tokens: 50,
                output_tokens: 200,
                reasoning_output_tokens: 25,
                total_tokens: 375,
            },
            last_token_usage: TokenUsage {
                input_tokens: 10,
                cached_input_tokens: 0,
                output_tokens: 20,
                reasoning_output_tokens: 5,
                total_tokens: 35,
            },
            model_context_window: Some(2_000),
        };

        rollout_items.push(RolloutItem::EventMsg(EventMsg::TokenCount(
            TokenCountEvent {
                info: Some(info1),
                rate_limits: None,
            },
        )));
        rollout_items.push(RolloutItem::EventMsg(EventMsg::TokenCount(
            TokenCountEvent {
                info: None,
                rate_limits: None,
            },
        )));
        rollout_items.push(RolloutItem::EventMsg(EventMsg::TokenCount(
            TokenCountEvent {
                info: Some(info2.clone()),
                rate_limits: None,
            },
        )));
        rollout_items.push(RolloutItem::EventMsg(EventMsg::TokenCount(
            TokenCountEvent {
                info: None,
                rate_limits: None,
            },
        )));

        session
            .record_initial_history(InitialHistory::Resumed(ResumedHistory {
                conversation_id: ThreadId::default(),
                history: rollout_items,
                rollout_path: PathBuf::from("/tmp/resume.jsonl"),
            }))
            .await;

        let actual = session.state.lock().await.token_info();
        assert_eq!(actual, Some(info2));
    }

    #[tokio::test]
    async fn recompute_token_usage_uses_session_base_instructions() {
        let (session, turn_context) = make_session_and_context().await;

        let override_instructions = "SESSION_OVERRIDE_INSTRUCTIONS_ONLY".repeat(120);
        {
            let mut state = session.state.lock().await;
            state.session_configuration.base_instructions = override_instructions.clone();
        }

        let item = user_message("hello");
        session
            .record_into_history(std::slice::from_ref(&item), &turn_context)
            .await;

        let history = session.clone_history().await;
        let session_base_instructions = BaseInstructions {
            text: override_instructions,
        };
        let expected_tokens = history
            .estimate_token_count_with_base_instructions(&session_base_instructions)
            .expect("estimate with session base instructions");
        let model_estimated_tokens = history
            .estimate_token_count(&turn_context)
            .expect("estimate with model instructions");
        assert_ne!(expected_tokens, model_estimated_tokens);

        session.recompute_token_usage(&turn_context).await;

        let actual_tokens = session
            .state
            .lock()
            .await
            .token_info()
            .expect("token info")
            .last_token_usage
            .total_tokens;
        assert_eq!(actual_tokens, expected_tokens.max(0));
    }

    #[tokio::test]
    async fn recompute_token_usage_updates_model_context_window() {
        let (session, mut turn_context) = make_session_and_context().await;

        {
            let mut state = session.state.lock().await;
            state.set_token_info(Some(TokenUsageInfo {
                total_token_usage: TokenUsage::default(),
                last_token_usage: TokenUsage::default(),
                model_context_window: Some(258_400),
            }));
        }

        turn_context.model_info.context_window = Some(128_000);
        turn_context.model_info.effective_context_window_percent = 100;

        session.recompute_token_usage(&turn_context).await;

        let actual = session.state.lock().await.token_info().expect("token info");
        assert_eq!(actual.model_context_window, Some(128_000));
    }

    #[tokio::test]
    async fn record_initial_history_reconstructs_forked_transcript() {
        let (session, turn_context) = make_session_and_context().await;
        let (rollout_items, mut expected) = sample_rollout(&session, &turn_context).await;

        session
            .record_initial_history(InitialHistory::Forked(rollout_items))
            .await;

        let reconstruction_turn = session.new_default_turn().await;
        expected.extend(
            session
                .build_initial_context(reconstruction_turn.as_ref())
                .await,
        );
        let history = session.state.lock().await.clone_history();
        assert_eq!(expected, history.raw_items());
    }

    #[tokio::test]
    async fn record_initial_history_forked_hydrates_previous_model() {
        let (session, turn_context) = make_session_and_context().await;
        let previous_model = "forked-rollout-model";
        let rollout_items = vec![RolloutItem::TurnContext(TurnContextItem {
            turn_id: Some(turn_context.sub_id.clone()),
            cwd: turn_context.cwd.clone(),
            approval_policy: turn_context.approval_policy,
            sandbox_policy: turn_context.sandbox_policy.clone(),
            network: None,
            model: previous_model.to_string(),
            personality: turn_context.personality,
            collaboration_mode: Some(turn_context.collaboration_mode.clone()),
            effort: turn_context.reasoning_effort,
            summary: turn_context.reasoning_summary,
            user_instructions: None,
            developer_instructions: None,
            final_output_json_schema: None,
            truncation_policy: Some(turn_context.truncation_policy.into()),
        })];

        session
            .record_initial_history(InitialHistory::Forked(rollout_items))
            .await;

        assert_eq!(
            session.previous_model().await,
            Some(previous_model.to_string())
        );
    }

    #[tokio::test]
    async fn thread_rollback_drops_last_turn_from_history() {
        let (sess, tc, rx) = make_session_and_context_with_rx().await;

        let initial_context = sess.build_initial_context(tc.as_ref()).await;
        sess.record_into_history(&initial_context, tc.as_ref())
            .await;

        let turn_1 = vec![
            ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "turn 1 user".to_string(),
                }],
                end_turn: None,
                phase: None,
            },
            ResponseItem::Message {
                id: None,
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText {
                    text: "turn 1 assistant".to_string(),
                }],
                end_turn: None,
                phase: None,
            },
        ];
        sess.record_into_history(&turn_1, tc.as_ref()).await;

        let turn_2 = vec![
            ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "turn 2 user".to_string(),
                }],
                end_turn: None,
                phase: None,
            },
            ResponseItem::Message {
                id: None,
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText {
                    text: "turn 2 assistant".to_string(),
                }],
                end_turn: None,
                phase: None,
            },
        ];
        sess.record_into_history(&turn_2, tc.as_ref()).await;

        handlers::thread_rollback(&sess, "sub-1".to_string(), 1).await;

        let rollback_event = wait_for_thread_rolled_back(&rx).await;
        assert_eq!(rollback_event.num_turns, 1);

        let mut expected = Vec::new();
        expected.extend(initial_context);
        expected.extend(turn_1);

        let history = sess.clone_history().await;
        assert_eq!(expected, history.raw_items());
        assert_eq!(
            sess.previous_model().await,
            Some(tc.model_info.slug.clone())
        );
    }

    #[tokio::test]
    async fn thread_rollback_clears_history_when_num_turns_exceeds_existing_turns() {
        let (sess, tc, rx) = make_session_and_context_with_rx().await;

        let initial_context = sess.build_initial_context(tc.as_ref()).await;
        sess.record_into_history(&initial_context, tc.as_ref())
            .await;

        let turn_1 = vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "turn 1 user".to_string(),
            }],
            end_turn: None,
            phase: None,
        }];
        sess.record_into_history(&turn_1, tc.as_ref()).await;

        handlers::thread_rollback(&sess, "sub-1".to_string(), 99).await;

        let rollback_event = wait_for_thread_rolled_back(&rx).await;
        assert_eq!(rollback_event.num_turns, 99);

        let history = sess.clone_history().await;
        assert_eq!(initial_context, history.raw_items());
    }

    #[tokio::test]
    async fn thread_rollback_fails_when_turn_in_progress() {
        let (sess, tc, rx) = make_session_and_context_with_rx().await;

        let initial_context = sess.build_initial_context(tc.as_ref()).await;
        sess.record_into_history(&initial_context, tc.as_ref())
            .await;

        *sess.active_turn.lock().await = Some(crate::state::ActiveTurn::default());
        handlers::thread_rollback(&sess, "sub-1".to_string(), 1).await;

        let error_event = wait_for_thread_rollback_failed(&rx).await;
        assert_eq!(
            error_event.codex_error_info,
            Some(CodexErrorInfo::ThreadRollbackFailed)
        );

        let history = sess.clone_history().await;
        assert_eq!(initial_context, history.raw_items());
    }

    #[tokio::test]
    async fn thread_rollback_fails_when_num_turns_is_zero() {
        let (sess, tc, rx) = make_session_and_context_with_rx().await;

        let initial_context = sess.build_initial_context(tc.as_ref()).await;
        sess.record_into_history(&initial_context, tc.as_ref())
            .await;

        handlers::thread_rollback(&sess, "sub-1".to_string(), 0).await;

        let error_event = wait_for_thread_rollback_failed(&rx).await;
        assert_eq!(error_event.message, "num_turns must be >= 1");
        assert_eq!(
            error_event.codex_error_info,
            Some(CodexErrorInfo::ThreadRollbackFailed)
        );

        let history = sess.clone_history().await;
        assert_eq!(initial_context, history.raw_items());
    }

    #[tokio::test]
    async fn set_rate_limits_retains_previous_credits() {
        let codex_home = tempfile::tempdir().expect("create temp dir");
        let config = build_test_config(codex_home.path()).await;
        let config = Arc::new(config);
        let model = ModelsManager::get_model_offline_for_tests(config.model.as_deref());
        let model_info =
            ModelsManager::construct_model_info_offline_for_tests(model.as_str(), &config);
        let reasoning_effort = config.model_reasoning_effort;
        let collaboration_mode = CollaborationMode {
            mode: ModeKind::Default,
            settings: Settings {
                model,
                reasoning_effort,
                developer_instructions: None,
            },
        };
        let session_configuration = SessionConfiguration {
            provider: config.model_provider.clone(),
            collaboration_mode,
            model_reasoning_summary: config.model_reasoning_summary,
            developer_instructions: config.developer_instructions.clone(),
            user_instructions: config.user_instructions.clone(),
            personality: config.personality,
            base_instructions: config
                .base_instructions
                .clone()
                .unwrap_or_else(|| model_info.get_model_instructions(config.personality)),
            compact_prompt: config.compact_prompt.clone(),
            approval_policy: config.permissions.approval_policy.clone(),
            sandbox_policy: config.permissions.sandbox_policy.clone(),
            windows_sandbox_level: WindowsSandboxLevel::from_config(&config),
            cwd: config.cwd.clone(),
            codex_home: config.codex_home.clone(),
            thread_name: None,
            original_config_do_not_use: Arc::clone(&config),
            session_source: SessionSource::Exec,
            dynamic_tools: Vec::new(),
            persist_extended_history: false,
        };

        let mut state = SessionState::new(session_configuration);
        let initial = RateLimitSnapshot {
            limit_id: None,
            limit_name: None,
            primary: Some(RateLimitWindow {
                used_percent: 10.0,
                window_minutes: Some(15),
                resets_at: Some(1_700),
            }),
            secondary: None,
            credits: Some(CreditsSnapshot {
                has_credits: true,
                unlimited: false,
                balance: Some("10.00".to_string()),
            }),
            plan_type: Some(codex_protocol::account::PlanType::Plus),
        };
        state.set_rate_limits(initial.clone());

        let update = RateLimitSnapshot {
            limit_id: Some("codex_other".to_string()),
            limit_name: Some("codex_other".to_string()),
            primary: Some(RateLimitWindow {
                used_percent: 40.0,
                window_minutes: Some(30),
                resets_at: Some(1_800),
            }),
            secondary: Some(RateLimitWindow {
                used_percent: 5.0,
                window_minutes: Some(60),
                resets_at: Some(1_900),
            }),
            credits: None,
            plan_type: None,
        };
        state.set_rate_limits(update.clone());

        assert_eq!(
            state.latest_rate_limits,
            Some(RateLimitSnapshot {
                limit_id: Some("codex_other".to_string()),
                limit_name: Some("codex_other".to_string()),
                primary: update.primary.clone(),
                secondary: update.secondary,
                credits: initial.credits,
                plan_type: initial.plan_type,
            })
        );
    }

    #[tokio::test]
    async fn set_rate_limits_updates_plan_type_when_present() {
        let codex_home = tempfile::tempdir().expect("create temp dir");
        let config = build_test_config(codex_home.path()).await;
        let config = Arc::new(config);
        let model = ModelsManager::get_model_offline_for_tests(config.model.as_deref());
        let model_info =
            ModelsManager::construct_model_info_offline_for_tests(model.as_str(), &config);
        let reasoning_effort = config.model_reasoning_effort;
        let collaboration_mode = CollaborationMode {
            mode: ModeKind::Default,
            settings: Settings {
                model,
                reasoning_effort,
                developer_instructions: None,
            },
        };
        let session_configuration = SessionConfiguration {
            provider: config.model_provider.clone(),
            collaboration_mode,
            model_reasoning_summary: config.model_reasoning_summary,
            developer_instructions: config.developer_instructions.clone(),
            user_instructions: config.user_instructions.clone(),
            personality: config.personality,
            base_instructions: config
                .base_instructions
                .clone()
                .unwrap_or_else(|| model_info.get_model_instructions(config.personality)),
            compact_prompt: config.compact_prompt.clone(),
            approval_policy: config.permissions.approval_policy.clone(),
            sandbox_policy: config.permissions.sandbox_policy.clone(),
            windows_sandbox_level: WindowsSandboxLevel::from_config(&config),
            cwd: config.cwd.clone(),
            codex_home: config.codex_home.clone(),
            thread_name: None,
            original_config_do_not_use: Arc::clone(&config),
            session_source: SessionSource::Exec,
            dynamic_tools: Vec::new(),
            persist_extended_history: false,
        };

        let mut state = SessionState::new(session_configuration);
        let initial = RateLimitSnapshot {
            limit_id: None,
            limit_name: None,
            primary: Some(RateLimitWindow {
                used_percent: 15.0,
                window_minutes: Some(20),
                resets_at: Some(1_600),
            }),
            secondary: Some(RateLimitWindow {
                used_percent: 5.0,
                window_minutes: Some(45),
                resets_at: Some(1_650),
            }),
            credits: Some(CreditsSnapshot {
                has_credits: true,
                unlimited: false,
                balance: Some("15.00".to_string()),
            }),
            plan_type: Some(codex_protocol::account::PlanType::Plus),
        };
        state.set_rate_limits(initial.clone());

        let update = RateLimitSnapshot {
            limit_id: None,
            limit_name: None,
            primary: Some(RateLimitWindow {
                used_percent: 35.0,
                window_minutes: Some(25),
                resets_at: Some(1_700),
            }),
            secondary: None,
            credits: None,
            plan_type: Some(codex_protocol::account::PlanType::Pro),
        };
        state.set_rate_limits(update.clone());

        assert_eq!(
            state.latest_rate_limits,
            Some(RateLimitSnapshot {
                limit_id: Some("codex".to_string()),
                limit_name: None,
                primary: update.primary,
                secondary: update.secondary,
                credits: initial.credits,
                plan_type: update.plan_type,
            })
        );
    }

    #[test]
    fn prefers_structured_content_when_present() {
        let ctr = McpCallToolResult {
            // Content present but should be ignored because structured_content is set.
            content: vec![text_block("ignored")],
            is_error: None,
            structured_content: Some(json!({
                "ok": true,
                "value": 42
            })),
            meta: None,
        };

        let got = FunctionCallOutputPayload::from(&ctr);
        let expected = FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text(
                serde_json::to_string(&json!({
                    "ok": true,
                    "value": 42
                }))
                .unwrap(),
            ),
            success: Some(true),
        };

        assert_eq!(expected, got);
    }

    #[tokio::test]
    async fn includes_timed_out_message() {
        let exec = ExecToolCallOutput {
            exit_code: 0,
            stdout: StreamOutput::new(String::new()),
            stderr: StreamOutput::new(String::new()),
            aggregated_output: StreamOutput::new("Command output".to_string()),
            duration: StdDuration::from_secs(1),
            timed_out: true,
        };
        let (_, turn_context) = make_session_and_context().await;

        let out = format_exec_output_str(&exec, turn_context.truncation_policy);

        assert_eq!(
            out,
            "command timed out after 1000 milliseconds\nCommand output"
        );
    }

    #[tokio::test]
    async fn spawn_uses_experimental_mode_for_initial_collaboration_mode() {
        let codex_home = tempfile::tempdir().expect("create temp dir");
        let mut config = build_test_config(codex_home.path()).await;
        config.experimental_mode = Some(ModeKind::Swarm);
        let auth_manager = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("dummy"));
        let models_manager = Arc::new(ModelsManager::new(
            config.codex_home.clone(),
            auth_manager.clone(),
        ));
        let skills_manager = Arc::new(SkillsManager::new(config.codex_home.clone()));
        let file_watcher = Arc::new(FileWatcher::noop());
        let spawned = Codex::spawn(
            config,
            auth_manager,
            models_manager.clone(),
            skills_manager,
            file_watcher,
            InitialHistory::New,
            SessionSource::Exec,
            AgentControl::default(),
            Vec::new(),
            false,
        )
        .await
        .expect("spawn should succeed");

        let collaboration_mode = spawned.codex.session.collaboration_mode().await;
        assert_eq!(collaboration_mode.mode, ModeKind::Swarm);
        let expected_instructions = models_manager
            .list_collaboration_modes()
            .into_iter()
            .find(|preset| preset.mode == Some(ModeKind::Swarm) && preset.name == "Swarm")
            .and_then(|preset| preset.developer_instructions.flatten())
            .expect("swarm preset should include developer instructions");
        assert_eq!(
            collaboration_mode.settings.developer_instructions,
            Some(expected_instructions)
        );

        let _ = spawned.codex.submit(Op::Shutdown).await;
    }

    #[tokio::test]
    async fn spawn_subagent_uses_swarm_sub_prompt_for_initial_collaboration_mode() {
        let codex_home = tempfile::tempdir().expect("create temp dir");
        let mut config = build_test_config(codex_home.path()).await;
        config.experimental_mode = Some(ModeKind::Swarm);
        let auth_manager = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("dummy"));
        let models_manager = Arc::new(ModelsManager::new(
            config.codex_home.clone(),
            auth_manager.clone(),
        ));
        let skills_manager = Arc::new(SkillsManager::new(config.codex_home.clone()));
        let file_watcher = Arc::new(FileWatcher::noop());
        let session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: ThreadId::new(),
            depth: 1,
            agent_type: None,
            agent_name_hint: None,
        });
        let spawned = Codex::spawn(
            config,
            auth_manager,
            models_manager,
            skills_manager,
            file_watcher,
            InitialHistory::New,
            session_source.clone(),
            AgentControl::default(),
            Vec::new(),
            false,
        )
        .await
        .expect("spawn should succeed");

        let collaboration_mode = spawned.codex.session.collaboration_mode().await;
        assert_eq!(collaboration_mode.mode, ModeKind::Swarm);
        assert_eq!(
            collaboration_mode.settings.developer_instructions,
            Some(resolve_swarm_prompt(&session_source, false).to_string())
        );

        let _ = spawned.codex.submit(Op::Shutdown).await;
    }

    #[tokio::test]
    async fn swarm_override_syncs_existing_spawned_agents() {
        let codex_home = tempfile::tempdir().expect("create temp dir");
        let config = build_test_config(codex_home.path()).await;
        let manager = ThreadManager::with_models_provider_and_home_for_tests(
            CodexAuth::from_api_key("dummy"),
            built_in_model_providers()["openai"].clone(),
            config.codex_home.clone(),
        );

        let parent = manager
            .start_thread(config.clone())
            .await
            .expect("start parent");
        let child_id = manager
            .agent_control()
            .spawn_agent(
                config,
                Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id: parent.thread_id,
                    depth: 1,
                    agent_type: None,
                    agent_name_hint: None,
                })),
            )
            .await
            .expect("spawn child");

        let collaboration_mode = CollaborationMode {
            mode: ModeKind::Swarm,
            settings: Settings {
                model: parent.session_configured.model.clone(),
                reasoning_effort: None,
                developer_instructions: Some("swarm instructions".to_string()),
            },
        };
        let _ = parent
            .thread
            .submit(Op::OverrideTurnContext {
                cwd: None,
                approval_policy: None,
                sandbox_policy: None,
                windows_sandbox_level: None,
                model: None,
                effort: None,
                summary: None,
                collaboration_mode: Some(collaboration_mode.clone()),
                personality: None,
            })
            .await
            .expect("override should submit");

        let expected_child_update = (
            child_id,
            Op::OverrideTurnContext {
                cwd: None,
                approval_policy: None,
                sandbox_policy: None,
                windows_sandbox_level: None,
                model: None,
                effort: None,
                summary: None,
                collaboration_mode: Some(collaboration_mode),
                personality: None,
            },
        );
        let mut found = false;
        for _ in 0..25 {
            if manager.captured_ops().contains(&expected_child_update) {
                found = true;
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
        assert!(found);

        let _ = manager
            .agent_control()
            .shutdown_agent(child_id)
            .await
            .expect("shutdown child");
        let _ = parent.thread.submit(Op::Shutdown).await;
    }

    #[test]
    fn sync_swarm_collaboration_mode_only_triggers_when_entering_swarm() {
        let base = CollaborationMode {
            mode: ModeKind::Default,
            settings: Settings {
                model: "gpt-5".to_string(),
                reasoning_effort: None,
                developer_instructions: None,
            },
        };
        let swarm = CollaborationMode {
            mode: ModeKind::Swarm,
            settings: Settings {
                model: "gpt-5".to_string(),
                reasoning_effort: None,
                developer_instructions: Some("swarm".to_string()),
            },
        };

        assert!(should_sync_swarm_collaboration_mode(&base, &swarm));
        assert!(!should_sync_swarm_collaboration_mode(&swarm, &swarm));
        assert!(!should_sync_swarm_collaboration_mode(&swarm, &base));
    }

    #[test]
    fn shared_blackboard_owner_thread_id_uses_parent_for_thread_spawn_subagent() {
        let parent_thread_id = ThreadId::new();
        let child_thread_id = ThreadId::new();
        let session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            depth: 1,
            agent_type: None,
            agent_name_hint: None,
        });

        let owner = shared_blackboard_owner_thread_id(child_thread_id, &session_source);
        assert_eq!(owner, parent_thread_id);
    }

    #[test]
    fn shared_blackboard_owner_thread_id_uses_self_for_non_spawn_sources() {
        let conversation_id = ThreadId::new();
        let owner = shared_blackboard_owner_thread_id(conversation_id, &SessionSource::Exec);
        assert_eq!(owner, conversation_id);
    }

    #[tokio::test]
    async fn turn_context_with_model_updates_model_fields() {
        let (session, mut turn_context) = make_session_and_context().await;
        turn_context.reasoning_effort = Some(ReasoningEffortConfig::Minimal);
        let updated = turn_context
            .with_model("gpt-5.1".to_string(), &session.services.models_manager)
            .await;
        let expected_model_info = session
            .services
            .models_manager
            .get_model_info("gpt-5.1", updated.config.as_ref())
            .await;

        assert_eq!(updated.config.model.as_deref(), Some("gpt-5.1"));
        assert_eq!(updated.collaboration_mode.model(), "gpt-5.1");
        assert_eq!(updated.model_info, expected_model_info);
        assert_eq!(
            updated.reasoning_effort,
            Some(ReasoningEffortConfig::Medium)
        );
        assert_eq!(
            updated.collaboration_mode.reasoning_effort(),
            Some(ReasoningEffortConfig::Medium)
        );
        assert_eq!(
            updated.config.model_reasoning_effort,
            Some(ReasoningEffortConfig::Medium)
        );
        assert_eq!(
            updated.truncation_policy,
            expected_model_info.truncation_policy.into()
        );
        assert!(!Arc::ptr_eq(
            &updated.tool_call_gate,
            &turn_context.tool_call_gate
        ));
    }

    #[test]
    fn falls_back_to_content_when_structured_is_null() {
        let ctr = McpCallToolResult {
            content: vec![text_block("hello"), text_block("world")],
            is_error: None,
            structured_content: Some(serde_json::Value::Null),
            meta: None,
        };

        let got = FunctionCallOutputPayload::from(&ctr);
        let expected = FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text(
                serde_json::to_string(&vec![text_block("hello"), text_block("world")]).unwrap(),
            ),
            success: Some(true),
        };

        assert_eq!(expected, got);
    }

    #[test]
    fn success_flag_reflects_is_error_true() {
        let ctr = McpCallToolResult {
            content: vec![text_block("unused")],
            is_error: Some(true),
            structured_content: Some(json!({ "message": "bad" })),
            meta: None,
        };

        let got = FunctionCallOutputPayload::from(&ctr);
        let expected = FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text(
                serde_json::to_string(&json!({ "message": "bad" })).unwrap(),
            ),
            success: Some(false),
        };

        assert_eq!(expected, got);
    }

    #[test]
    fn success_flag_true_with_no_error_and_content_used() {
        let ctr = McpCallToolResult {
            content: vec![text_block("alpha")],
            is_error: Some(false),
            structured_content: None,
            meta: None,
        };

        let got = FunctionCallOutputPayload::from(&ctr);
        let expected = FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text(
                serde_json::to_string(&vec![text_block("alpha")]).unwrap(),
            ),
            success: Some(true),
        };

        assert_eq!(expected, got);
    }

    async fn wait_for_thread_rolled_back(
        rx: &async_channel::Receiver<Event>,
    ) -> crate::protocol::ThreadRolledBackEvent {
        let deadline = StdDuration::from_secs(2);
        let start = std::time::Instant::now();
        loop {
            let remaining = deadline.saturating_sub(start.elapsed());
            let evt = tokio::time::timeout(remaining, rx.recv())
                .await
                .expect("timeout waiting for event")
                .expect("event");
            match evt.msg {
                EventMsg::ThreadRolledBack(payload) => return payload,
                _ => continue,
            }
        }
    }

    async fn wait_for_thread_rollback_failed(rx: &async_channel::Receiver<Event>) -> ErrorEvent {
        let deadline = StdDuration::from_secs(2);
        let start = std::time::Instant::now();
        loop {
            let remaining = deadline.saturating_sub(start.elapsed());
            let evt = tokio::time::timeout(remaining, rx.recv())
                .await
                .expect("timeout waiting for event")
                .expect("event");
            match evt.msg {
                EventMsg::Error(payload)
                    if payload.codex_error_info == Some(CodexErrorInfo::ThreadRollbackFailed) =>
                {
                    return payload;
                }
                _ => continue,
            }
        }
    }

    fn text_block(s: &str) -> serde_json::Value {
        json!({
            "type": "text",
            "text": s,
        })
    }

    async fn build_test_config(codex_home: &Path) -> Config {
        let test_cwd = std::env::temp_dir().join("codex-core-test-cwd");
        std::fs::create_dir_all(&test_cwd).expect("create test cwd");
        ConfigBuilder::default()
            .codex_home(codex_home.to_path_buf())
            .fallback_cwd(Some(test_cwd))
            .build()
            .await
            .expect("load default test config")
    }

    struct EnvVarGuard {
        key: &'static str,
        original: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let original = env::var_os(key);
            unsafe {
                env::set_var(key, value);
            }
            Self { key, original }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.original {
                    Some(value) => env::set_var(self.key, value),
                    None => env::remove_var(self.key),
                }
            }
        }
    }

    fn otel_manager(
        conversation_id: ThreadId,
        config: &Config,
        model_info: &ModelInfo,
        session_source: SessionSource,
    ) -> OtelManager {
        OtelManager::new(
            conversation_id,
            ModelsManager::get_model_offline_for_tests(config.model.as_deref()).as_str(),
            model_info.slug.as_str(),
            None,
            Some("test@test.com".to_string()),
            Some(TelemetryAuthMode::Chatgpt),
            "test_originator".to_string(),
            false,
            "test".to_string(),
            session_source,
        )
    }

    pub(crate) async fn make_session_configuration_for_tests() -> SessionConfiguration {
        let codex_home = tempfile::tempdir().expect("create temp dir");
        let config = build_test_config(codex_home.path()).await;
        let config = Arc::new(config);
        let model = ModelsManager::get_model_offline_for_tests(config.model.as_deref());
        let model_info =
            ModelsManager::construct_model_info_offline_for_tests(model.as_str(), &config);
        let reasoning_effort = config.model_reasoning_effort;
        let collaboration_mode = CollaborationMode {
            mode: ModeKind::Default,
            settings: Settings {
                model,
                reasoning_effort,
                developer_instructions: None,
            },
        };

        SessionConfiguration {
            provider: config.model_provider.clone(),
            collaboration_mode,
            model_reasoning_summary: config.model_reasoning_summary,
            developer_instructions: config.developer_instructions.clone(),
            user_instructions: config.user_instructions.clone(),
            personality: config.personality,
            base_instructions: config
                .base_instructions
                .clone()
                .unwrap_or_else(|| model_info.get_model_instructions(config.personality)),
            compact_prompt: config.compact_prompt.clone(),
            approval_policy: config.permissions.approval_policy.clone(),
            sandbox_policy: config.permissions.sandbox_policy.clone(),
            windows_sandbox_level: WindowsSandboxLevel::from_config(&config),
            cwd: config.cwd.clone(),
            codex_home: config.codex_home.clone(),
            thread_name: None,
            original_config_do_not_use: Arc::clone(&config),
            session_source: SessionSource::Exec,
            dynamic_tools: Vec::new(),
            persist_extended_history: false,
        }
    }

    // todo: use online model info
    pub(crate) async fn make_session_and_context() -> (Session, TurnContext) {
        let (tx_event, _rx_event) = async_channel::unbounded();
        let codex_home = tempfile::tempdir().expect("create temp dir");
        let config = build_test_config(codex_home.path()).await;
        let config = Arc::new(config);
        let conversation_id = ThreadId::default();
        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::from_api_key("Test API Key"));
        let models_manager = Arc::new(ModelsManager::new(
            config.codex_home.clone(),
            auth_manager.clone(),
        ));
        let agent_control = AgentControl::default();
        let exec_policy = ExecPolicyManager::default();
        let (agent_status_tx, _agent_status_rx) = watch::channel(AgentStatus::PendingInit);
        let model = ModelsManager::get_model_offline_for_tests(config.model.as_deref());
        let model_info =
            ModelsManager::construct_model_info_offline_for_tests(model.as_str(), &config);
        let reasoning_effort = config.model_reasoning_effort;
        let collaboration_mode = CollaborationMode {
            mode: ModeKind::Default,
            settings: Settings {
                model,
                reasoning_effort,
                developer_instructions: None,
            },
        };
        let session_configuration = SessionConfiguration {
            provider: config.model_provider.clone(),
            collaboration_mode,
            model_reasoning_summary: config.model_reasoning_summary,
            developer_instructions: config.developer_instructions.clone(),
            user_instructions: config.user_instructions.clone(),
            personality: config.personality,
            base_instructions: config
                .base_instructions
                .clone()
                .unwrap_or_else(|| model_info.get_model_instructions(config.personality)),
            compact_prompt: config.compact_prompt.clone(),
            approval_policy: config.permissions.approval_policy.clone(),
            sandbox_policy: config.permissions.sandbox_policy.clone(),
            windows_sandbox_level: WindowsSandboxLevel::from_config(&config),
            cwd: config.cwd.clone(),
            codex_home: config.codex_home.clone(),
            thread_name: None,
            original_config_do_not_use: Arc::clone(&config),
            session_source: SessionSource::Exec,
            dynamic_tools: Vec::new(),
            persist_extended_history: false,
        };
        let per_turn_config = Session::build_per_turn_config(&session_configuration);
        let model_info = ModelsManager::construct_model_info_offline_for_tests(
            session_configuration.collaboration_mode.model(),
            &per_turn_config,
        );
        let otel_manager = otel_manager(
            conversation_id,
            config.as_ref(),
            &model_info,
            session_configuration.session_source.clone(),
        );

        let mut state = SessionState::new(session_configuration.clone());
        let shared_blackboard_path = blackboard::ensure_session_blackboard(
            &config.cwd,
            shared_blackboard_owner_thread_id(
                conversation_id,
                &session_configuration.session_source,
            ),
        )
        .await
        .expect("initialize shared blackboard for tests");
        state.set_shared_blackboard_path(shared_blackboard_path);
        mark_state_initial_context_seeded(&mut state);
        let skills_manager = Arc::new(SkillsManager::new(config.codex_home.clone()));
        let network_approval = Arc::new(NetworkApprovalService::default());

        let file_watcher = Arc::new(FileWatcher::noop());
        let services = SessionServices {
            mcp_connection_manager: Arc::new(RwLock::new(McpConnectionManager::default())),
            mcp_startup_cancellation_token: Mutex::new(CancellationToken::new()),
            unified_exec_manager: UnifiedExecProcessManager::default(),
            analytics_events_client: AnalyticsEventsClient::new(
                Arc::clone(&config),
                Arc::clone(&auth_manager),
            ),
            hooks: Hooks::new(HooksConfig {
                legacy_notify_argv: config.notify.clone(),
            }),
            rollout: Mutex::new(None),
            user_shell: Arc::new(default_user_shell()),
            shell_snapshot_tx: watch::channel(None).0,
            show_raw_agent_reasoning: config.show_raw_agent_reasoning,
            exec_policy,
            auth_manager: auth_manager.clone(),
            otel_manager: otel_manager.clone(),
            models_manager: Arc::clone(&models_manager),
            tool_approvals: Mutex::new(ApprovalStore::default()),
            skills_manager,
            file_watcher,
            agent_control,
            network_proxy: None,
            network_approval: Arc::clone(&network_approval),
            state_db: None,
            model_client: ModelClient::new(
                Some(auth_manager.clone()),
                conversation_id,
                session_configuration.provider.clone(),
                session_configuration.session_source.clone(),
                config.model_verbosity,
                model_info.prefer_websockets
                    || config.features.enabled(Feature::ResponsesWebsockets)
                    || config.features.enabled(Feature::ResponsesWebsocketsV2),
                config.features.enabled(Feature::ResponsesWebsocketsV2),
                config.features.enabled(Feature::EnableRequestCompression),
                config.features.enabled(Feature::RuntimeMetrics),
                Session::build_model_client_beta_features_header(config.as_ref()),
                model_io_debug_dir(config.as_ref()),
            ),
        };
        let js_repl = Arc::new(JsReplHandle::with_node_path(
            config.js_repl_node_path.clone(),
            config.codex_home.clone(),
        ));

        let turn_context = Session::make_turn_context(
            Some(Arc::clone(&auth_manager)),
            &otel_manager,
            session_configuration.provider.clone(),
            &session_configuration,
            per_turn_config,
            model_info,
            None,
            "turn_id".to_string(),
            Arc::clone(&js_repl),
        );

        let session = Session {
            conversation_id,
            tx_event,
            agent_status: agent_status_tx,
            state: Mutex::new(state),
            features: config.features.clone(),
            pending_mcp_server_refresh_config: Mutex::new(None),
            active_turn: Mutex::new(None),
            services,
            js_repl,
            next_internal_sub_id: AtomicU64::new(0),
        };
        session
            .services
            .agent_control
            .register_agent_name(conversation_id, crate::agent::PRIMARY_AGENT_NAME)
            .expect("register primary agent name for tests");

        (session, turn_context)
    }

    // Like make_session_and_context, but returns Arc<Session> and the event receiver
    // so tests can assert on emitted events.
    pub(crate) async fn make_session_and_context_with_rx() -> (
        Arc<Session>,
        Arc<TurnContext>,
        async_channel::Receiver<Event>,
    ) {
        let (tx_event, rx_event) = async_channel::unbounded();
        let codex_home = tempfile::tempdir().expect("create temp dir");
        let config = build_test_config(codex_home.path()).await;
        let config = Arc::new(config);
        let conversation_id = ThreadId::default();
        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::from_api_key("Test API Key"));
        let models_manager = Arc::new(ModelsManager::new(
            config.codex_home.clone(),
            auth_manager.clone(),
        ));
        let agent_control = AgentControl::default();
        let exec_policy = ExecPolicyManager::default();
        let (agent_status_tx, _agent_status_rx) = watch::channel(AgentStatus::PendingInit);
        let model = ModelsManager::get_model_offline_for_tests(config.model.as_deref());
        let model_info =
            ModelsManager::construct_model_info_offline_for_tests(model.as_str(), &config);
        let reasoning_effort = config.model_reasoning_effort;
        let collaboration_mode = CollaborationMode {
            mode: ModeKind::Default,
            settings: Settings {
                model,
                reasoning_effort,
                developer_instructions: None,
            },
        };
        let session_configuration = SessionConfiguration {
            provider: config.model_provider.clone(),
            collaboration_mode,
            model_reasoning_summary: config.model_reasoning_summary,
            developer_instructions: config.developer_instructions.clone(),
            user_instructions: config.user_instructions.clone(),
            personality: config.personality,
            base_instructions: config
                .base_instructions
                .clone()
                .unwrap_or_else(|| model_info.get_model_instructions(config.personality)),
            compact_prompt: config.compact_prompt.clone(),
            approval_policy: config.permissions.approval_policy.clone(),
            sandbox_policy: config.permissions.sandbox_policy.clone(),
            windows_sandbox_level: WindowsSandboxLevel::from_config(&config),
            cwd: config.cwd.clone(),
            codex_home: config.codex_home.clone(),
            thread_name: None,
            original_config_do_not_use: Arc::clone(&config),
            session_source: SessionSource::Exec,
            dynamic_tools: Vec::new(),
            persist_extended_history: false,
        };
        let per_turn_config = Session::build_per_turn_config(&session_configuration);
        let model_info = ModelsManager::construct_model_info_offline_for_tests(
            session_configuration.collaboration_mode.model(),
            &per_turn_config,
        );
        let otel_manager = otel_manager(
            conversation_id,
            config.as_ref(),
            &model_info,
            session_configuration.session_source.clone(),
        );

        let mut state = SessionState::new(session_configuration.clone());
        mark_state_initial_context_seeded(&mut state);
        let skills_manager = Arc::new(SkillsManager::new(config.codex_home.clone()));
        let network_approval = Arc::new(NetworkApprovalService::default());

        let file_watcher = Arc::new(FileWatcher::noop());
        let services = SessionServices {
            mcp_connection_manager: Arc::new(RwLock::new(McpConnectionManager::default())),
            mcp_startup_cancellation_token: Mutex::new(CancellationToken::new()),
            unified_exec_manager: UnifiedExecProcessManager::default(),
            analytics_events_client: AnalyticsEventsClient::new(
                Arc::clone(&config),
                Arc::clone(&auth_manager),
            ),
            hooks: Hooks::new(HooksConfig {
                legacy_notify_argv: config.notify.clone(),
            }),
            rollout: Mutex::new(None),
            user_shell: Arc::new(default_user_shell()),
            shell_snapshot_tx: watch::channel(None).0,
            show_raw_agent_reasoning: config.show_raw_agent_reasoning,
            exec_policy,
            auth_manager: Arc::clone(&auth_manager),
            otel_manager: otel_manager.clone(),
            models_manager: Arc::clone(&models_manager),
            tool_approvals: Mutex::new(ApprovalStore::default()),
            skills_manager,
            file_watcher,
            agent_control,
            network_proxy: None,
            network_approval: Arc::clone(&network_approval),
            state_db: None,
            model_client: ModelClient::new(
                Some(Arc::clone(&auth_manager)),
                conversation_id,
                session_configuration.provider.clone(),
                session_configuration.session_source.clone(),
                config.model_verbosity,
                model_info.prefer_websockets
                    || config.features.enabled(Feature::ResponsesWebsockets)
                    || config.features.enabled(Feature::ResponsesWebsocketsV2),
                config.features.enabled(Feature::ResponsesWebsocketsV2),
                config.features.enabled(Feature::EnableRequestCompression),
                config.features.enabled(Feature::RuntimeMetrics),
                Session::build_model_client_beta_features_header(config.as_ref()),
                model_io_debug_dir(config.as_ref()),
            ),
        };
        let js_repl = Arc::new(JsReplHandle::with_node_path(
            config.js_repl_node_path.clone(),
            config.codex_home.clone(),
        ));

        let turn_context = Arc::new(Session::make_turn_context(
            Some(Arc::clone(&auth_manager)),
            &otel_manager,
            session_configuration.provider.clone(),
            &session_configuration,
            per_turn_config,
            model_info,
            None,
            "turn_id".to_string(),
            Arc::clone(&js_repl),
        ));

        let session = Arc::new(Session {
            conversation_id,
            tx_event,
            agent_status: agent_status_tx,
            state: Mutex::new(state),
            features: config.features.clone(),
            pending_mcp_server_refresh_config: Mutex::new(None),
            active_turn: Mutex::new(None),
            services,
            js_repl,
            next_internal_sub_id: AtomicU64::new(0),
        });
        session
            .services
            .agent_control
            .register_agent_name(conversation_id, crate::agent::PRIMARY_AGENT_NAME)
            .expect("register primary agent name for tests");

        (session, turn_context, rx_event)
    }

    fn mark_state_initial_context_seeded(state: &mut SessionState) {
        state.initial_context_seeded = true;
    }

    #[tokio::test]
    async fn refresh_mcp_servers_is_deferred_until_next_turn() {
        let (session, turn_context) = make_session_and_context().await;
        let old_token = session.mcp_startup_cancellation_token().await;
        assert!(!old_token.is_cancelled());

        let mcp_oauth_credentials_store_mode =
            serde_json::to_value(OAuthCredentialsStoreMode::Auto).expect("serialize store mode");
        let refresh_config = McpServerRefreshConfig {
            mcp_servers: json!({}),
            mcp_oauth_credentials_store_mode,
        };
        {
            let mut guard = session.pending_mcp_server_refresh_config.lock().await;
            *guard = Some(refresh_config);
        }

        assert!(!old_token.is_cancelled());
        assert!(
            session
                .pending_mcp_server_refresh_config
                .lock()
                .await
                .is_some()
        );

        session
            .refresh_mcp_servers_if_requested(&turn_context)
            .await;

        assert!(old_token.is_cancelled());
        assert!(
            session
                .pending_mcp_server_refresh_config
                .lock()
                .await
                .is_none()
        );
        let new_token = session.mcp_startup_cancellation_token().await;
        assert!(!new_token.is_cancelled());
    }

    #[tokio::test]
    async fn record_model_warning_appends_user_message() {
        let (mut session, turn_context) = make_session_and_context().await;
        let features = Features::with_defaults();
        session.features = features;

        session
            .record_model_warning("too many unified exec processes", &turn_context)
            .await;

        let history = session.clone_history().await;
        let history_items = history.raw_items();
        let last = history_items.last().expect("warning recorded");

        match last {
            ResponseItem::Message { role, content, .. } => {
                assert_eq!(role, "user");
                assert_eq!(
                    content,
                    &vec![ContentItem::InputText {
                        text: "Warning: too many unified exec processes".to_string(),
                    }]
                );
            }
            other => panic!("expected user message, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn spawn_task_hydrates_previous_model() {
        let (sess, tc, _rx) = make_session_and_context_with_rx().await;
        sess.set_previous_model(None).await;
        let input = vec![UserInput::Text {
            text: "hello".to_string(),
            text_elements: Vec::new(),
        }];

        sess.spawn_task(
            Arc::clone(&tc),
            input,
            NeverEndingTask {
                kind: TaskKind::Regular,
                listen_to_cancellation_token: true,
            },
        )
        .await;

        sess.abort_all_tasks(TurnAbortReason::Interrupted).await;
        assert_eq!(
            sess.previous_model().await,
            Some(tc.model_info.slug.clone())
        );
    }

    #[tokio::test]
    async fn build_settings_update_items_emits_environment_item_for_network_changes() {
        let (session, previous_context) = make_session_and_context().await;
        let previous_context = Arc::new(previous_context);
        let mut current_context = previous_context
            .with_model(
                previous_context.model_info.slug.clone(),
                &session.services.models_manager,
            )
            .await;

        let mut config = (*current_context.config).clone();
        let mut requirements = config.config_layer_stack.requirements().clone();
        requirements.network = Some(Sourced::new(
            NetworkConstraints {
                allowed_domains: Some(vec!["api.example.com".to_string()]),
                denied_domains: Some(vec!["blocked.example.com".to_string()]),
                ..Default::default()
            },
            RequirementSource::CloudRequirements,
        ));
        let layers = config
            .config_layer_stack
            .get_layers(ConfigLayerStackOrdering::LowestPrecedenceFirst, true)
            .into_iter()
            .cloned()
            .collect();
        config.config_layer_stack = ConfigLayerStack::new(
            layers,
            requirements,
            config.config_layer_stack.requirements_toml().clone(),
        )
        .expect("rebuild config layer stack with network requirements");
        current_context.config = Arc::new(config);

        let update_items =
            session.build_settings_update_items(Some(&previous_context), None, &current_context);

        let environment_update = update_items
            .iter()
            .find_map(|item| match item {
                ResponseItem::Message { role, content, .. } if role == "user" => {
                    let [ContentItem::InputText { text }] = content.as_slice() else {
                        return None;
                    };
                    text.contains("<environment_context>").then_some(text)
                }
                _ => None,
            })
            .expect("environment update item should be emitted");
        assert!(environment_update.contains("<network enabled=\"true\">"));
        assert!(environment_update.contains("<allowed>api.example.com</allowed>"));
        assert!(environment_update.contains("<denied>blocked.example.com</denied>"));
    }

    #[tokio::test]
    async fn build_initial_context_omits_disabled_instruction_blocks() {
        let (session, mut turn_context) = make_session_and_context().await;
        let mut config = (*turn_context.config).clone();
        config.include_permissions_instructions = false;
        config.include_apps_instructions = false;
        config.include_environment_context = false;
        config.features.enable(Feature::Apps);
        turn_context.config = Arc::new(config);

        let initial_context = session.build_initial_context(&turn_context).await;
        let combined_text = initial_context
            .iter()
            .filter_map(|item| {
                let ResponseItem::Message { content, .. } = item else {
                    return None;
                };
                Some(
                    content
                        .iter()
                        .filter_map(|chunk| match chunk {
                            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                                Some(text.as_str())
                            }
                            ContentItem::InputImage { .. } => None,
                        })
                        .collect::<Vec<_>>()
                        .join(
                            "
",
                        ),
                )
            })
            .collect::<Vec<_>>()
            .join(
                "
",
            );

        assert!(!combined_text.contains("<permissions instructions>"));
        assert!(!combined_text.contains("## Apps"));
        assert!(!combined_text.contains("<environment_context>"));
    }

    #[tokio::test]
    #[serial]
    async fn build_initial_context_uses_logical_workspace_cwd_in_gateway_mode() {
        let _guard = EnvVarGuard::set("CODEX_TOOL_GATEWAY_URL", "http://localhost:1234");
        let (session, mut turn_context) = make_session_and_context().await;
        turn_context.user_instructions = Some("test user instructions".to_string());

        let initial_context = session.build_initial_context(&turn_context).await;
        let combined_text = initial_context
            .iter()
            .filter_map(|item| match item {
                ResponseItem::Message { content, .. } => Some(content),
                _ => None,
            })
            .flat_map(|content| content.iter())
            .filter_map(|chunk| match chunk {
                ContentItem::InputText { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(combined_text.contains("# AGENTS.md instructions for /workspace"));
        assert!(combined_text.contains("<cwd>/workspace</cwd>"));
    }

    #[tokio::test]
    async fn build_settings_update_items_omits_environment_item_when_disabled() {
        let (session, previous_context) = make_session_and_context().await;
        let previous_context = Arc::new(previous_context);
        let mut current_context = previous_context
            .with_model(
                previous_context.model_info.slug.clone(),
                &session.services.models_manager,
            )
            .await;

        let mut config = (*current_context.config).clone();
        config.include_environment_context = false;
        let mut requirements = config.config_layer_stack.requirements().clone();
        requirements.network = Some(Sourced::new(
            NetworkConstraints {
                allowed_domains: Some(vec!["api.example.com".to_string()]),
                ..Default::default()
            },
            RequirementSource::CloudRequirements,
        ));
        let layers = config
            .config_layer_stack
            .get_layers(ConfigLayerStackOrdering::LowestPrecedenceFirst, true)
            .into_iter()
            .cloned()
            .collect();
        config.config_layer_stack = ConfigLayerStack::new(
            layers,
            requirements,
            config.config_layer_stack.requirements_toml().clone(),
        )
        .expect("rebuild config layer stack with network requirements");
        current_context.config = Arc::new(config);

        let update_items =
            session.build_settings_update_items(Some(&previous_context), None, &current_context);

        assert!(
            !update_items.iter().any(|item| match item {
                ResponseItem::Message { role, content, .. } if role == "user" => {
                    matches!(
                        content.as_slice(),
                        [ContentItem::InputText { text }] if text.contains("<environment_context>")
                    )
                }
                _ => false,
            }),
            "did not expect environment context updates when disabled: {update_items:?}"
        );
    }

    #[tokio::test]
    async fn build_settings_update_items_uses_swarm_sub_prompt_for_subagent_context() {
        let (session, previous_context) = make_session_and_context().await;
        let previous_context = Arc::new(previous_context);
        let mut current_context = previous_context
            .with_model(
                previous_context.model_info.slug.clone(),
                &session.services.models_manager,
            )
            .await;
        current_context.collaboration_mode.mode = ModeKind::Swarm;
        current_context
            .collaboration_mode
            .settings
            .developer_instructions = Some("SHOULD_NOT_APPEAR".to_string());
        current_context.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: ThreadId::new(),
            depth: 1,
            agent_type: None,
            agent_name_hint: None,
        });

        let update_items =
            session.build_settings_update_items(Some(&previous_context), None, &current_context);
        let collaboration_update = update_items
            .iter()
            .find_map(|item| match item {
                ResponseItem::Message { role, content, .. } if role == "developer" => {
                    let [ContentItem::InputText { text }] = content.as_slice() else {
                        return None;
                    };
                    text.contains("# Collaboration Mode: Swarm").then_some(text)
                }
                _ => None,
            })
            .expect("collaboration mode update item should be emitted");

        assert!(collaboration_update.contains("Focused Execution Mandate"));
        assert!(!collaboration_update.contains("The Parallelism Mandate"));
        assert!(!collaboration_update.contains("SHOULD_NOT_APPEAR"));
    }

    #[tokio::test]
    async fn build_settings_update_items_uses_swarm_sub_complex_prompt_for_complex_subagent_context()
     {
        let (session, previous_context) = make_session_and_context().await;
        let previous_context = Arc::new(previous_context);
        let mut current_context = previous_context
            .with_model(
                previous_context.model_info.slug.clone(),
                &session.services.models_manager,
            )
            .await;
        current_context.collaboration_mode.mode = ModeKind::Swarm;
        current_context
            .collaboration_mode
            .settings
            .developer_instructions =
            Some(crate::swarm::complex_root_swarm_developer_instructions().to_string());
        current_context.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: ThreadId::new(),
            depth: 1,
            agent_type: None,
            agent_name_hint: None,
        });

        let update_items =
            session.build_settings_update_items(Some(&previous_context), None, &current_context);
        let collaboration_update = update_items
            .iter()
            .find_map(|item| match item {
                ResponseItem::Message { role, content, .. } if role == "developer" => {
                    let [ContentItem::InputText { text }] = content.as_slice() else {
                        return None;
                    };
                    text.contains("Focused Execution Mandate").then_some(text)
                }
                _ => None,
            })
            .expect("collaboration mode update item should be emitted");

        assert!(collaboration_update.contains("Focused Execution Mandate"));
        assert!(!collaboration_update.contains("The Parallelism Mandate"));
    }

    #[test]
    fn response_item_text_if_assistant_message_ignores_non_assistant_items() {
        let item = ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "hello".to_string(),
            }],
            end_turn: None,
            phase: None,
        };

        assert_eq!(response_item_text_if_assistant_message(&item), None);
    }

    #[test]
    fn termination_judge_recent_assistant_messages_are_limited_to_last_fifteen() {
        let mut history = ContextManager::new();
        let items = (0..20)
            .map(|idx| ResponseItem::Message {
                id: None,
                role: if idx % 2 == 0 { "assistant" } else { "user" }.to_string(),
                content: vec![ContentItem::InputText {
                    text: format!("message-{idx}"),
                }],
                end_turn: None,
                phase: None,
            })
            .collect::<Vec<_>>();
        history.record_items(items.iter(), TruncationPolicy::Tokens(1024));

        let messages = history
            .raw_items()
            .iter()
            .filter_map(response_item_text_if_assistant_message)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .take(15)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>();

        assert_eq!(messages.first().map(String::as_str), Some("message-0"));
        assert_eq!(messages.last().map(String::as_str), Some("message-18"));
        assert_eq!(messages.len(), 10);
    }

    #[test]
    fn build_termination_judge_input_mentions_last_agent_message() {
        let input = build_termination_judge_input(
            &["first".to_string(), "second".to_string()],
            &Some("wrapped up".to_string()),
        );

        assert!(input.contains("[1] first"));
        assert!(input.contains("[2] second"));
        assert!(input.contains("last_agent_message: wrapped up"));
    }

    #[tokio::test]
    async fn build_settings_update_items_uses_swarm_sub_verifier_complex_prompt_for_typed_subagent_context()
     {
        let (session, previous_context) = make_session_and_context().await;
        let previous_context = Arc::new(previous_context);
        let mut current_context = previous_context
            .with_model(
                previous_context.model_info.slug.clone(),
                &session.services.models_manager,
            )
            .await;
        current_context.collaboration_mode.mode = ModeKind::Swarm;
        current_context
            .collaboration_mode
            .settings
            .developer_instructions =
            Some(crate::swarm::complex_root_swarm_developer_instructions().to_string());
        current_context.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: ThreadId::new(),
            depth: 1,
            agent_type: Some(SpawnedAgentType::Verifier),
            agent_name_hint: None,
        });

        let update_items =
            session.build_settings_update_items(Some(&previous_context), None, &current_context);
        let collaboration_update = update_items
            .iter()
            .find_map(|item| match item {
                ResponseItem::Message { role, content, .. } if role == "developer" => {
                    let [ContentItem::InputText { text }] = content.as_slice() else {
                        return None;
                    };
                    text.contains("# Collaboration Mode: Swarm Complex")
                        .then_some(text)
                }
                _ => None,
            })
            .expect("collaboration mode update item should be emitted");

        assert!(collaboration_update.contains("Verification Status Vocabulary"));
        assert!(collaboration_update.contains("Role: Verification Agent in Swarm Complex"));
        assert!(!collaboration_update.contains("The Parallelism Mandate"));
    }

    #[derive(Clone, Copy)]
    struct NeverEndingTask {
        kind: TaskKind,
        listen_to_cancellation_token: bool,
    }

    #[async_trait::async_trait]
    impl SessionTask for NeverEndingTask {
        fn kind(&self) -> TaskKind {
            self.kind
        }

        async fn run(
            self: Arc<Self>,
            _session: Arc<SessionTaskContext>,
            _ctx: Arc<TurnContext>,
            _input: Vec<UserInput>,
            cancellation_token: CancellationToken,
        ) -> Option<String> {
            if self.listen_to_cancellation_token {
                cancellation_token.cancelled().await;
                return None;
            }
            loop {
                sleep(Duration::from_secs(60)).await;
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[test_log::test]
    async fn abort_regular_task_emits_turn_aborted_only() {
        let (sess, tc, rx) = make_session_and_context_with_rx().await;
        let input = vec![UserInput::Text {
            text: "hello".to_string(),
            text_elements: Vec::new(),
        }];
        sess.spawn_task(
            Arc::clone(&tc),
            input,
            NeverEndingTask {
                kind: TaskKind::Regular,
                listen_to_cancellation_token: false,
            },
        )
        .await;

        sess.abort_all_tasks(TurnAbortReason::Interrupted).await;

        // Interrupts persist a model-visible `<turn_aborted>` marker into history, but there is no
        // separate client-visible event for that marker (only `EventMsg::TurnAborted`).
        let evt = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout waiting for event")
            .expect("event");
        match evt.msg {
            EventMsg::TurnAborted(e) => assert_eq!(TurnAbortReason::Interrupted, e.reason),
            other => panic!("unexpected event: {other:?}"),
        }
        // No extra events should be emitted after an abort.
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn abort_gracefully_emits_turn_aborted_only() {
        let (sess, tc, rx) = make_session_and_context_with_rx().await;
        let input = vec![UserInput::Text {
            text: "hello".to_string(),
            text_elements: Vec::new(),
        }];
        sess.spawn_task(
            Arc::clone(&tc),
            input,
            NeverEndingTask {
                kind: TaskKind::Regular,
                listen_to_cancellation_token: true,
            },
        )
        .await;

        sess.abort_all_tasks(TurnAbortReason::Interrupted).await;

        // Even if tasks handle cancellation gracefully, interrupts still result in `TurnAborted`
        // being the only client-visible signal.
        let evt = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout waiting for event")
            .expect("event");
        match evt.msg {
            EventMsg::TurnAborted(e) => assert_eq!(TurnAbortReason::Interrupted, e.reason),
            other => panic!("unexpected event: {other:?}"),
        }
        // No extra events should be emitted after an abort.
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn task_finish_persists_leftover_pending_input() {
        let (sess, tc, _rx) = make_session_and_context_with_rx().await;
        let input = vec![UserInput::Text {
            text: "hello".to_string(),
            text_elements: Vec::new(),
        }];
        sess.spawn_task(
            Arc::clone(&tc),
            input,
            NeverEndingTask {
                kind: TaskKind::Regular,
                listen_to_cancellation_token: false,
            },
        )
        .await;

        sess.inject_response_items(vec![ResponseInputItem::Message {
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "late pending input".to_string(),
            }],
        }])
        .await
        .expect("inject pending input into active turn");

        sess.on_task_finished(Arc::clone(&tc), None).await;

        let history = sess.clone_history().await;
        let expected = ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "late pending input".to_string(),
            }],
            end_turn: None,
            phase: None,
        };
        assert!(
            history.raw_items().iter().any(|item| item == &expected),
            "expected pending input to be persisted into history on turn completion"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn swarm_required_reply_reminder_injects_pending_user_input() {
        collab_inbox::reset_for_tests();
        let (sess, tc, _rx) = make_session_and_context_with_rx().await;
        let mut collaboration_mode = tc.collaboration_mode.clone();
        collaboration_mode.mode = ModeKind::Swarm;
        sess.update_settings(SessionSettingsUpdate {
            collaboration_mode: Some(collaboration_mode),
            ..Default::default()
        })
        .await
        .expect("switch to swarm mode");
        let swarm_tc = sess.new_default_turn().await;
        let input = vec![UserInput::Text {
            text: "hello".to_string(),
            text_elements: Vec::new(),
        }];
        sess.spawn_task(
            Arc::clone(&swarm_tc),
            input,
            NeverEndingTask {
                kind: TaskKind::Regular,
                listen_to_cancellation_token: false,
            },
        )
        .await;
        collab_inbox::register_required_reply(
            sess.conversation_id,
            "alice-worker".to_string(),
            ThreadId::new(),
            "msg-1".to_string(),
            "send status".to_string(),
        );

        assert!(maybe_enqueue_swarm_required_reply_reminder(&sess, &swarm_tc).await);
        let pending = sess.get_pending_input().await;
        let [ResponseInputItem::Message { role, content }] = pending.as_slice() else {
            panic!("expected one reminder message");
        };
        assert_eq!(role, "user");
        let [ContentItem::InputText { text }] = content.as_slice() else {
            panic!("expected input text reminder");
        };
        assert!(text.contains("alice-worker"));
        assert!(text.contains("msg-1"));
        assert!(text.contains("send status"));

        sess.abort_all_tasks(TurnAbortReason::Interrupted).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn swarm_required_reply_reminder_aggregates_multiple_obligations() {
        collab_inbox::reset_for_tests();
        let (sess, tc, _rx) = make_session_and_context_with_rx().await;
        let mut collaboration_mode = tc.collaboration_mode.clone();
        collaboration_mode.mode = ModeKind::Swarm;
        sess.update_settings(SessionSettingsUpdate {
            collaboration_mode: Some(collaboration_mode),
            ..Default::default()
        })
        .await
        .expect("switch to swarm mode");
        let swarm_tc = sess.new_default_turn().await;
        let input = vec![UserInput::Text {
            text: "hello".to_string(),
            text_elements: Vec::new(),
        }];
        sess.spawn_task(
            Arc::clone(&swarm_tc),
            input,
            NeverEndingTask {
                kind: TaskKind::Regular,
                listen_to_cancellation_token: false,
            },
        )
        .await;
        collab_inbox::register_required_reply(
            sess.conversation_id,
            "alice-worker".to_string(),
            ThreadId::new(),
            "msg-1".to_string(),
            "first".to_string(),
        );
        collab_inbox::register_required_reply(
            sess.conversation_id,
            "bob-worker".to_string(),
            ThreadId::new(),
            "msg-2".to_string(),
            "second".to_string(),
        );

        assert!(maybe_enqueue_swarm_required_reply_reminder(&sess, &swarm_tc).await);
        let pending = sess.get_pending_input().await;
        let [ResponseInputItem::Message { content, .. }] = pending.as_slice() else {
            panic!("expected one reminder message");
        };
        let [ContentItem::InputText { text }] = content.as_slice() else {
            panic!("expected input text reminder");
        };
        assert!(text.contains("msg-1"));
        assert!(text.contains("msg-2"));
        assert!(text.contains("alice-worker"));
        assert!(text.contains("bob-worker"));

        sess.abort_all_tasks(TurnAbortReason::Interrupted).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn required_reply_reminder_noops_outside_swarm_mode() {
        collab_inbox::reset_for_tests();
        let (sess, tc, _rx) = make_session_and_context_with_rx().await;
        let input = vec![UserInput::Text {
            text: "hello".to_string(),
            text_elements: Vec::new(),
        }];
        sess.spawn_task(
            Arc::clone(&tc),
            input,
            NeverEndingTask {
                kind: TaskKind::Regular,
                listen_to_cancellation_token: false,
            },
        )
        .await;
        collab_inbox::register_required_reply(
            sess.conversation_id,
            "alice-worker".to_string(),
            ThreadId::new(),
            "msg-1".to_string(),
            "send status".to_string(),
        );

        assert!(!maybe_enqueue_swarm_required_reply_reminder(&sess, &tc).await);
        assert!(sess.get_pending_input().await.is_empty());

        sess.abort_all_tasks(TurnAbortReason::Interrupted).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn swarm_required_reply_reminder_keeps_injecting_while_unresolved() {
        collab_inbox::reset_for_tests();
        let (sess, tc, _rx) = make_session_and_context_with_rx().await;
        let mut collaboration_mode = tc.collaboration_mode.clone();
        collaboration_mode.mode = ModeKind::Swarm;
        sess.update_settings(SessionSettingsUpdate {
            collaboration_mode: Some(collaboration_mode),
            ..Default::default()
        })
        .await
        .expect("switch to swarm mode");
        let swarm_tc = sess.new_default_turn().await;
        let input = vec![UserInput::Text {
            text: "hello".to_string(),
            text_elements: Vec::new(),
        }];
        sess.spawn_task(
            Arc::clone(&swarm_tc),
            input,
            NeverEndingTask {
                kind: TaskKind::Regular,
                listen_to_cancellation_token: false,
            },
        )
        .await;
        collab_inbox::register_required_reply(
            sess.conversation_id,
            "alice-worker".to_string(),
            ThreadId::new(),
            "msg-1".to_string(),
            "send status".to_string(),
        );

        assert!(maybe_enqueue_swarm_required_reply_reminder(&sess, &swarm_tc).await);
        assert!(maybe_enqueue_swarm_required_reply_reminder(&sess, &swarm_tc).await);
        assert!(maybe_enqueue_swarm_required_reply_reminder(&sess, &swarm_tc).await);
        let unresolved = collab_inbox::unresolved_required_reply_obligations(sess.conversation_id);
        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0].reminder_count, 0);

        sess.abort_all_tasks(TurnAbortReason::Interrupted).await;
    }

    #[tokio::test]
    async fn finalization_guard_warns_before_clearing_unresolved_outgoing_replies() {
        collab_inbox::reset_for_tests();
        let (sess, tc, _rx) = make_session_and_context_with_rx().await;
        let mut collaboration_mode = tc.collaboration_mode.clone();
        collaboration_mode.mode = ModeKind::Swarm;
        sess.update_settings(SessionSettingsUpdate {
            collaboration_mode: Some(collaboration_mode),
            ..Default::default()
        })
        .await
        .expect("switch to swarm mode");
        let swarm_tc = sess.new_default_turn().await;
        let input = vec![UserInput::Text {
            text: "hello".to_string(),
            text_elements: Vec::new(),
        }];
        sess.spawn_task(
            Arc::clone(&swarm_tc),
            input,
            NeverEndingTask {
                kind: TaskKind::Regular,
                listen_to_cancellation_token: false,
            },
        )
        .await;
        let target = ThreadId::new();
        sess.services
            .agent_control
            .register_agent_name(target, "Dakota-worker")
            .expect("register target agent");

        collab_inbox::register_required_reply(
            target,
            "wmj-assistant".to_string(),
            sess.conversation_id,
            "main-dakota-finalcheck".to_string(),
            "please validate final artifact".to_string(),
        );

        assert!(
            maybe_enqueue_swarm_pending_required_reply_finalization_guard(
                &sess,
                &swarm_tc,
                &Some("final response".to_string()),
            )
            .await
        );

        let pending = sess.get_pending_input().await;
        let [ResponseInputItem::Message { role, content }] = pending.as_slice() else {
            panic!("expected one finalization guard message");
        };
        assert_eq!(role, "user");
        let [ContentItem::InputText { text }] = content.as_slice() else {
            panic!("expected input text finalization guard");
        };
        assert!(text.contains("Swarm finalization guard"));
        assert!(text.contains("Dakota-worker"));
        assert!(text.contains("main-dakota-finalcheck"));
        assert!(text.contains("please validate final artifact"));

        let unresolved = collab_inbox::unresolved_required_reply_obligations(target);
        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0].message_id, "main-dakota-finalcheck");

        sess.abort_all_tasks(TurnAbortReason::Interrupted).await;
    }

    #[tokio::test]
    async fn finalization_guard_only_warns_once_for_same_outgoing_reply() {
        collab_inbox::reset_for_tests();
        let (sess, tc, _rx) = make_session_and_context_with_rx().await;
        let mut collaboration_mode = tc.collaboration_mode.clone();
        collaboration_mode.mode = ModeKind::Swarm;
        sess.update_settings(SessionSettingsUpdate {
            collaboration_mode: Some(collaboration_mode),
            ..Default::default()
        })
        .await
        .expect("switch to swarm mode");
        let swarm_tc = sess.new_default_turn().await;
        let input = vec![UserInput::Text {
            text: "hello".to_string(),
            text_elements: Vec::new(),
        }];
        sess.spawn_task(
            Arc::clone(&swarm_tc),
            input,
            NeverEndingTask {
                kind: TaskKind::Regular,
                listen_to_cancellation_token: false,
            },
        )
        .await;
        let target = ThreadId::new();

        collab_inbox::register_required_reply(
            target,
            "wmj-assistant".to_string(),
            sess.conversation_id,
            "main-dakota-finalcheck".to_string(),
            "please validate final artifact".to_string(),
        );

        assert!(
            maybe_enqueue_swarm_pending_required_reply_finalization_guard(
                &sess,
                &swarm_tc,
                &Some("final response".to_string()),
            )
            .await
        );
        assert!(
            !maybe_enqueue_swarm_pending_required_reply_finalization_guard(
                &sess,
                &swarm_tc,
                &Some("final response".to_string()),
            )
            .await
        );

        let unresolved = collab_inbox::unresolved_required_reply_obligations(target);
        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0].reminder_count, 1);
    }

    #[tokio::test]
    async fn primary_completion_clears_required_reply_obligations_for_main_thread() {
        collab_inbox::reset_for_tests();
        let (sess, tc, _rx) = make_session_and_context_with_rx().await;
        let mut collaboration_mode = tc.collaboration_mode.clone();
        collaboration_mode.mode = ModeKind::Swarm;
        sess.update_settings(SessionSettingsUpdate {
            collaboration_mode: Some(collaboration_mode),
            ..Default::default()
        })
        .await
        .expect("switch to swarm mode");
        let swarm_tc = sess.new_default_turn().await;
        let other_receiver = ThreadId::new();
        let other_source = ThreadId::new();

        // Main thread owes a reply to another source.
        collab_inbox::register_required_reply(
            sess.conversation_id,
            "sub-agent".to_string(),
            other_source,
            "msg-main-1".to_string(),
            "reply needed".to_string(),
        );
        // Another receiver owes a reply to the main thread.
        collab_inbox::register_required_reply(
            other_receiver,
            "wmj-assistant".to_string(),
            sess.conversation_id,
            "msg-sub-1".to_string(),
            "status update".to_string(),
        );
        // Unrelated obligation should stay untouched in this fallback test path.
        collab_inbox::register_required_reply(
            other_receiver,
            "another-agent".to_string(),
            ThreadId::new(),
            "msg-sub-2".to_string(),
            "independent".to_string(),
        );

        maybe_clear_required_reply_obligations_after_primary_completion(
            &sess,
            &swarm_tc,
            &Some("final response".to_string()),
        )
        .await;

        assert!(
            collab_inbox::unresolved_required_reply_obligations(sess.conversation_id).is_empty()
        );
        let unresolved_other = collab_inbox::unresolved_required_reply_obligations(other_receiver);
        assert_eq!(unresolved_other.len(), 1);
        assert_eq!(unresolved_other[0].message_id, "msg-sub-2");
    }

    #[tokio::test]
    async fn primary_completion_does_not_clear_required_reply_obligations_for_subagent_turn() {
        collab_inbox::reset_for_tests();
        let (session, mut turn_context) = make_session_and_context().await;
        let session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: ThreadId::new(),
            depth: 1,
            agent_type: None,
            agent_name_hint: None,
        });
        turn_context.session_source = session_source;
        turn_context.collaboration_mode.mode = ModeKind::Swarm;
        let sess = Arc::new(session);
        let tc = Arc::new(turn_context);

        collab_inbox::register_required_reply(
            sess.conversation_id,
            "alice-worker".to_string(),
            ThreadId::new(),
            "msg-1".to_string(),
            "send status".to_string(),
        );

        maybe_clear_required_reply_obligations_after_primary_completion(
            &sess,
            &tc,
            &Some("final response".to_string()),
        )
        .await;

        let unresolved = collab_inbox::unresolved_required_reply_obligations(sess.conversation_id);
        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0].message_id, "msg-1");
    }

    #[tokio::test]
    async fn steer_input_requires_active_turn() {
        let (sess, _tc, _rx) = make_session_and_context_with_rx().await;
        let input = vec![UserInput::Text {
            text: "steer".to_string(),
            text_elements: Vec::new(),
        }];

        let err = sess
            .steer_input(input, None)
            .await
            .expect_err("steering without active turn should fail");

        assert!(matches!(err, SteerInputError::NoActiveTurn(_)));
    }

    #[tokio::test]
    async fn steer_input_enforces_expected_turn_id() {
        let (sess, tc, _rx) = make_session_and_context_with_rx().await;
        let input = vec![UserInput::Text {
            text: "hello".to_string(),
            text_elements: Vec::new(),
        }];
        sess.spawn_task(
            Arc::clone(&tc),
            input,
            NeverEndingTask {
                kind: TaskKind::Regular,
                listen_to_cancellation_token: false,
            },
        )
        .await;

        let steer_input = vec![UserInput::Text {
            text: "steer".to_string(),
            text_elements: Vec::new(),
        }];
        let err = sess
            .steer_input(steer_input, Some("different-turn-id"))
            .await
            .expect_err("mismatched expected turn id should fail");

        match err {
            SteerInputError::ExpectedTurnMismatch { expected, actual } => {
                assert_eq!(
                    (expected, actual),
                    ("different-turn-id".to_string(), tc.sub_id.clone())
                );
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn steer_input_returns_active_turn_id() {
        let (sess, tc, _rx) = make_session_and_context_with_rx().await;
        let input = vec![UserInput::Text {
            text: "hello".to_string(),
            text_elements: Vec::new(),
        }];
        sess.spawn_task(
            Arc::clone(&tc),
            input,
            NeverEndingTask {
                kind: TaskKind::Regular,
                listen_to_cancellation_token: false,
            },
        )
        .await;

        let steer_input = vec![UserInput::Text {
            text: "steer".to_string(),
            text_elements: Vec::new(),
        }];
        let turn_id = sess
            .steer_input(steer_input, Some(&tc.sub_id))
            .await
            .expect("steering with matching expected turn id should succeed");

        assert_eq!(turn_id, tc.sub_id);
        assert!(sess.has_pending_input().await);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn abort_review_task_emits_exited_then_aborted_and_records_history() {
        let (sess, tc, rx) = make_session_and_context_with_rx().await;
        let input = vec![UserInput::Text {
            text: "start review".to_string(),
            text_elements: Vec::new(),
        }];
        sess.spawn_task(Arc::clone(&tc), input, ReviewTask::new())
            .await;

        sess.abort_all_tasks(TurnAbortReason::Interrupted).await;

        // Aborting a review task should exit review mode before surfacing the abort to the client.
        // We scan for these events (rather than relying on fixed ordering) since unrelated events
        // may interleave.
        let mut exited_review_mode_idx = None;
        let mut turn_aborted_idx = None;
        let mut idx = 0usize;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let evt = tokio::time::timeout(remaining, rx.recv())
                .await
                .expect("timeout waiting for event")
                .expect("event");
            let event_idx = idx;
            idx = idx.saturating_add(1);
            match evt.msg {
                EventMsg::ExitedReviewMode(ev) => {
                    assert!(ev.review_output.is_none());
                    exited_review_mode_idx = Some(event_idx);
                }
                EventMsg::TurnAborted(ev) => {
                    assert_eq!(TurnAbortReason::Interrupted, ev.reason);
                    turn_aborted_idx = Some(event_idx);
                    break;
                }
                _ => {}
            }
        }
        assert!(
            exited_review_mode_idx.is_some(),
            "expected ExitedReviewMode after abort"
        );
        assert!(
            turn_aborted_idx.is_some(),
            "expected TurnAborted after abort"
        );
        assert!(
            exited_review_mode_idx.unwrap() < turn_aborted_idx.unwrap(),
            "expected ExitedReviewMode before TurnAborted"
        );

        let history = sess.clone_history().await;
        // The `<turn_aborted>` marker is silent in the event stream, so verify it is still
        // recorded in history for the model.
        assert!(
            history.raw_items().iter().any(|item| {
                let ResponseItem::Message { role, content, .. } = item else {
                    return false;
                };
                if role != "user" {
                    return false;
                }
                content.iter().any(|content_item| {
                    let ContentItem::InputText { text } = content_item else {
                        return false;
                    };
                    text.contains(crate::session_prefix::TURN_ABORTED_OPEN_TAG)
                })
            }),
            "expected a model-visible turn aborted marker in history after interrupt"
        );
    }

    #[tokio::test]
    async fn fatal_tool_error_stops_turn_and_reports_error() {
        let (session, turn_context, _rx) = make_session_and_context_with_rx().await;
        let tools = {
            session
                .services
                .mcp_connection_manager
                .read()
                .await
                .list_all_tools()
                .await
        };
        let app_tools = Some(tools.clone());
        let router = ToolRouter::from_config(
            &turn_context.tools_config,
            Some(
                tools
                    .into_iter()
                    .map(|(name, tool)| (name, tool.tool))
                    .collect(),
            ),
            app_tools,
            turn_context.dynamic_tools.as_slice(),
        );
        let item = ResponseItem::CustomToolCall {
            id: None,
            status: None,
            call_id: "call-1".to_string(),
            name: "shell".to_string(),
            input: "{}".to_string(),
        };

        let call = ToolRouter::build_tool_call(session.as_ref(), item.clone())
            .await
            .expect("build tool call")
            .expect("tool call present");
        let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
        let err = router
            .dispatch_tool_call(
                Arc::clone(&session),
                Arc::clone(&turn_context),
                tracker,
                call,
                ToolCallSource::Direct,
            )
            .await
            .expect_err("expected fatal error");

        match err {
            FunctionCallError::Fatal(message) => {
                assert_eq!(message, "tool shell invoked with incompatible payload");
            }
            other => panic!("expected FunctionCallError::Fatal, got {other:?}"),
        }
    }

    async fn sample_rollout(
        session: &Session,
        _turn_context: &TurnContext,
    ) -> (Vec<RolloutItem>, Vec<ResponseItem>) {
        let mut rollout_items = Vec::new();
        let mut live_history = ContextManager::new();

        // Use the same turn_context source as record_initial_history so model_info (and thus
        // personality_spec) matches reconstruction.
        let reconstruction_turn = session.new_default_turn().await;
        let mut initial_context = session
            .build_initial_context(reconstruction_turn.as_ref())
            .await;
        // Ensure personality_spec is present when Personality is enabled, so expected matches
        // what reconstruction produces (build_initial_context may omit it when baked into model).
        if !initial_context.iter().any(|m| {
            matches!(m, ResponseItem::Message { role, content, .. }
                if role == "developer"
                    && content.iter().any(|c| {
                        matches!(c, ContentItem::InputText { text } if text.contains("<personality_spec>"))
                    }))
        })
            && let Some(p) = reconstruction_turn.personality
            && session.features.enabled(Feature::Personality)
            && let Some(personality_message) = reconstruction_turn
                .model_info
                .model_messages
                .as_ref()
                .and_then(|m| m.get_personality_message(Some(p)).filter(|s| !s.is_empty()))
        {
            let msg =
                DeveloperInstructions::personality_spec_message(personality_message).into();
            let insert_at = initial_context
                .iter()
                .position(|m| matches!(m, ResponseItem::Message { role, .. } if role == "developer"))
                .map(|i| i + 1)
                .unwrap_or(0);
            initial_context.insert(insert_at, msg);
        }
        for item in &initial_context {
            rollout_items.push(RolloutItem::ResponseItem(item.clone()));
        }
        live_history.record_items(
            initial_context.iter(),
            reconstruction_turn.truncation_policy,
        );

        let user1 = ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "first user".to_string(),
            }],
            end_turn: None,
            phase: None,
        };
        live_history.record_items(
            std::iter::once(&user1),
            reconstruction_turn.truncation_policy,
        );
        rollout_items.push(RolloutItem::ResponseItem(user1.clone()));

        let assistant1 = ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: "assistant reply one".to_string(),
            }],
            end_turn: None,
            phase: None,
        };
        live_history.record_items(
            std::iter::once(&assistant1),
            reconstruction_turn.truncation_policy,
        );
        rollout_items.push(RolloutItem::ResponseItem(assistant1.clone()));

        let summary1 = "summary one";
        let snapshot1 = live_history
            .clone()
            .for_prompt(&reconstruction_turn.model_info.input_modalities);
        let user_messages1 = collect_user_messages(&snapshot1);
        let rebuilt1 =
            compact::build_compacted_history(initial_context.clone(), &user_messages1, summary1);
        live_history.replace(rebuilt1);
        rollout_items.push(RolloutItem::Compacted(CompactedItem {
            message: summary1.to_string(),
            replacement_history: None,
        }));

        let user2 = ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "second user".to_string(),
            }],
            end_turn: None,
            phase: None,
        };
        live_history.record_items(
            std::iter::once(&user2),
            reconstruction_turn.truncation_policy,
        );
        rollout_items.push(RolloutItem::ResponseItem(user2.clone()));

        let assistant2 = ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: "assistant reply two".to_string(),
            }],
            end_turn: None,
            phase: None,
        };
        live_history.record_items(
            std::iter::once(&assistant2),
            reconstruction_turn.truncation_policy,
        );
        rollout_items.push(RolloutItem::ResponseItem(assistant2.clone()));

        let summary2 = "summary two";
        let snapshot2 = live_history
            .clone()
            .for_prompt(&reconstruction_turn.model_info.input_modalities);
        let user_messages2 = collect_user_messages(&snapshot2);
        let rebuilt2 =
            compact::build_compacted_history(initial_context.clone(), &user_messages2, summary2);
        live_history.replace(rebuilt2);
        rollout_items.push(RolloutItem::Compacted(CompactedItem {
            message: summary2.to_string(),
            replacement_history: None,
        }));

        let user3 = ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "third user".to_string(),
            }],
            end_turn: None,
            phase: None,
        };
        live_history.record_items(
            std::iter::once(&user3),
            reconstruction_turn.truncation_policy,
        );
        rollout_items.push(RolloutItem::ResponseItem(user3));

        let assistant3 = ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: "assistant reply three".to_string(),
            }],
            end_turn: None,
            phase: None,
        };
        live_history.record_items(
            std::iter::once(&assistant3),
            reconstruction_turn.truncation_policy,
        );
        rollout_items.push(RolloutItem::ResponseItem(assistant3));

        (
            rollout_items,
            live_history.for_prompt(&reconstruction_turn.model_info.input_modalities),
        )
    }

    #[tokio::test]
    async fn rejects_escalated_permissions_when_policy_not_on_request() {
        use crate::exec::ExecParams;
        use crate::protocol::AskForApproval;
        use crate::protocol::SandboxPolicy;
        use crate::sandboxing::SandboxPermissions;
        use crate::turn_diff_tracker::TurnDiffTracker;
        use std::collections::HashMap;

        let (session, mut turn_context_raw) = make_session_and_context().await;
        // Ensure policy is NOT OnRequest so the early rejection path triggers
        turn_context_raw.approval_policy = AskForApproval::OnFailure;
        let session = Arc::new(session);
        let mut turn_context = Arc::new(turn_context_raw);

        let timeout_ms = 1000;
        let sandbox_permissions = SandboxPermissions::RequireEscalated;
        let params = ExecParams {
            command: if cfg!(windows) {
                vec![
                    "cmd.exe".to_string(),
                    "/C".to_string(),
                    "echo hi".to_string(),
                ]
            } else {
                vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "echo hi".to_string(),
                ]
            },
            cwd: turn_context.cwd.clone(),
            expiration: timeout_ms.into(),
            env: HashMap::new(),
            network: None,
            network_attempt_id: None,
            sandbox_permissions,
            windows_sandbox_level: turn_context.windows_sandbox_level,
            justification: Some("test".to_string()),
            arg0: None,
        };

        let params2 = ExecParams {
            sandbox_permissions: SandboxPermissions::UseDefault,
            command: params.command.clone(),
            cwd: params.cwd.clone(),
            expiration: timeout_ms.into(),
            env: HashMap::new(),
            network: None,
            network_attempt_id: None,
            windows_sandbox_level: turn_context.windows_sandbox_level,
            justification: params.justification.clone(),
            arg0: None,
        };

        let turn_diff_tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));

        let tool_name = "shell";
        let call_id = "test-call".to_string();

        let handler = ShellHandler;
        let resp = handler
            .handle(ToolInvocation {
                session: Arc::clone(&session),
                turn: Arc::clone(&turn_context),
                tracker: Arc::clone(&turn_diff_tracker),
                call_id,
                tool_name: tool_name.to_string(),
                payload: ToolPayload::Function {
                    arguments: serde_json::json!({
                        "command": params.command.clone(),
                        "workdir": Some(turn_context.cwd.to_string_lossy().to_string()),
                        "timeout_ms": params.expiration.timeout_ms(),
                        "sandbox_permissions": params.sandbox_permissions,
                        "justification": params.justification.clone(),
                    })
                    .to_string(),
                },
            })
            .await;

        let Err(FunctionCallError::RespondToModel(output)) = resp else {
            panic!("expected error result");
        };

        let expected = format!(
            "approval policy is {policy:?}; reject command — you should not ask for escalated permissions if the approval policy is {policy:?}",
            policy = turn_context.approval_policy
        );

        pretty_assertions::assert_eq!(output, expected);

        // Now retry the same command WITHOUT escalated permissions; should succeed.
        // Force DangerFullAccess to avoid platform sandbox dependencies in tests.
        Arc::get_mut(&mut turn_context)
            .expect("unique turn context Arc")
            .sandbox_policy = SandboxPolicy::DangerFullAccess;

        let resp2 = handler
            .handle(ToolInvocation {
                session: Arc::clone(&session),
                turn: Arc::clone(&turn_context),
                tracker: Arc::clone(&turn_diff_tracker),
                call_id: "test-call-2".to_string(),
                tool_name: tool_name.to_string(),
                payload: ToolPayload::Function {
                    arguments: serde_json::json!({
                        "command": params2.command.clone(),
                        "workdir": Some(turn_context.cwd.to_string_lossy().to_string()),
                        "timeout_ms": params2.expiration.timeout_ms(),
                        "sandbox_permissions": params2.sandbox_permissions,
                        "justification": params2.justification.clone(),
                    })
                    .to_string(),
                },
            })
            .await;

        let output = match resp2.expect("expected Ok result") {
            ToolOutput::Function {
                body: FunctionCallOutputBody::Text(content),
                ..
            } => content,
            _ => panic!("unexpected tool output"),
        };

        #[derive(Deserialize, PartialEq, Eq, Debug)]
        struct ResponseExecMetadata {
            exit_code: i32,
        }

        #[derive(Deserialize)]
        struct ResponseExecOutput {
            output: String,
            metadata: ResponseExecMetadata,
        }

        let exec_output: ResponseExecOutput =
            serde_json::from_str(&output).expect("valid exec output json");

        pretty_assertions::assert_eq!(exec_output.metadata, ResponseExecMetadata { exit_code: 0 });
        assert!(exec_output.output.contains("hi"));
    }
    #[tokio::test]
    async fn unified_exec_rejects_escalated_permissions_when_policy_not_on_request() {
        use crate::protocol::AskForApproval;
        use crate::sandboxing::SandboxPermissions;
        use crate::turn_diff_tracker::TurnDiffTracker;

        let (session, mut turn_context_raw) = make_session_and_context().await;
        turn_context_raw.approval_policy = AskForApproval::OnFailure;
        let session = Arc::new(session);
        let turn_context = Arc::new(turn_context_raw);
        let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));

        let handler = UnifiedExecHandler;
        let resp = handler
            .handle(ToolInvocation {
                session: Arc::clone(&session),
                turn: Arc::clone(&turn_context),
                tracker: Arc::clone(&tracker),
                call_id: "exec-call".to_string(),
                tool_name: "exec_command".to_string(),
                payload: ToolPayload::Function {
                    arguments: serde_json::json!({
                        "cmd": "echo hi",
                        "sandbox_permissions": SandboxPermissions::RequireEscalated,
                        "justification": "need unsandboxed execution",
                    })
                    .to_string(),
                },
            })
            .await;

        let Err(FunctionCallError::RespondToModel(output)) = resp else {
            panic!("expected error result");
        };

        let expected = format!(
            "approval policy is {policy:?}; reject command — you cannot ask for escalated permissions if the approval policy is {policy:?}",
            policy = turn_context.approval_policy
        );

        pretty_assertions::assert_eq!(output, expected);
    }
}
