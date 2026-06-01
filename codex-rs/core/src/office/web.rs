use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::header::ACCEPT;
use axum::http::header::CONTENT_TYPE;
use axum::http::header::COOKIE;
use axum::http::header::LOCATION;
use axum::http::header::SET_COOKIE;
use axum::response::Html;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::routing::post;
use codex_protocol::ThreadId;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::TurnItem;
use codex_protocol::models::WebSearchAction;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecCommandStatus;
use codex_protocol::protocol::PatchApplyStatus;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SpawnedAgentType;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::user_input::UserInput;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::de;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use uuid::Uuid;

use super::AgentMemoryStore;
use super::AgentOwnerBinding;
use super::AgentProfile;
use super::AgentRuntimeState;
use super::DailyReflection;
use super::HumanProfile;
use super::OfficeActivityEntry;
use super::OfficeActivityKind;
use super::OfficeAgentContextSnapshot;
use super::OfficeBlackboard;
use super::OfficeBlackboardError;
use super::OfficeBlackboardStore;
use super::OfficeRuntimeStore;
use super::OfficeRuntimeStoreError;
use super::PersistentPilotDirectory;
use super::PilotStoreError;
use super::PilotWebSession;
use super::prompts::OfficeOwnerPromptContext;
use super::prompts::merge_office_developer_instructions;
use super::prompts::render_office_agent_developer_instructions;
use super::prompts::render_owner_turn_context;
use crate::ThreadManager;
use crate::agent::AgentControl;
use crate::agent::write_agent_owner_profile_prompt_for_agent;
use crate::config::Config;
use crate::tools::handlers::collab_inbox;

const DEFAULT_SESSION_COOKIE: &str = "codex_office_session";
const OFFICE_CEO_AGENT_ID: &str = "agent_ceo";
const OFFICE_FIXED_AGENT_IDS: [&str; 6] = [
    "agent_ceo",
    "agent_a",
    "agent_b",
    "agent_c",
    "agent_d",
    "agent_e",
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OfficeWebConfig {
    pub store_path: PathBuf,
    pub runtime_store_path: PathBuf,
    pub collab_inbox_path: PathBuf,
    pub blackboard_path: PathBuf,
    pub memory_root: PathBuf,
    pub codex_home: PathBuf,
    pub session_cookie: String,
}

impl OfficeWebConfig {
    pub fn new(store_path: impl Into<PathBuf>) -> Self {
        let store_path = store_path.into();
        let runtime_store_path = store_path.with_file_name("office-runtime.json");
        let collab_inbox_path = store_path.with_file_name("office-collab-inbox.json");
        let blackboard_path = store_path.with_file_name("office-blackboard.json");
        let memory_root = store_path
            .parent()
            .map(|parent| parent.join("office-memory"))
            .unwrap_or_else(|| PathBuf::from("office-memory"));
        let codex_home = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|home| home.join(".codex"))
                    .unwrap_or_else(|| PathBuf::from(".codex"))
            });
        Self {
            store_path,
            runtime_store_path,
            collab_inbox_path,
            blackboard_path,
            memory_root,
            codex_home,
            session_cookie: DEFAULT_SESSION_COOKIE.to_string(),
        }
    }

    pub fn with_session_cookie(mut self, session_cookie: impl Into<String>) -> Self {
        self.session_cookie = session_cookie.into();
        self
    }

    pub fn with_runtime_store_path(mut self, runtime_store_path: impl Into<PathBuf>) -> Self {
        self.runtime_store_path = runtime_store_path.into();
        self
    }

    pub fn with_collab_inbox_path(mut self, collab_inbox_path: impl Into<PathBuf>) -> Self {
        self.collab_inbox_path = collab_inbox_path.into();
        self
    }

    pub fn with_blackboard_path(mut self, blackboard_path: impl Into<PathBuf>) -> Self {
        self.blackboard_path = blackboard_path.into();
        self
    }

    pub fn with_memory_root(mut self, memory_root: impl Into<PathBuf>) -> Self {
        self.memory_root = memory_root.into();
        self
    }

    pub fn with_codex_home(mut self, codex_home: impl Into<PathBuf>) -> Self {
        self.codex_home = codex_home.into();
        self
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OfficeWebError {
    #[error(transparent)]
    PilotStore(#[from] PilotStoreError),
    #[error(transparent)]
    RuntimeStore(#[from] OfficeRuntimeStoreError),
    #[error(transparent)]
    Blackboard(#[from] OfficeBlackboardError),
    #[error("failed to bind office web server at {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        source: std::io::Error,
    },
    #[error("office web server failed: {0}")]
    Serve(std::io::Error),
}

#[derive(Clone, Debug)]
pub struct OfficeWebApp {
    config: OfficeWebConfig,
    state: Arc<OfficeWebState>,
}

struct OfficeWebState {
    config: OfficeWebConfig,
    directory: Mutex<PersistentPilotDirectory>,
    runtime_store: Mutex<OfficeRuntimeStore>,
    _blackboard_store: Mutex<OfficeBlackboardStore>,
    memory_store: AgentMemoryStore,
    runtime_bridge: Option<Arc<dyn OfficeRuntimeBridge>>,
}

impl std::fmt::Debug for OfficeWebState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OfficeWebState")
            .field("config", &self.config)
            .field("directory", &self.directory)
            .field("runtime_store", &self.runtime_store)
            .field("_blackboard_store", &self._blackboard_store)
            .field("memory_store", &self.memory_store)
            .field("runtime_bridge", &self.runtime_bridge.is_some())
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OfficeOwnerDelivery {
    pub agent_id: String,
    pub owner_user_id: String,
    pub message_id: String,
    pub owner_reply_target_message_id: Option<String>,
    pub content: String,
    pub need_reply: bool,
    pub reply_to_message_id: Option<String>,
}

#[derive(Clone, Debug)]
struct OfficeOwnerContext {
    agent_id: String,
    owner_user_id: String,
    binding_version: u64,
    binding: AgentOwnerBinding,
}

impl OfficeOwnerContext {
    fn to_prompt_context(&self) -> OfficeOwnerPromptContext {
        OfficeOwnerPromptContext {
            agent_id: self.agent_id.clone(),
            owner_user_id: self.owner_user_id.clone(),
            binding_type: format!("{:?}", self.binding.binding_type),
            binding_status: format!("{:?}", self.binding.status),
            binding_version: self.binding_version,
        }
    }
}

#[async_trait]
pub trait OfficeRuntimeBridge: Send + Sync + std::fmt::Debug {
    async fn deliver_owner_message(
        &self,
        delivery: OfficeOwnerDelivery,
    ) -> Result<Option<ThreadId>, String>;
    fn thread_id_for_agent(&self, agent_id: &str) -> Option<ThreadId>;
}

#[derive(Clone)]
pub(crate) struct AgentControlOfficeRuntimeBridge {
    agent_control: AgentControl,
}

impl std::fmt::Debug for AgentControlOfficeRuntimeBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentControlOfficeRuntimeBridge").finish()
    }
}

impl AgentControlOfficeRuntimeBridge {
    pub(crate) fn new(agent_control: AgentControl) -> Self {
        Self { agent_control }
    }
}

#[async_trait]
impl OfficeRuntimeBridge for AgentControlOfficeRuntimeBridge {
    async fn deliver_owner_message(
        &self,
        delivery: OfficeOwnerDelivery,
    ) -> Result<Option<ThreadId>, String> {
        let Some(thread_id) = self
            .agent_control
            .thread_id_for_agent_name(&delivery.agent_id)
        else {
            return Ok(None);
        };
        self.agent_control
            .send_input(thread_id, owner_delivery_user_input(&delivery))
            .await
            .map(|_| Some(thread_id))
            .map_err(|err| err.to_string())
    }

    fn thread_id_for_agent(&self, agent_id: &str) -> Option<ThreadId> {
        self.agent_control.thread_id_for_agent_name(agent_id)
    }
}

#[derive(Clone)]
pub(crate) struct ThreadManagerOfficeRuntimeBridge {
    thread_manager: Arc<ThreadManager>,
    agent_control: AgentControl,
    agent_config: Config,
    runtime_store_path: PathBuf,
    store_path: PathBuf,
    memory_root: PathBuf,
    delivery_lock: Arc<Mutex<()>>,
}

impl std::fmt::Debug for ThreadManagerOfficeRuntimeBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThreadManagerOfficeRuntimeBridge").finish()
    }
}

impl ThreadManagerOfficeRuntimeBridge {
    pub(crate) fn new(
        thread_manager: Arc<ThreadManager>,
        agent_config: Config,
        config: &OfficeWebConfig,
    ) -> Self {
        Self {
            agent_control: thread_manager.agent_control(),
            thread_manager,
            agent_config,
            runtime_store_path: config.runtime_store_path.clone(),
            store_path: config.store_path.clone(),
            memory_root: config.memory_root.clone(),
            delivery_lock: Arc::new(Mutex::new(())),
        }
    }

    async fn ensure_agent_thread(
        &self,
        agent_id: &str,
        parent_thread_id: Option<ThreadId>,
    ) -> Result<ThreadId, String> {
        let directory = PersistentPilotDirectory::open(self.store_path.clone())
            .map_err(|err| err.to_string())?;
        let owner_context = office_owner_context_for_agent(&directory, agent_id)
            .ok_or_else(|| format!("owner binding not found for `{agent_id}`"))?;
        self.initialize_agent_memory_and_runtime(&owner_context)?;
        if let Some(thread_id) = self.agent_control.thread_id_for_agent_name(agent_id) {
            return Ok(thread_id);
        }

        let mut agent_config = self.agent_config.clone();
        agent_config.developer_instructions = merge_office_developer_instructions(
            agent_config.developer_instructions,
            office_developer_instructions_for_agent(&directory, &owner_context),
        );
        let session_source = parent_thread_id
            .map(|parent_thread_id| {
                SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id,
                    depth: 1,
                    agent_type: Some(SpawnedAgentType::Worker),
                    agent_name_hint: Some(agent_id.to_string()),
                })
            })
            .unwrap_or(SessionSource::Exec);
        let thread_id = self
            .agent_control
            .spawn_agent(agent_config, Some(session_source))
            .await
            .map_err(|err| err.to_string())?;
        self.agent_control
            .register_agent_name(thread_id, agent_id)
            .map_err(|err| err.to_string())?;
        self.drain_thread_events_until_shutdown(thread_id, agent_id.to_string())
            .await?;
        Ok(thread_id)
    }

    fn owner_context(&self, agent_id: &str) -> Result<OfficeOwnerContext, String> {
        let directory = PersistentPilotDirectory::open(self.store_path.clone())
            .map_err(|err| err.to_string())?;
        office_owner_context_for_agent(&directory, agent_id)
            .ok_or_else(|| format!("owner binding not found for `{agent_id}`"))
    }

    fn initialize_agent_memory_and_runtime(
        &self,
        owner_context: &OfficeOwnerContext,
    ) -> Result<(), String> {
        AgentMemoryStore::new(self.memory_root.clone())
            .ensure_agent_files(&owner_context.agent_id, &owner_context.owner_user_id)
            .map_err(|err| err.to_string())?;
        let mut runtime_store = OfficeRuntimeStore::open(self.runtime_store_path.clone())
            .map_err(|err| err.to_string())?;
        runtime_store
            .ensure_agent(
                owner_context.agent_id.clone(),
                owner_context.owner_user_id.clone(),
            )
            .owner_user_id = owner_context.owner_user_id.clone();
        runtime_store.persist().map_err(|err| err.to_string())
    }

    async fn drain_thread_events_until_shutdown(
        &self,
        thread_id: ThreadId,
        agent_id: String,
    ) -> Result<(), String> {
        let thread = self
            .thread_manager
            .get_thread(thread_id)
            .await
            .map_err(|err| err.to_string())?;
        let runtime_store_path = self.runtime_store_path.clone();
        let agent_control = self.agent_control.clone();
        tokio::spawn(async move {
            while let Ok(event) = thread.next_event().await {
                record_runtime_event_from_codex(
                    &runtime_store_path,
                    &agent_control,
                    &agent_id,
                    &event.msg,
                );
                match &event.msg {
                    EventMsg::ItemCompleted(item) => {
                        if let TurnItem::AgentMessage(message) = &item.item {
                            let _text = message
                                .content
                                .iter()
                                .map(|content| match content {
                                    AgentMessageContent::Text { text } => text.as_str(),
                                })
                                .collect::<Vec<_>>()
                                .join("\n");
                        }
                    }
                    EventMsg::TurnComplete(turn) => {
                        let _ = turn.last_agent_message.as_deref();
                    }
                    EventMsg::ShutdownComplete => break,
                    _ => {}
                }
            }
        });
        Ok(())
    }
}

fn record_runtime_event_from_codex(
    runtime_store_path: &PathBuf,
    agent_control: &AgentControl,
    fallback_agent_id: &str,
    msg: &EventMsg,
) {
    let (thread_id, kind, source, title, summary, detail, message_id, tool_name) =
        match runtime_activity_from_event(msg) {
            Some(activity) => activity,
            None => return,
        };
    let agent_id = thread_id
        .and_then(|thread_id| agent_control.agent_name_for_thread(thread_id))
        .unwrap_or_else(|| fallback_agent_id.to_string());
    let Ok(mut runtime_store) = OfficeRuntimeStore::open(runtime_store_path.clone()) else {
        return;
    };
    let owner_user_id = runtime_store
        .get(&agent_id)
        .map(|record| record.owner_user_id.clone())
        .unwrap_or_else(|| "unknown_owner".to_string());
    let _ = runtime_store.record_runtime_event(
        &agent_id,
        &owner_user_id,
        kind,
        source,
        title,
        summary,
        detail,
        message_id,
        tool_name,
    );
}

#[allow(clippy::type_complexity)]
fn runtime_activity_from_event(
    msg: &EventMsg,
) -> Option<(
    Option<ThreadId>,
    OfficeActivityKind,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
)> {
    match msg {
        EventMsg::TurnStarted(turn) => Some((
            None,
            OfficeActivityKind::RuntimeTurnStarted,
            "runtime_event".to_string(),
            "Agent 开始处理".to_string(),
            format!("turn `{}` started", turn.turn_id),
            None,
            None,
            None,
        )),
        EventMsg::AgentReasoning(reasoning) => {
            let text = trimmed_preview(&reasoning.text, 1000)?;
            Some((
                None,
                OfficeActivityKind::RuntimeReasoning,
                "reasoning_summary".to_string(),
                "思考摘要".to_string(),
                trimmed_preview(&text, 160).unwrap_or_else(|| "Agent 更新了思考摘要".to_string()),
                Some(text),
                None,
                None,
            ))
        }
        EventMsg::AgentReasoningDelta(reasoning) => {
            let text = trimmed_preview(&reasoning.delta, 1000)?;
            Some((
                None,
                OfficeActivityKind::RuntimeReasoning,
                "reasoning_summary".to_string(),
                "思考摘要更新".to_string(),
                trimmed_preview(&text, 160).unwrap_or_else(|| "Agent 更新了思考摘要".to_string()),
                Some(text),
                None,
                None,
            ))
        }
        EventMsg::PlanUpdate(update) => {
            let mut lines = Vec::new();
            if let Some(explanation) = update.explanation.as_deref() {
                if !explanation.trim().is_empty() {
                    lines.push(explanation.trim().to_string());
                }
            }
            for item in &update.plan {
                lines.push(format!("- {:?}: {}", item.status, item.step.trim()));
            }
            let text = trimmed_preview(&lines.join("\n"), 2000)?;
            Some((
                None,
                OfficeActivityKind::RuntimePlan,
                "plan_update".to_string(),
                "计划更新".to_string(),
                trimmed_preview(&text, 160).unwrap_or_else(|| "Agent 更新了计划".to_string()),
                Some(text),
                None,
                None,
            ))
        }
        EventMsg::ItemCompleted(item) => match &item.item {
            TurnItem::Plan(plan) => {
                let text = trimmed_preview(&plan.text, 2000)?;
                Some((
                    Some(item.thread_id),
                    OfficeActivityKind::RuntimePlan,
                    "plan_item".to_string(),
                    "计划".to_string(),
                    trimmed_preview(&text, 160).unwrap_or_else(|| "Agent 生成了计划".to_string()),
                    Some(text),
                    None,
                    None,
                ))
            }
            TurnItem::Reasoning(reasoning) => {
                let text = trimmed_preview(&reasoning.summary_text.join("\n"), 2000)?;
                Some((
                    Some(item.thread_id),
                    OfficeActivityKind::RuntimeReasoning,
                    "reasoning_summary".to_string(),
                    "思考摘要".to_string(),
                    trimmed_preview(&text, 160)
                        .unwrap_or_else(|| "Agent 生成了思考摘要".to_string()),
                    Some(text),
                    None,
                    None,
                ))
            }
            TurnItem::AgentMessage(message) => {
                let text = agent_message_text(message)?;
                let message_id = extract_reply_to_message_id(&text);
                Some((
                    Some(item.thread_id),
                    OfficeActivityKind::RuntimeAgentMessage,
                    "assistant_message".to_string(),
                    "Agent 输出".to_string(),
                    trimmed_preview(&text, 160).unwrap_or_else(|| "Agent 输出了消息".to_string()),
                    Some(text),
                    message_id,
                    None,
                ))
            }
            TurnItem::WebSearch(search) => Some((
                Some(item.thread_id),
                OfficeActivityKind::RuntimeToolFinished,
                "web_search".to_string(),
                "网页搜索完成".to_string(),
                web_search_summary(&search.action),
                Some(web_search_summary(&search.action)),
                None,
                Some("web_search".to_string()),
            )),
            _ => None,
        },
        EventMsg::ExecCommandBegin(exec) => Some((
            None,
            OfficeActivityKind::RuntimeToolStarted,
            "exec".to_string(),
            "执行命令".to_string(),
            command_summary(&exec.command),
            Some(format!(
                "cwd: {}\ncommand: {}",
                exec.cwd.display(),
                command_summary(&exec.command)
            )),
            None,
            Some("exec".to_string()),
        )),
        EventMsg::ExecCommandEnd(exec) => {
            let status = match exec.status {
                ExecCommandStatus::Completed => "completed",
                ExecCommandStatus::Failed => "failed",
                ExecCommandStatus::Declined => "declined",
            };
            Some((
                None,
                OfficeActivityKind::RuntimeToolFinished,
                "exec".to_string(),
                "命令结束".to_string(),
                format!(
                    "{} ({status}, exit {})",
                    command_summary(&exec.command),
                    exec.exit_code
                ),
                Some(trim_tool_output(
                    &exec.aggregated_output,
                    &exec.stdout,
                    &exec.stderr,
                )),
                None,
                Some("exec".to_string()),
            ))
        }
        EventMsg::PatchApplyBegin(_) => Some((
            None,
            OfficeActivityKind::RuntimeToolStarted,
            "apply_patch".to_string(),
            "准备应用代码改动".to_string(),
            "Agent 正在应用代码改动".to_string(),
            None,
            None,
            Some("apply_patch".to_string()),
        )),
        EventMsg::PatchApplyEnd(patch) => {
            let status = match patch.status {
                PatchApplyStatus::Completed => "completed",
                PatchApplyStatus::Failed => "failed",
                PatchApplyStatus::Declined => "declined",
            };
            Some((
                None,
                OfficeActivityKind::RuntimeToolFinished,
                "apply_patch".to_string(),
                "代码改动结束".to_string(),
                format!("apply_patch {status}"),
                Some(trim_tool_output("", &patch.stdout, &patch.stderr)),
                None,
                Some("apply_patch".to_string()),
            ))
        }
        EventMsg::McpToolCallBegin(mcp) => Some((
            None,
            OfficeActivityKind::RuntimeToolStarted,
            "mcp".to_string(),
            "调用 MCP 工具".to_string(),
            format!("{}.{}", mcp.invocation.server, mcp.invocation.tool),
            mcp.invocation
                .arguments
                .as_ref()
                .map(|value| value.to_string()),
            None,
            Some(format!("{}.{}", mcp.invocation.server, mcp.invocation.tool)),
        )),
        EventMsg::McpToolCallEnd(mcp) => Some((
            None,
            OfficeActivityKind::RuntimeToolFinished,
            "mcp".to_string(),
            "MCP 工具结束".to_string(),
            format!(
                "{}.{} {}",
                mcp.invocation.server,
                mcp.invocation.tool,
                if mcp.is_success() {
                    "completed"
                } else {
                    "failed"
                }
            ),
            Some(format!("{:?}", mcp.result)),
            None,
            Some(format!("{}.{}", mcp.invocation.server, mcp.invocation.tool)),
        )),
        EventMsg::WebSearchBegin(_) => Some((
            None,
            OfficeActivityKind::RuntimeToolStarted,
            "web_search".to_string(),
            "开始网页搜索".to_string(),
            "Agent 正在进行网页搜索".to_string(),
            None,
            None,
            Some("web_search".to_string()),
        )),
        EventMsg::WebSearchEnd(search) => Some((
            None,
            OfficeActivityKind::RuntimeToolFinished,
            "web_search".to_string(),
            "网页搜索完成".to_string(),
            web_search_summary(&search.action),
            Some(web_search_summary(&search.action)),
            None,
            Some("web_search".to_string()),
        )),
        EventMsg::CollabAgentSpawnEnd(ev) => Some((
            Some(ev.sender_thread_id),
            OfficeActivityKind::RuntimeCollab,
            "collab".to_string(),
            "创建协作 Agent".to_string(),
            format!(
                "spawn `{}` -> {}",
                ev.call_id,
                ev.new_thread_id
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "not created".to_string())
            ),
            Some(format!(
                "status: {:?}\nprompt: {}",
                ev.status,
                trimmed_preview(&ev.prompt, 1000).unwrap_or_default()
            )),
            None,
            Some("spawn_agent".to_string()),
        )),
        EventMsg::CollabAgentInteractionEnd(ev) => Some((
            Some(ev.sender_thread_id),
            OfficeActivityKind::RuntimeCollab,
            "collab".to_string(),
            "发送给协作 Agent".to_string(),
            format!("sent `{}` to {}", ev.call_id, ev.receiver_thread_id),
            Some(format!(
                "receiver: {}\nstatus: {:?}\nprompt: {}",
                ev.receiver_thread_id,
                ev.status,
                trimmed_preview(&ev.prompt, 1000).unwrap_or_default()
            )),
            None,
            Some("call".to_string()),
        )),
        EventMsg::CollabWaitingBegin(ev) => Some((
            Some(ev.sender_thread_id),
            OfficeActivityKind::RuntimeWait,
            "collab_wait".to_string(),
            "等待协作 Agent".to_string(),
            format!("waiting for {}", wait_targets_label(&ev.targets)),
            None,
            None,
            Some("wait".to_string()),
        )),
        EventMsg::CollabWaitingEnd(ev) => Some((
            Some(ev.sender_thread_id),
            OfficeActivityKind::RuntimeCollab,
            "collab_wait".to_string(),
            "等待结束".to_string(),
            format!(
                "{} for {}",
                if ev.timed_out {
                    "timed out"
                } else {
                    "completed"
                },
                wait_targets_label(&ev.targets)
            ),
            Some(wait_targets_detail(&ev.targets)),
            None,
            Some("wait".to_string()),
        )),
        EventMsg::CollabResumeBegin(ev) => Some((
            Some(ev.sender_thread_id),
            OfficeActivityKind::RuntimeCollab,
            "collab".to_string(),
            "恢复协作 Agent".to_string(),
            format!("resuming {}", ev.receiver_thread_id),
            None,
            None,
            Some("resume_agent".to_string()),
        )),
        EventMsg::CollabResumeEnd(ev) => Some((
            Some(ev.sender_thread_id),
            OfficeActivityKind::RuntimeCollab,
            "collab".to_string(),
            "协作 Agent 已恢复".to_string(),
            format!("{} status {:?}", ev.receiver_thread_id, ev.status),
            None,
            None,
            Some("resume_agent".to_string()),
        )),
        EventMsg::AgentWorkSummary(work) => Some((
            Some(work.thread_id),
            OfficeActivityKind::RuntimeAgentMessage,
            "work_summary".to_string(),
            "工作摘要".to_string(),
            trimmed_preview(&work.summary, 160)
                .unwrap_or_else(|| "Agent 更新了工作摘要".to_string()),
            Some(work.summary.clone()),
            None,
            None,
        )),
        EventMsg::Warning(warning) => Some((
            None,
            OfficeActivityKind::RuntimeError,
            "warning".to_string(),
            "运行警告".to_string(),
            warning.message.clone(),
            Some(warning.message.clone()),
            None,
            None,
        )),
        EventMsg::Error(error) => Some((
            None,
            OfficeActivityKind::RuntimeError,
            "error".to_string(),
            "运行错误".to_string(),
            error.message.clone(),
            Some(error.message.clone()),
            None,
            None,
        )),
        _ => None,
    }
}

fn agent_message_text(message: &codex_protocol::items::AgentMessageItem) -> Option<String> {
    trimmed_preview(
        &message
            .content
            .iter()
            .map(|content| match content {
                AgentMessageContent::Text { text } => text.as_str(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        4000,
    )
}

fn trimmed_preview(value: &str, max_chars: usize) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut out = trimmed.chars().take(max_chars).collect::<String>();
    if trimmed.chars().count() > max_chars {
        out.push_str("...");
    }
    Some(out)
}

fn command_summary(command: &[String]) -> String {
    if command.is_empty() {
        return "empty command".to_string();
    }
    trimmed_preview(&command.join(" "), 240).unwrap_or_else(|| "command".to_string())
}

fn trim_tool_output(aggregated: &str, stdout: &str, stderr: &str) -> String {
    let mut parts = Vec::new();
    if let Some(value) = trimmed_preview(aggregated, 4000) {
        parts.push(value);
    }
    if parts.is_empty() {
        if let Some(value) = trimmed_preview(stdout, 3000) {
            parts.push(format!("stdout:\n{value}"));
        }
    }
    if let Some(value) = trimmed_preview(stderr, 2000) {
        parts.push(format!("stderr:\n{value}"));
    }
    if parts.is_empty() {
        "no output".to_string()
    } else {
        parts.join("\n\n")
    }
}

fn web_search_summary(action: &WebSearchAction) -> String {
    match action {
        WebSearchAction::Search { query, queries } => query
            .clone()
            .or_else(|| queries.as_ref().map(|values| values.join(", ")))
            .map(|value| format!("search: {value}"))
            .unwrap_or_else(|| "search completed".to_string()),
        WebSearchAction::OpenPage { url } => url
            .as_ref()
            .map(|url| format!("open: {url}"))
            .unwrap_or_else(|| "page opened".to_string()),
        WebSearchAction::FindInPage { url, pattern } => {
            format!(
                "find `{}` in {}",
                pattern.as_deref().unwrap_or(""),
                url.as_deref().unwrap_or("page")
            )
        }
        WebSearchAction::Other => "web action completed".to_string(),
    }
}

fn wait_targets_label(targets: &[codex_protocol::protocol::CollabWaitTargetEvent]) -> String {
    if targets.is_empty() {
        return "next inbox message".to_string();
    }
    targets
        .iter()
        .map(|target| {
            if target.receiver_agent_name.trim().is_empty() {
                target.receiver_thread_id.to_string()
            } else {
                target.receiver_agent_name.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn wait_targets_detail(targets: &[codex_protocol::protocol::CollabWaitTargetEvent]) -> String {
    if targets.is_empty() {
        return "targets: any inbox message".to_string();
    }
    targets
        .iter()
        .map(|target| {
            format!(
                "{}: {:?}",
                if target.receiver_agent_name.trim().is_empty() {
                    target.receiver_thread_id.to_string()
                } else {
                    target.receiver_agent_name.clone()
                },
                target.state
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[async_trait]
impl OfficeRuntimeBridge for ThreadManagerOfficeRuntimeBridge {
    async fn deliver_owner_message(
        &self,
        delivery: OfficeOwnerDelivery,
    ) -> Result<Option<ThreadId>, String> {
        let _guard = self.delivery_lock.lock().await;
        let parent_thread_id = if delivery.agent_id == OFFICE_CEO_AGENT_ID {
            None
        } else {
            self.agent_control
                .thread_id_for_agent_name(OFFICE_CEO_AGENT_ID)
        };
        let thread_id = self
            .ensure_agent_thread(&delivery.agent_id, parent_thread_id)
            .await?;
        if delivery.agent_id == OFFICE_CEO_AGENT_ID {
            self.ensure_fixed_roster_threads(thread_id).await?;
        }
        let directory = PersistentPilotDirectory::open(self.store_path.clone())
            .map_err(|err| err.to_string())?;
        self.agent_control
            .send_input(
                thread_id,
                owner_delivery_user_input_with_directory(&delivery, Some(&directory)),
            )
            .await
            .map(|_| Some(thread_id))
            .map_err(|err| err.to_string())
    }

    fn thread_id_for_agent(&self, agent_id: &str) -> Option<ThreadId> {
        self.agent_control.thread_id_for_agent_name(agent_id)
    }
}

impl ThreadManagerOfficeRuntimeBridge {
    async fn ensure_fixed_roster_threads(&self, ceo_thread_id: ThreadId) -> Result<(), String> {
        for agent_id in OFFICE_FIXED_AGENT_IDS {
            self.ensure_agent_thread(agent_id, Some(ceo_thread_id))
                .await?;
        }
        Ok(())
    }
}

fn owner_delivery_user_input(delivery: &OfficeOwnerDelivery) -> Vec<UserInput> {
    owner_delivery_user_input_with_directory(delivery, None)
}

fn owner_delivery_user_input_with_directory(
    delivery: &OfficeOwnerDelivery,
    _directory: Option<&PersistentPilotDirectory>,
) -> Vec<UserInput> {
    let reply_to_message_id = delivery.reply_to_message_id.as_deref().unwrap_or("none");
    let owner_reply_target_message_id = delivery
        .owner_reply_target_message_id
        .as_deref()
        .or_else(|| delivery.need_reply.then_some(delivery.message_id.as_str()));
    let mut text = format!(
        "office owner message from [{}],reply_to[{}],message_id[{}],need_reply[{}]\n\n{}",
        delivery.owner_user_id,
        reply_to_message_id,
        delivery.message_id,
        delivery.need_reply,
        delivery.content.trim_end()
    );
    text.push_str(&render_owner_turn_context(
        &delivery.agent_id,
        &delivery.owner_user_id,
        &delivery.message_id,
        reply_to_message_id,
        delivery.need_reply,
        owner_reply_target_message_id,
    ));
    vec![UserInput::Text {
        text,
        text_elements: Vec::new(),
    }]
}

fn office_developer_instructions_for_agent(
    directory: &PersistentPilotDirectory,
    owner_context: &OfficeOwnerContext,
) -> String {
    let human_participants = render_human_participants_from_directory(directory)
        .unwrap_or_else(render_default_human_participants);
    render_office_agent_developer_instructions(
        &owner_context.to_prompt_context(),
        &human_participants,
    )
}

fn office_owner_context_for_agent(
    directory: &PersistentPilotDirectory,
    agent_id: &str,
) -> Option<OfficeOwnerContext> {
    let binding = directory.active_owner_binding(agent_id)?.clone();
    Some(OfficeOwnerContext {
        agent_id: agent_id.to_string(),
        owner_user_id: binding.owner_user_id.clone(),
        binding_version: binding.version,
        binding,
    })
}

fn render_default_human_participants() -> String {
    [
        ("user_ceo", "agent_ceo"),
        ("user_a", "agent_a"),
        ("user_b", "agent_b"),
        ("user_c", "agent_c"),
        ("user_d", "agent_d"),
        ("user_e", "agent_e"),
    ]
    .iter()
    .map(|(user_id, agent_id)| format!("- {user_id}: owner of {agent_id}.\n"))
    .collect::<String>()
}

fn render_human_participants_from_directory(
    directory: &PersistentPilotDirectory,
) -> Option<String> {
    let mut entries = Vec::new();
    for agent_id in OFFICE_FIXED_AGENT_IDS {
        let context = office_owner_context_for_agent(directory, agent_id)?;
        entries.push(format!(
            "- {}: owner of {}; binding_version: {}.\n",
            context.owner_user_id, context.agent_id, context.binding_version
        ));
    }
    Some(entries.join(""))
}

fn extract_reply_to_message_id(message: &str) -> Option<String> {
    for line in message.lines().take(12) {
        let trimmed = line.trim();
        if let Some(value) = trimmed.strip_prefix("- `reply_to`:") {
            let value = value.trim().trim_matches('`').trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
        if let Some(value) = trimmed.strip_prefix("reply_to:") {
            let value = value.trim().trim_matches('`').trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OfficeAgentExternalState {
    Idle,
    Working,
    Waiting,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OfficeAgentStateView {
    pub state: OfficeAgentExternalState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OfficeInboxMessage {
    pub message_id: String,
    pub from: String,
    pub to: String,
    pub content: String,
    pub need_reply: bool,
    #[serde(default)]
    pub reply_status: OfficeInboxReplyStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to_message_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OfficeInboxReplyStatus {
    #[default]
    NotRequired,
    Pending,
    Replied,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OfficeAgentInboxView {
    pub agent_id: String,
    pub queued_count: usize,
    pub messages: Vec<OfficeInboxMessage>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OfficeAgentActivityView {
    pub agent_id: String,
    pub entries: Vec<OfficeActivityEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OfficeMeResponse {
    pub username: String,
    pub user_id: String,
    pub agent_id: String,
    pub human_profile: HumanProfile,
    pub agent_profile: AgentProfile,
    pub owner_binding: AgentOwnerBinding,
    pub agent_state: OfficeAgentStateView,
    pub agent_activity: OfficeAgentActivityView,
    pub pending_owner_replies: Vec<OfficeInboxMessage>,
    pub agent_inbox: OfficeAgentInboxView,
    pub human_inbox: OfficeAgentInboxView,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OfficeLoginResponse {
    pub session_token: String,
    pub me: OfficeMeResponse,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OfficeInterviewResponse {
    pub human_profile: HumanProfile,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OfficeReflectionResponse {
    pub reflection: DailyReflection,
    pub human_profile: HumanProfile,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OfficeOwnerMessageResponse {
    pub queued_to_agent_id: String,
    pub message: OfficeInboxMessage,
    pub learned_profile_facts: Vec<super::pilot::UserProfileFact>,
    pub me: OfficeMeResponse,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OfficeOwnerProfileSyncResponse {
    pub synced_agent_id: String,
    pub owner_user_id: String,
    pub owner_profile_path: String,
    pub me: OfficeMeResponse,
}

#[derive(Debug, Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

#[derive(Debug, Deserialize)]
struct InterviewRequest {
    role: String,
    #[serde(default)]
    capabilities: Vec<String>,
    #[serde(default)]
    avoid: Vec<String>,
    #[serde(default)]
    report_preference: String,
}

#[derive(Debug, Deserialize)]
struct ReflectionRequest {
    #[serde(default)]
    profile_updates: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct OwnerMessageRequest {
    #[serde(default)]
    message_id: Option<String>,
    content: String,
    #[serde(default, deserialize_with = "deserialize_form_compatible_bool")]
    need_reply: bool,
    #[serde(default)]
    reply_to_message_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
}

impl OfficeWebApp {
    pub fn open(config: OfficeWebConfig) -> Result<Self, OfficeWebError> {
        Self::open_with_runtime_bridge(config, None)
    }

    pub fn open_with_runtime_bridge(
        config: OfficeWebConfig,
        runtime_bridge: Option<Arc<dyn OfficeRuntimeBridge>>,
    ) -> Result<Self, OfficeWebError> {
        collab_inbox::configure_store_path(config.collab_inbox_path.clone());
        let directory = PersistentPilotDirectory::open(config.store_path.clone())?;
        let runtime_store = OfficeRuntimeStore::open(config.runtime_store_path.clone())?;
        let blackboard_store = OfficeBlackboardStore::open(config.blackboard_path.clone())?;
        let memory_store = AgentMemoryStore::new(config.memory_root.clone());
        Ok(Self {
            state: Arc::new(OfficeWebState {
                config: config.clone(),
                directory: Mutex::new(directory),
                runtime_store: Mutex::new(runtime_store),
                _blackboard_store: Mutex::new(blackboard_store),
                memory_store,
                runtime_bridge,
            }),
            config,
        })
    }

    pub(crate) fn open_with_agent_control(
        config: OfficeWebConfig,
        agent_control: AgentControl,
    ) -> Result<Self, OfficeWebError> {
        Self::open_with_runtime_bridge(
            config,
            Some(Arc::new(AgentControlOfficeRuntimeBridge::new(
                agent_control,
            ))),
        )
    }

    pub fn open_with_thread_manager(
        config: OfficeWebConfig,
        thread_manager: Arc<ThreadManager>,
        agent_config: Config,
    ) -> Result<Self, OfficeWebError> {
        let runtime_bridge =
            ThreadManagerOfficeRuntimeBridge::new(thread_manager, agent_config, &config);
        Self::open_with_runtime_bridge(config, Some(Arc::new(runtime_bridge)))
    }

    pub fn new(config: OfficeWebConfig) -> Result<Self, OfficeWebError> {
        Self::open(config)
    }

    pub fn config(&self) -> &OfficeWebConfig {
        &self.config
    }

    pub async fn agent_context_snapshot(
        &self,
        agent_id: &str,
    ) -> Option<OfficeAgentContextSnapshot> {
        let directory = self.state.directory.lock().await;
        let agent_profile = directory.agent_profile(agent_id)?.clone();
        let memory_paths = self.state.memory_store.paths_for(agent_id);
        let runtime_store = self.state.runtime_store.lock().await;
        let runtime = runtime_store.get(agent_id).cloned();
        let blackboard_store = self.state._blackboard_store.lock().await;
        Some(OfficeAgentContextSnapshot::new(
            agent_id.to_string(),
            agent_profile.owner_user_id,
            runtime,
            collab_inbox::messages_for_logical(agent_id).len(),
            memory_paths,
            blackboard_store.blackboard().clone(),
        ))
    }

    pub fn into_router(self) -> Router {
        Router::new()
            .route("/healthz", get(healthz))
            .route("/", get(login_page))
            .route("/login", get(login_page).post(login))
            .route("/api/login", post(login))
            .route("/me", get(me_page))
            .route("/api/me", get(me_json))
            .route("/api/inbox", post(owner_message))
            .route("/interview", get(interview_page).post(interview))
            .route("/api/interview", post(interview))
            .route("/api/reflection", post(reflection))
            .route("/api/owner-profile/sync", post(sync_owner_profile))
            .route("/blackboard", get(blackboard_page))
            .route(
                "/api/blackboard",
                get(blackboard_json).post(blackboard_post),
            )
            .route("/logout", post(logout))
            .route("/api/logout", post(logout))
            .with_state(self.state)
    }

    pub async fn serve(self, addr: SocketAddr) -> Result<(), OfficeWebError> {
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|source| OfficeWebError::Bind { addr, source })?;
        axum::serve(listener, self.into_router())
            .await
            .map_err(OfficeWebError::Serve)
    }
}

async fn healthz() -> &'static str {
    "ok"
}

async fn login_page() -> Html<String> {
    Html(render_login_page(None))
}

async fn login(
    State(state): State<Arc<OfficeWebState>>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    let wants_json = wants_json(&headers);
    let request = match parse_request::<LoginRequest>(&headers, &body) {
        Ok(request) => request,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, message, wants_json),
    };

    let mut directory = state.directory.lock().await;
    let Some(session) = directory.login(request.username.trim(), request.password.as_str()) else {
        if wants_json {
            return error_response(
                StatusCode::UNAUTHORIZED,
                "invalid username or password",
                true,
            );
        }
        return (
            StatusCode::UNAUTHORIZED,
            Html(render_login_page(Some("用户名或密码不正确"))),
        )
            .into_response();
    };

    let token = Uuid::new_v4().to_string();
    let web_session = match directory.create_web_session(token.clone(), session) {
        Ok(session) => session,
        Err(err) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to create session: {err}"),
                wants_json,
            );
        }
    };
    if register_session_participants(&directory, &web_session).is_none() {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "session profile is inconsistent",
            wants_json,
        );
    }
    if let Err(err) = state
        .memory_store
        .ensure_agent_files(&web_session.agent_id, &web_session.user_id)
    {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to initialize agent memory: {err}"),
            wants_json,
        );
    }
    let mut runtime_store = state.runtime_store.lock().await;
    let Some(me) = build_me(&directory, &mut runtime_store, &web_session) else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "session profile is inconsistent",
            wants_json,
        );
    };
    drop(directory);

    let cookie = session_cookie(&state.config.session_cookie, &token);
    if wants_json {
        let mut response = (
            StatusCode::OK,
            Json(OfficeLoginResponse {
                session_token: token,
                me,
            }),
        )
            .into_response();
        response.headers_mut().insert(
            SET_COOKIE,
            cookie
                .parse()
                .expect("session cookie value is a valid header"),
        );
        response
    } else {
        let mut response = (StatusCode::SEE_OTHER, [(LOCATION, "/me")]).into_response();
        response.headers_mut().insert(
            SET_COOKIE,
            cookie
                .parse()
                .expect("session cookie value is a valid header"),
        );
        response
    }
}

async fn me_json(
    State(state): State<Arc<OfficeWebState>>,
    headers: HeaderMap,
) -> axum::response::Response {
    let Some(token) = session_token(&state.config.session_cookie, &headers) else {
        return error_response(StatusCode::UNAUTHORIZED, "not logged in", true);
    };
    let directory = state.directory.lock().await;
    let Some(web_session) = directory.web_session(&token) else {
        return error_response(StatusCode::UNAUTHORIZED, "not logged in", true);
    };
    let web_session = web_session.clone();
    if register_session_participants(&directory, &web_session).is_none() {
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, "profile not found", true);
    }
    collab_inbox::reload_inbox_from_disk();
    let mut runtime_store = state.runtime_store.lock().await;
    if let Err(err) = runtime_store.reload() {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to reload office runtime store: {err}"),
            true,
        );
    }
    let Some(me) = build_me(&directory, &mut runtime_store, &web_session) else {
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, "profile not found", true);
    };
    (StatusCode::OK, Json(me)).into_response()
}

async fn me_page(
    State(state): State<Arc<OfficeWebState>>,
    headers: HeaderMap,
) -> axum::response::Response {
    let Some(token) = session_token(&state.config.session_cookie, &headers) else {
        return redirect_to_login().into_response();
    };
    let directory = state.directory.lock().await;
    let Some(web_session) = directory.web_session(&token) else {
        return redirect_to_login().into_response();
    };
    let web_session = web_session.clone();
    if register_session_participants(&directory, &web_session).is_none() {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "profile not found",
            false,
        );
    }
    collab_inbox::reload_inbox_from_disk();
    let mut runtime_store = state.runtime_store.lock().await;
    if let Err(err) = runtime_store.reload() {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to reload office runtime store: {err}"),
            false,
        );
    }
    let Some(me) = build_me(&directory, &mut runtime_store, &web_session) else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "profile not found",
            false,
        );
    };
    Html(render_dashboard(&me)).into_response()
}

async fn interview_page(
    State(state): State<Arc<OfficeWebState>>,
    headers: HeaderMap,
) -> axum::response::Response {
    let Some(token) = session_token(&state.config.session_cookie, &headers) else {
        return redirect_to_login().into_response();
    };
    let directory = state.directory.lock().await;
    if directory.web_session(&token).is_none() {
        return redirect_to_login().into_response();
    }
    Html(render_interview_page()).into_response()
}

async fn interview(
    State(state): State<Arc<OfficeWebState>>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    let wants_json = wants_json(&headers);
    let Some(token) = session_token(&state.config.session_cookie, &headers) else {
        return error_response(StatusCode::UNAUTHORIZED, "not logged in", wants_json);
    };
    let request = match parse_request::<InterviewRequest>(&headers, &body) {
        Ok(request) => request,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, message, wants_json),
    };

    let mut directory = state.directory.lock().await;
    let Some(user_id) = directory
        .web_session(&token)
        .map(|session| session.user_id.clone())
    else {
        return error_response(StatusCode::UNAUTHORIZED, "not logged in", wants_json);
    };
    let report_preference = if request.report_preference.trim().is_empty() {
        "先结论后细节".to_string()
    } else {
        request.report_preference
    };
    let profile = match directory.run_mini_interview(
        &user_id,
        request.role,
        request.capabilities,
        request.avoid,
        report_preference,
    ) {
        Ok(Some(profile)) => profile,
        Ok(None) => return error_response(StatusCode::NOT_FOUND, "profile not found", wants_json),
        Err(err) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to save interview: {err}"),
                wants_json,
            );
        }
    };
    if wants_json {
        (
            StatusCode::OK,
            Json(OfficeInterviewResponse {
                human_profile: profile,
            }),
        )
            .into_response()
    } else {
        (StatusCode::SEE_OTHER, [(LOCATION, "/me")]).into_response()
    }
}

async fn reflection(
    State(state): State<Arc<OfficeWebState>>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    let wants_json = wants_json(&headers);
    let Some(token) = session_token(&state.config.session_cookie, &headers) else {
        return error_response(StatusCode::UNAUTHORIZED, "not logged in", wants_json);
    };
    let request = match parse_request::<ReflectionRequest>(&headers, &body) {
        Ok(request) => request,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, message, wants_json),
    };

    let mut directory = state.directory.lock().await;
    let Some(user_id) = directory
        .web_session(&token)
        .map(|session| session.user_id.clone())
    else {
        return error_response(StatusCode::UNAUTHORIZED, "not logged in", wants_json);
    };
    let reflection = match directory.daily_reflection(&user_id, request.profile_updates) {
        Ok(Some(reflection)) => reflection,
        Ok(None) => return error_response(StatusCode::NOT_FOUND, "profile not found", wants_json),
        Err(err) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to save reflection: {err}"),
                wants_json,
            );
        }
    };
    let human_profile = directory
        .human_profile(&user_id)
        .expect("reflection user profile exists")
        .clone();
    if wants_json {
        (
            StatusCode::OK,
            Json(OfficeReflectionResponse {
                reflection,
                human_profile,
            }),
        )
            .into_response()
    } else {
        (StatusCode::SEE_OTHER, [(LOCATION, "/me")]).into_response()
    }
}

async fn owner_message(
    State(state): State<Arc<OfficeWebState>>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    let wants_json = wants_json(&headers);
    let Some(token) = session_token(&state.config.session_cookie, &headers) else {
        return error_response(StatusCode::UNAUTHORIZED, "not logged in", wants_json);
    };
    let request = match parse_request::<OwnerMessageRequest>(&headers, &body) {
        Ok(request) => request,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, message, wants_json),
    };
    let content = request.content.trim();
    if content.is_empty() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "message content is required",
            wants_json,
        );
    }

    let mut directory = state.directory.lock().await;
    let Some(web_session) = directory.web_session(&token) else {
        return error_response(StatusCode::UNAUTHORIZED, "not logged in", wants_json);
    };
    let web_session = web_session.clone();

    if register_session_participants(&directory, &web_session).is_none() {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "profile not found",
            wants_json,
        );
    }
    if let Err(err) = state
        .memory_store
        .ensure_agent_files(&web_session.agent_id, &web_session.user_id)
    {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to initialize agent memory: {err}"),
            wants_json,
        );
    }

    let message_id = request
        .message_id
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let reply_to_message_id = request
        .reply_to_message_id
        .filter(|value| !value.trim().is_empty());
    let need_reply = request.need_reply || (!wants_json && reply_to_message_id.is_none());
    let learned_profile_facts = match directory.record_owner_message_profile_evidence(
        &web_session.user_id,
        &web_session.agent_id,
        &message_id,
        content,
    ) {
        Ok(Some(facts)) => facts,
        Ok(None) => return error_response(StatusCode::NOT_FOUND, "profile not found", wants_json),
        Err(err) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to update owner profile: {err}"),
                wants_json,
            );
        }
    };
    collab_inbox::append_logical_message(
        &web_session.agent_id,
        web_session.user_id.clone(),
        message_id.clone(),
        need_reply,
        reply_to_message_id.clone(),
        content.to_string(),
    );
    let owner_reply_target_message_id = {
        let mut runtime_store = state.runtime_store.lock().await;
        if let Err(err) = runtime_store.reload() {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to reload office runtime store: {err}"),
                wants_json,
            );
        }
        let owner_reply_target_message_id = if need_reply {
            Some(message_id.clone())
        } else {
            runtime_store.latest_unanswered_owner_message_id(&web_session.agent_id)
        };
        if need_reply {
            if let Err(err) = runtime_store.mark_owner_message_queued(
                &web_session.agent_id,
                &web_session.user_id,
                &message_id,
                content,
            ) {
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("failed to update office runtime store: {err}"),
                    wants_json,
                );
            }
        }
        owner_reply_target_message_id
    };
    let mut owner_reply_resolved = false;
    let mut resumed_thread_id = None;
    if let Some(runtime_bridge) = state.runtime_bridge.as_ref() {
        let existing_target_thread_id = runtime_bridge.thread_id_for_agent(&web_session.agent_id);
        match runtime_bridge
            .deliver_owner_message(OfficeOwnerDelivery {
                agent_id: web_session.agent_id.clone(),
                owner_user_id: web_session.user_id.clone(),
                message_id: message_id.clone(),
                owner_reply_target_message_id: owner_reply_target_message_id.clone(),
                content: content.to_string(),
                need_reply,
                reply_to_message_id: reply_to_message_id.clone(),
            })
            .await
        {
            Ok(thread_id) => {
                resumed_thread_id = thread_id;
                if let Some(target_thread_id) = existing_target_thread_id {
                    if let Some(reply_to_message_id) = reply_to_message_id.as_deref() {
                        owner_reply_resolved =
                            collab_inbox::resolve_required_reply_logical_from_source_id(
                                &web_session.user_id,
                                &target_thread_id.to_string(),
                                reply_to_message_id,
                            );
                    }
                    collab_inbox::append_message(
                        target_thread_id,
                        web_session.user_id.clone(),
                        message_id.clone(),
                        reply_to_message_id.clone(),
                        content.to_string(),
                    );
                }
            }
            Err(err) => {
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("failed to deliver owner message to runtime: {err}"),
                    wants_json,
                );
            }
        }
    }
    if !owner_reply_resolved {
        if let Some(reply_to_message_id) = reply_to_message_id.as_deref() {
            let _ = collab_inbox::resolve_required_reply_logical_any_source(
                &web_session.user_id,
                reply_to_message_id,
            );
        }
    }
    if let Some(thread_id) = resumed_thread_id {
        let mut runtime_store = state.runtime_store.lock().await;
        if let Err(err) = runtime_store.reload() {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to reload office runtime store: {err}"),
                wants_json,
            );
        }
        if let Err(err) =
            runtime_store.bind_thread(&web_session.agent_id, &web_session.user_id, thread_id)
        {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to bind office runtime thread: {err}"),
                wants_json,
            );
        }
    }
    let message_view = OfficeInboxMessage {
        message_id,
        from: web_session.user_id.clone(),
        to: web_session.agent_id.clone(),
        content: content.to_string(),
        need_reply,
        reply_status: reply_status_from_need_reply(need_reply),
        reply_to_message_id,
    };
    let mut runtime_store = state.runtime_store.lock().await;
    if let Err(err) = runtime_store.reload() {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to reload office runtime store: {err}"),
            wants_json,
        );
    }
    let Some(me) = build_me(&directory, &mut runtime_store, &web_session) else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "profile not found",
            wants_json,
        );
    };

    if wants_json {
        (
            StatusCode::OK,
            Json(OfficeOwnerMessageResponse {
                queued_to_agent_id: web_session.agent_id,
                message: message_view,
                learned_profile_facts,
                me,
            }),
        )
            .into_response()
    } else {
        (StatusCode::SEE_OTHER, [(LOCATION, "/me")]).into_response()
    }
}

async fn sync_owner_profile(
    State(state): State<Arc<OfficeWebState>>,
    headers: HeaderMap,
) -> axum::response::Response {
    let wants_json = wants_json(&headers);
    let Some(token) = session_token(&state.config.session_cookie, &headers) else {
        return error_response(StatusCode::UNAUTHORIZED, "not logged in", wants_json);
    };

    let directory = state.directory.lock().await;
    let Some(web_session) = directory.web_session(&token) else {
        return error_response(StatusCode::UNAUTHORIZED, "not logged in", wants_json);
    };
    let web_session = web_session.clone();
    if register_session_participants(&directory, &web_session).is_none() {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "profile not found",
            wants_json,
        );
    }
    let owner_profile_path = match sync_owner_profile_prompt_for_session(
        &state.config,
        &directory,
        &web_session,
    )
    .await
    {
        Ok(path) => path,
        Err(err) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to sync owner profile prompt: {err}"),
                wants_json,
            );
        }
    };

    let mut runtime_store = state.runtime_store.lock().await;
    let Some(me) = build_me(&directory, &mut runtime_store, &web_session) else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "profile not found",
            wants_json,
        );
    };

    if wants_json {
        (
            StatusCode::OK,
            Json(OfficeOwnerProfileSyncResponse {
                synced_agent_id: web_session.agent_id,
                owner_user_id: web_session.user_id,
                owner_profile_path: owner_profile_path.display().to_string(),
                me,
            }),
        )
            .into_response()
    } else {
        (StatusCode::SEE_OTHER, [(LOCATION, "/me")]).into_response()
    }
}

async fn logout(
    State(state): State<Arc<OfficeWebState>>,
    headers: HeaderMap,
) -> axum::response::Response {
    if let Some(token) = session_token(&state.config.session_cookie, &headers) {
        let mut directory = state.directory.lock().await;
        if let Err(err) = directory.delete_web_session(&token) {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to delete session: {err}"),
                wants_json(&headers),
            );
        }
    }
    let expired = expire_session_cookie(&state.config.session_cookie);
    let mut response = if wants_json(&headers) {
        (StatusCode::OK, Json(serde_json::json!({ "ok": true }))).into_response()
    } else {
        (StatusCode::SEE_OTHER, [(LOCATION, "/login")]).into_response()
    };
    response.headers_mut().insert(
        SET_COOKIE,
        expired.parse().expect("expired cookie value is valid"),
    );
    response
}

fn build_me(
    directory: &PersistentPilotDirectory,
    runtime_store: &mut OfficeRuntimeStore,
    session: &PilotWebSession,
) -> Option<OfficeMeResponse> {
    let account = directory.account_for_user(&session.user_id)?;
    if account.agent_id != session.agent_id {
        return None;
    }
    let human_profile = directory.human_profile(&session.user_id)?.clone();
    let agent_profile = directory.agent_profile(&session.agent_id)?.clone();
    if agent_profile.owner_user_id != session.user_id {
        return None;
    }
    let owner_binding = directory.active_owner_binding(&session.agent_id)?.clone();
    runtime_store.ensure_agent(
        session.agent_id.clone(),
        owner_binding.owner_user_id.clone(),
    );
    let _ = runtime_store.persist();
    sync_owner_reply_ready_from_human_inboxes(runtime_store, session);
    let agent_inbox = build_agent_inbox(&session.agent_id);
    let human_inbox = build_human_inbox(&session.user_id);
    let pending_owner_replies = agent_inbox
        .messages
        .iter()
        .filter(|message| {
            message.need_reply
                && message.from == session.user_id
                && !runtime_store.has_owner_reply_ready(&session.agent_id, &message.message_id)
        })
        .cloned()
        .collect();
    Some(OfficeMeResponse {
        username: account.username.clone(),
        user_id: session.user_id.clone(),
        agent_id: session.agent_id.clone(),
        human_profile,
        agent_profile,
        owner_binding,
        agent_state: build_agent_state(runtime_store, &session.agent_id),
        agent_activity: build_agent_activity(runtime_store, &session.agent_id),
        pending_owner_replies,
        agent_inbox,
        human_inbox,
    })
}

fn sync_owner_reply_ready_from_human_inboxes(
    runtime_store: &mut OfficeRuntimeStore,
    session: &PilotWebSession,
) {
    let Some(owner_record) = runtime_store.get(&session.agent_id) else {
        return;
    };
    let owner_user_id = owner_record.owner_user_id.clone();
    for message in collab_inbox::messages_for_logical(&session.user_id) {
        if message.need_reply {
            continue;
        }
        let Some(reply_to_message_id) = message.reply_to_message_id.as_deref() else {
            continue;
        };
        if !runtime_store.has_owner_message_queued(&session.agent_id, reply_to_message_id) {
            continue;
        }
        if runtime_store.has_owner_reply_ready(&session.agent_id, reply_to_message_id) {
            continue;
        }
        let _ = runtime_store.mark_owner_reply_ready(
            &session.agent_id,
            &owner_user_id,
            reply_to_message_id,
            message.content.trim(),
        );
    }
}

fn build_agent_activity(
    runtime_store: &OfficeRuntimeStore,
    agent_id: &str,
) -> OfficeAgentActivityView {
    OfficeAgentActivityView {
        agent_id: agent_id.to_string(),
        entries: runtime_store.activity_for_agent(agent_id).to_vec(),
    }
}

fn build_agent_state(runtime_store: &OfficeRuntimeStore, agent_id: &str) -> OfficeAgentStateView {
    let Some(record) = runtime_store.get(agent_id) else {
        return OfficeAgentStateView {
            state: OfficeAgentExternalState::Idle,
            waiting_reason: None,
        };
    };
    let state = match record.state {
        AgentRuntimeState::Idle => OfficeAgentExternalState::Idle,
        AgentRuntimeState::Working => OfficeAgentExternalState::Working,
        AgentRuntimeState::Waiting => OfficeAgentExternalState::Waiting,
    };
    OfficeAgentStateView {
        state,
        waiting_reason: record.waiting_reason.clone(),
    }
}

fn register_session_participants(
    directory: &PersistentPilotDirectory,
    session: &PilotWebSession,
) -> Option<()> {
    let account = directory.account_for_user(&session.user_id)?;
    if account.agent_id != session.agent_id {
        return None;
    }
    let agent_profile = directory.agent_profile(&session.agent_id)?;
    if agent_profile.owner_user_id != session.user_id {
        return None;
    }
    Some(())
}

async fn sync_owner_profile_prompt_for_session(
    config: &OfficeWebConfig,
    directory: &PersistentPilotDirectory,
    session: &PilotWebSession,
) -> Result<PathBuf, std::io::Error> {
    let Some(human_profile) = directory.human_profile(&session.user_id) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "human profile not found",
        ));
    };
    let Some(agent_profile) = directory.agent_profile(&session.agent_id) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "agent profile not found",
        ));
    };
    if agent_profile.owner_user_id != session.user_id {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "agent profile owner mismatch",
        ));
    }
    let rendered = render_owner_profile_prompt(human_profile);
    AgentMemoryStore::new(config.memory_root.clone())
        .write_owner_profile_text(&session.agent_id, &session.user_id, &rendered)
        .map_err(|err| std::io::Error::other(err.to_string()))?;
    write_agent_owner_profile_prompt_for_agent(
        &config.codex_home,
        &session.agent_id,
        &session.agent_id,
        &rendered,
    )
    .await
}

fn render_owner_profile_prompt(profile: &HumanProfile) -> String {
    const MAX_FACTS: usize = 16;
    let mut lines = Vec::new();
    lines.push(format!("# Owner Profile: {}", profile.user_id));
    lines.push(String::new());
    lines.push("This is durable context about the human owner this agent serves. Use it to adapt prioritization, communication style, and escalation judgment. Do not treat it as a replacement for the current task instructions.".to_string());
    lines.push(String::new());
    push_profile_scalar(&mut lines, "role", &profile.role);
    push_profile_scalar(&mut lines, "report_preference", &profile.report_preference);
    push_profile_list(&mut lines, "capabilities", &profile.capability_labels);
    push_profile_list(&mut lines, "do_not_disturb", &profile.do_not_disturb);
    push_profile_list(&mut lines, "likes", &profile.likes);
    push_profile_list(&mut lines, "dislikes", &profile.dislikes);
    push_profile_list(&mut lines, "preferences", &profile.preferences);

    let mut active_facts = profile
        .profile_facts
        .iter()
        .filter(|fact| matches!(fact.status, super::pilot::UserProfileFactStatus::Active))
        .collect::<Vec<_>>();
    active_facts.sort_by(|left, right| {
        right
            .salience
            .cmp(&left.salience)
            .then_with(|| right.confidence.cmp(&left.confidence))
            .then_with(|| right.updated_at.cmp(&left.updated_at))
    });
    if !active_facts.is_empty() {
        lines.push(String::new());
        lines.push("active_facts:".to_string());
        for fact in active_facts.into_iter().take(MAX_FACTS) {
            lines.push(format!(
                "- [{} confidence={} salience={}] {}",
                fact.category, fact.confidence, fact.salience, fact.value
            ));
        }
    }
    if let Some(last_reflection_at) = profile.last_reflection_at {
        lines.push(String::new());
        lines.push(format!(
            "last_reflection_at: {}",
            last_reflection_at.to_rfc3339()
        ));
    }
    lines.join("\n")
}

fn push_profile_scalar(lines: &mut Vec<String>, key: &str, value: &str) {
    let value = value.trim();
    if !value.is_empty() {
        lines.push(format!("{key}: {value}"));
    }
}

fn push_profile_list(lines: &mut Vec<String>, key: &str, values: &[String]) {
    let values = values
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    if values.is_empty() {
        return;
    }
    lines.push(format!("{key}:"));
    for value in values {
        lines.push(format!("- {value}"));
    }
}

fn build_agent_inbox(agent_id: &str) -> OfficeAgentInboxView {
    build_inbox(agent_id)
}

fn build_human_inbox(user_id: &str) -> OfficeAgentInboxView {
    let messages = collab_inbox::messages_for_logical(user_id)
        .iter()
        .map(|message| {
            let reply_status = human_reply_status(user_id, message);
            OfficeInboxMessage {
                message_id: message.message_id.clone(),
                from: message.sender_agent_name.clone(),
                to: user_id.to_string(),
                content: message.content.clone(),
                need_reply: message.need_reply,
                reply_status,
                reply_to_message_id: message.reply_to_message_id.clone(),
            }
        })
        .collect::<Vec<_>>();
    let queued_count = messages
        .iter()
        .filter(|message| message.reply_status == OfficeInboxReplyStatus::Pending)
        .count();
    OfficeAgentInboxView {
        agent_id: user_id.to_string(),
        queued_count,
        messages,
    }
}

fn build_inbox(participant_id: &str) -> OfficeAgentInboxView {
    let messages = collab_inbox::messages_for_logical(participant_id)
        .iter()
        .map(|message| OfficeInboxMessage {
            message_id: message.message_id.clone(),
            from: message.sender_agent_name.clone(),
            to: participant_id.to_string(),
            content: message.content.clone(),
            need_reply: message.need_reply,
            reply_status: reply_status_from_need_reply(message.need_reply),
            reply_to_message_id: message.reply_to_message_id.clone(),
        })
        .collect::<Vec<_>>();
    OfficeAgentInboxView {
        agent_id: participant_id.to_string(),
        queued_count: messages.len(),
        messages,
    }
}

fn human_reply_status(
    user_id: &str,
    message: &collab_inbox::InboxMessage,
) -> OfficeInboxReplyStatus {
    if !message.need_reply {
        return OfficeInboxReplyStatus::NotRequired;
    }
    match collab_inbox::required_reply_resolved_logical(user_id, &message.message_id) {
        Some(true) => OfficeInboxReplyStatus::Replied,
        Some(false) | None => OfficeInboxReplyStatus::Pending,
    }
}

fn reply_status_from_need_reply(need_reply: bool) -> OfficeInboxReplyStatus {
    if need_reply {
        OfficeInboxReplyStatus::Pending
    } else {
        OfficeInboxReplyStatus::NotRequired
    }
}

fn parse_request<T>(headers: &HeaderMap, body: &[u8]) -> Result<T, String>
where
    T: for<'de> Deserialize<'de>,
{
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if content_type.starts_with("application/json") {
        serde_json::from_slice(body).map_err(|err| format!("invalid JSON body: {err}"))
    } else {
        let mut form = HashMap::new();
        for (key, value) in url::form_urlencoded::parse(body).into_owned() {
            if matches!(key.as_str(), "capabilities" | "avoid" | "profile_updates") {
                let values = value
                    .split([',', '，', '\n'])
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                form.insert(
                    key,
                    serde_json::Value::Array(
                        values.into_iter().map(serde_json::Value::String).collect(),
                    ),
                );
            } else {
                form.insert(key, serde_json::Value::String(value));
            }
        }
        let value = serde_json::Value::Object(form.into_iter().collect());
        serde_json::from_value(value).map_err(|err| format!("invalid form body: {err}"))
    }
}

fn deserialize_form_compatible_bool<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Bool(value) => Ok(value),
        serde_json::Value::String(value) => match value.trim().to_ascii_lowercase().as_str() {
            "" => Ok(false),
            "true" | "1" | "yes" | "on" => Ok(true),
            "false" | "0" | "no" | "off" => Ok(false),
            other => Err(de::Error::custom(format!(
                "invalid boolean string `{other}`"
            ))),
        },
        serde_json::Value::Number(value) => match value.as_i64() {
            Some(0) => Ok(false),
            Some(1) => Ok(true),
            _ => Err(de::Error::custom("boolean number must be 0 or 1")),
        },
        serde_json::Value::Null => Ok(false),
        other => Err(de::Error::custom(format!(
            "invalid boolean value `{other}`"
        ))),
    }
}

fn wants_json(headers: &HeaderMap) -> bool {
    let content_type_json = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"));
    let accept_json = headers
        .get(ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("application/json"));
    content_type_json || accept_json
}

fn session_token(cookie_name: &str, headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(COOKIE)?.to_str().ok()?;
    for cookie in raw.split(';') {
        let Some((name, value)) = cookie.trim().split_once('=') else {
            continue;
        };
        if name == cookie_name && !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

fn session_cookie(cookie_name: &str, token: &str) -> String {
    format!("{cookie_name}={token}; Path=/; HttpOnly; SameSite=Lax")
}

fn expire_session_cookie(cookie_name: &str) -> String {
    format!("{cookie_name}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0")
}

fn redirect_to_login() -> impl IntoResponse {
    (StatusCode::SEE_OTHER, [(LOCATION, "/login")])
}

fn error_response(
    status: StatusCode,
    message: impl Into<String>,
    json: bool,
) -> axum::response::Response {
    let message = message.into();
    if json {
        (status, Json(ErrorBody { error: message })).into_response()
    } else {
        (status, Html(format!("<h1>{}</h1>", html_escape(&message)))).into_response()
    }
}

fn render_login_page(error: Option<&str>) -> String {
    let error = error
        .map(|message| {
            format!(
                r#"<div class="error" role="alert"><span style="margin-right:6px">⚠</span>{}</div>"#,
                html_escape(message)
            )
        })
        .unwrap_or_default();
    format!(
        r#"<!doctype html>
<html lang="zh-CN">
<head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>Codex Office · 登录</title>{style}</head>
<body class="auth-page"><main class="auth-shell">
<section class="auth-hero" aria-labelledby="login-title">
<div class="brand-mark" aria-hidden="true">✦</div>
<p class="eyebrow">AI-Native Workspace</p>
<h1 id="login-title">AI 协同办公室</h1>
<p class="hero-copy">每一个 Agent 都是一个可信赖的数字同事。委派、协作、复盘——如同与最优秀的团队共事。</p>
<div class="auth-highlights" aria-label="产品亮点">
<span>⚡ 实时 Agent 状态</span><span>💬 清晰上下文回复</span><span>📋 共享团队黑板</span><span>🧠 长期记忆成长</span>
</div>
</section>
<section class="auth-card" aria-label="登录表单">{error}
<div style="margin-bottom:18px"><span class="live-badge">系统在线</span></div>
<form method="post" action="/login" class="stacked-form">
<label>账号 <input name="username" autocomplete="username" required placeholder="例如：userA"></label>
<label>密码 <input name="password" type="password" autocomplete="current-password" required placeholder="输入密码"></label>
<button type="submit">进入我的办公室</button>
</form>
<p class="section-help" style="margin-top:14px">试点账号：userA · userB · userC · userD · userE · userF</p>
</section>
</main></body></html>"#,
        style = page_style(),
        error = error
    )
}
fn render_dashboard(me: &OfficeMeResponse) -> String {
    let activity_items = render_activity_items(&me.agent_activity.entries);
    let human_inbox_items = render_human_inbox_items(&me.human_inbox.messages);
    let dashboard_script = dashboard_script();
    let agent_state = html_escape(&format!("{:?}", me.agent_state.state).to_ascii_lowercase());
    let role = html_escape(&me.human_profile.role);
    let capabilities_text = html_escape(&me.human_profile.capability_labels.join(" · "));
    let avoid_text = html_escape(&me.human_profile.do_not_disturb.join(" · "));
    let report_preference = html_escape(&me.human_profile.report_preference);
    let last_reflection = me
        .human_profile
        .last_reflection_at
        .map(|value| value.to_rfc3339())
        .unwrap_or_else(|| "尚未复盘".to_string());
    let state_emoji = match me.agent_state.state {
        OfficeAgentExternalState::Idle => "🟢",
        OfficeAgentExternalState::Working => "🟣",
        OfficeAgentExternalState::Waiting => "🟡",
    };
    let state_label = match me.agent_state.state {
        OfficeAgentExternalState::Idle => "空闲中",
        OfficeAgentExternalState::Working => "工作中",
        OfficeAgentExternalState::Waiting => "等待回复",
    };
    format!(
        r##"<!doctype html>
<html lang="zh-CN">
<head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>我的 Agent · Codex Office</title>{style}</head>
<body class="dashboard-page">
<div class="app-shell">
<aside class="side-rail" aria-label="导航">
<a class="rail-brand" href="/me" aria-label="Codex Office"><span>✦</span><strong>Codex<br>Office</strong></a>
<nav>
<a class="active" href="/me">🏠 仪表盘</a>
<a href="#agent-composer">✏️ 委派任务</a>
<a href="#human-inbox-section">📨 我的收件箱</a>
<a href="#activity-section">📊 活动流</a>
<a href="/blackboard">📋 共享黑板</a>
</nav>
<form method="post" action="/logout" class="rail-logout"><button type="submit" class="ghost-button" style="width:100%;color:rgba(255,255,255,.5)">退出登录</button></form>
</aside>
<main class="office-main">
<section class="hero-panel" aria-labelledby="dashboard-title">
<div>
<div style="display:flex;align-items:center;gap:8px;margin-bottom:6px"><span class="live-badge">在线</span><p class="eyebrow" style="margin:0">你的 AI 办公室</p></div>
<h1 id="dashboard-title">我的 Agent</h1>
<p class="hero-copy">一个入口委派任务，一眼看懂状态、回复与交付物。Agent 会记住你的偏好，越用越懂你。</p>
</div>
<div class="hero-status" aria-label="Agent 状态">
<span class="state-chip state-{agent_state}" id="agent-state">{state_emoji} {state_label}</span>
<span class="metric"><strong id="queued-count">{queued_count}</strong><small>待处理</small></span>
<span class="metric"><strong id="human-queued-count">{human_queued_count}</strong><small>待我回复</small></span>
</div>
</section>
<section class="identity-grid" aria-label="身份与画像">
<article class="surface-card identity-card">
<div class="card-heading"><span class="avatar" aria-hidden="true">{username_initial}</span><div><p class="eyebrow">Owner</p><h2>{username}</h2></div></div>
<div class="meta-grid"><span>User <code>{user_id}</code></span><span>Agent <code>{agent_id}</code></span><span>角色 {role}</span></div>
</article>
<article class="surface-card profile-card">
<div class="card-heading"><div><p class="eyebrow">主人画像</p><h2>偏好与边界</h2></div><form method="post" action="/api/owner-profile/sync"><button type="submit" class="secondary-button">🔄 同步到 Agent Prompt</button></form></div>
<div class="tag-cloud"><span>🎯 {capabilities}</span><span>🚫 {avoid}</span><span>📝 {report_preference}</span></div>
<p class="section-help">最近复盘：{last_reflection}</p>
</article>
</section>
<section class="command-card" id="agent-composer" aria-labelledby="composer-title">
<div class="command-copy">
<p class="eyebrow">委派命令</p>
<h2 id="composer-title">发给我的 Agent</h2>
<p>队列中有 <span class="count-pill" id="queued-count-shadow" style="background:rgba(139,92,246,.1);border-color:rgba(139,92,246,.2);color:var(--accent-2)">{queued_count}</span> 条待处理消息。直接描述目标，Agent 会在活动流中同步进展。</p>
</div>
<form class="agent-message-form" method="post" action="/api/inbox">
<input type="hidden" name="need_reply" value="true">
<label>任务内容 <textarea name="content" rows="4" placeholder="例如：请帮我检查这份材料的逻辑，先给结论再列风险点，最后给出下一步建议。" required></textarea></label>
<div class="composer-actions"><button type="submit">🚀 委派给我的 Agent</button><span>💡 先结论、再风险、最后下一步</span></div>
</form>
</section>
<div class="workspace-grid">
<section class="surface-card stream-panel" id="human-inbox-section">
<div class="section-header"><div><p class="eyebrow">收件箱</p><h2>Agent 给我的消息</h2></div><span class="count-pill" style="background:rgba(245,158,11,.1);border-color:rgba(245,158,11,.2);color:var(--warn)">待回复 <span id="human-queued-count-shadow">{human_queued_count}</span></span></div>
<div class="stream-toolbar">
<div class="filter-tabs" id="inbox-filters">
<button class="filter-tab active" data-filter="all">全部</button>
<button class="filter-tab" data-filter="pending">待回复</button>
<button class="filter-tab" data-filter="replied">已回复</button>
</div>
<button class="ghost-button stream-expand" id="inbox-expand" title="展开/折叠全部">📋 展开全部</button>
</div>
<div class="stream-scroll" id="inbox-scroll">
<ol class="human-inbox" id="human-inbox-list">{human_inbox_items}</ol>
</div>
</section>
<section class="surface-card stream-panel" id="activity-section">
<div class="section-header"><div><p class="eyebrow">实时动态</p><h2>Agent 活动流</h2></div><p class="activity-status" id="activity-status">自动刷新中</p></div>
<div class="stream-toolbar">
<div class="filter-tabs" id="activity-filters">
<button class="filter-tab active" data-filter="all">全部</button>
<button class="filter-tab" data-filter="tool">🔧 工具调用</button>
<button class="filter-tab" data-filter="summary">📝 工作摘要</button>
<button class="filter-tab" data-filter="reply">💬 Agent 回复</button>
</div>
<button class="ghost-button stream-expand" id="activity-expand" title="展开/折叠全部">📋 展开全部</button>
</div>
<div class="stream-scroll" id="activity-scroll">
<ol class="activity" id="activity-list">{activity_items}</ol>
</div>
</section>
<aside class="surface-card artifact-panel" aria-label="快捷操作">
<div class="section-header"><div><p class="eyebrow">Workspace</p><h2>工作区</h2></div></div>
<a class="quick-link" href="/interview"><strong>📋 首次访谈</strong><span>填写或更新主人画像 →</span></a>
<form method="post" action="/api/reflection" class="reflection-form">
<label style="font-size:.85em">每日复盘 <input name="profile_updates" placeholder="客户关系, 招聘判断"></label>
<button type="submit" class="secondary-button" style="width:100%">💾 保存复盘</button>
</form>
<a class="quick-link" href="/blackboard"><strong>📋 共享黑板</strong><span>查看 Agent 协作进展 →</span></a>
</aside>
</div>
</main>
</div>{dashboard_script}</body></html>"##,
        style = page_style(),
        dashboard_script = dashboard_script,
        username = html_escape(&me.username),
        username_initial = html_escape(&me.username.chars().next().unwrap_or('用').to_string()),
        user_id = html_escape(&me.user_id),
        agent_id = html_escape(&me.agent_id),
        agent_state = agent_state,
        queued_count = me.agent_inbox.queued_count,
        human_queued_count = me.human_inbox.queued_count,
        human_inbox_items = human_inbox_items,
        activity_items = activity_items,
        role = role,
        capabilities = if capabilities_text.is_empty() {
            "暂无能力标签".to_string()
        } else {
            format!("能力标签：{capabilities_text}")
        },
        avoid = if avoid_text.is_empty() {
            "暂无勿扰事项".to_string()
        } else {
            format!("勿扰事项：{avoid_text}")
        },
        report_preference = if report_preference.is_empty() {
            "汇报偏好：先结论后细节".to_string()
        } else {
            format!("汇报偏好：{report_preference}")
        },
        last_reflection = html_escape(&last_reflection)
    )
}
fn render_human_inbox_items(messages: &[OfficeInboxMessage]) -> String {
    if messages.is_empty() {
        return r#"<li class="empty-state"><span aria-hidden="true">✉</span><strong>暂无 Agent 发给你的消息。</strong><p>当 Agent 需要你确认、补充或决策时，会在这里形成一张可直接回复的卡片。</p></li>"#.to_string();
    }
    messages
        .iter()
        .rev()
        .map(|message| {
            let status = inbox_reply_status_label(message.reply_status);
            let reply_to = render_reply_to_code(message.reply_to_message_id.as_deref());
            let reply_form = render_human_reply_form(message);
            format!(
                r#"<li class="message-card" data-message-id="{}" data-reply-status="{}"><div class="message-topline"><span class="inbox-status">{}</span><strong>{}</strong></div><p>{}</p><div class="message-meta"><code>message_id: {}</code>{}</div>{}</li>"#,
                html_escape(&message.message_id),
                html_escape(inbox_reply_status_value(message.reply_status)),
                html_escape(status),
                html_escape(&message.from),
                html_escape(&message.content),
                html_escape(&message.message_id),
                reply_to,
                reply_form
            )
        })
        .collect::<Vec<_>>()
        .join("")
}
fn render_reply_to_code(reply_to_message_id: Option<&str>) -> String {
    reply_to_message_id
        .map(|reply_to| format!("<code>reply_to: {}</code>", html_escape(reply_to)))
        .unwrap_or_default()
}

fn render_human_reply_form(message: &OfficeInboxMessage) -> String {
    if message.reply_status != OfficeInboxReplyStatus::Pending {
        return String::new();
    }
    format!(
        r#"<form class="human-reply-form" method="post" action="/api/inbox">
<input type="hidden" name="need_reply" value="false">
<input type="hidden" name="reply_to_message_id" value="{message_id}">
<label>回复 Agent <textarea name="content" rows="3" placeholder="输入你的回复，例如：同意，请继续推进。" required></textarea></label>
<button type="submit">回复这条消息</button>
</form>"#,
        message_id = html_escape(&message.message_id)
    )
}
fn inbox_reply_status_label(status: OfficeInboxReplyStatus) -> &'static str {
    match status {
        OfficeInboxReplyStatus::NotRequired => "无需回复",
        OfficeInboxReplyStatus::Pending => "待回复",
        OfficeInboxReplyStatus::Replied => "已回复",
    }
}

fn inbox_reply_status_value(status: OfficeInboxReplyStatus) -> &'static str {
    match status {
        OfficeInboxReplyStatus::NotRequired => "not_required",
        OfficeInboxReplyStatus::Pending => "pending",
        OfficeInboxReplyStatus::Replied => "replied",
    }
}

fn render_activity_items(entries: &[OfficeActivityEntry]) -> String {
    if entries.is_empty() {
        return r#"<li class="empty-state"><span aria-hidden="true">📊</span><strong>暂无活动记录</strong><p>给 Agent 发送任务后，它会在这里同步工作进展与交付物。</p></li>"#.to_string();
    }
    entries
        .iter()
        .rev()
        .map(|entry| {
            let detail = entry
                .detail
                .as_deref()
                .filter(|value| !value.is_empty())
                .map(|value| format!(r#"<div class="activity-detail">{}</div>"#, html_escape(value)))
                .unwrap_or_default();
            let tool = entry.tool_name.as_deref().filter(|t| !t.is_empty())
                .map(|t| format!(r#"<span class="activity-tool">🔧 {}</span>"#, html_escape(t)))
                .unwrap_or_default();
            let cat = if entry.tool_name.as_deref().map_or(false, |t| !t.is_empty()) { "tool" }
                else if entry.title.contains("工作摘要") || entry.title.contains("计划") { "summary" }
                else if entry.title.contains("Agent 输出") || entry.title.contains("Agent 已回复") || entry.title.contains("发送给协作") { "reply" }
                else { "other" };
            format!(
                r#"<li class="timeline-card" data-category="{cat}" data-sequence="{seq}"><div class="timeline-dot" aria-hidden="true"></div><time>{time}</time><strong>{title}</strong>{tool}<p>{summary}</p>{detail}</li>"#,
                cat = cat,
                seq = entry.sequence,
                time = html_escape(&entry.timestamp.to_rfc3339()),
                title = html_escape(&entry.title),
                summary = html_escape(&entry.summary),
                detail = detail,
            )
        })
        .collect::<Vec<_>>()
        .join("")
}
fn dashboard_script() -> &'static str {
    r##"<script>
(() => {
  const activityList = document.getElementById("activity-list");
  const humanInboxList = document.getElementById("human-inbox-list");
  const agentState = document.getElementById("agent-state");
  const queuedCount = document.getElementById("queued-count");
  const queuedCountShadow = document.getElementById("queued-count-shadow");
  const humanQueuedCount = document.getElementById("human-queued-count");
  const humanQueuedCountShadow = document.getElementById("human-queued-count-shadow");
  const activityStatus = document.getElementById("activity-status");
  const setText = (node, value) => { if (node) node.textContent = String(value); };
  const escapeHtml = (value) => String(value ?? "")
    .replaceAll("&", "&amp;").replaceAll("<", "&lt;").replaceAll(">", "&gt;").replaceAll('"', "&quot;");

  let inboxFilter = "all";
  let activityFilter = "all";
  let inboxExpanded = false;
  let activityExpanded = false;

  const stateEmoji = (state) => {
    if (!state) return "";
    const s = String(state).toLowerCase();
    if (s.includes("idle")) return "🟢 空闲中";
    if (s.includes("work") || s.includes("running")) return "🟣 工作中";
    if (s.includes("wait") || s.includes("pending")) return "🟡 等待回复";
    return state;
  };

  const applyInboxFilter = () => {
    if (!humanInboxList) return;
    const cards = humanInboxList.querySelectorAll(".message-card");
    let visible = 0;
    cards.forEach(card => {
      const status = card.dataset.replyStatus;
      if (inboxFilter === "all" || status === inboxFilter) { card.hidden = false; visible++; }
      else { card.hidden = true; }
    });
    const scroll = document.getElementById("inbox-scroll");
    if (scroll && scroll.scrollTop > scroll.scrollHeight - scroll.clientHeight - 100) {
      scroll.scrollTop = scroll.scrollHeight;
    }
  };

  const getCardCategory = (entry) => {
    const t = entry.tool_name || "";
    const title = entry.title || "";
    if (t) return "tool";
    if (title.includes("工作摘要") || title.includes("计划")) return "summary";
    if (title.includes("Agent 输出") || title.includes("Agent 已回复") || title.includes("发送给协作")) return "reply";
    return "other";
  };

  const applyActivityFilter = () => {
    if (!activityList) return;
    const cards = activityList.querySelectorAll(".timeline-card:not(.thinking-entry)");
    const thinkingCards = activityList.querySelectorAll(".timeline-card.thinking-entry");
    const groups = activityList.querySelectorAll(".activity-group-header");
    if (activityFilter === "all") {
      cards.forEach(c => c.hidden = false);
      thinkingCards.forEach(c => c.hidden = false);
      groups.forEach(g => g.hidden = false);
    } else {
      cards.forEach(card => {
        const cat = card.dataset.category || "other";
        card.hidden = cat !== activityFilter;
      });
      thinkingCards.forEach(c => c.hidden = true);
      groups.forEach(g => {
        const body = g.nextElementSibling;
        if (!body || !body.classList.contains("activity-group-body")) return;
        const visibleCards = body.querySelectorAll(".timeline-card:not([hidden])");
        g.hidden = visibleCards.length === 0;
      });
    }
    if (activityFilter === "other") {
      thinkingCards.forEach(c => c.hidden = false);
      groups.forEach(g => {
        g.hidden = false;
        const body = g.nextElementSibling;
        if (body && body.classList.contains("activity-group-body")) {
          body.querySelectorAll(".timeline-card").forEach(c => c.hidden = false);
        }
      });
    }
  };

  let lastActivityKey = "";
  const renderActivity = (entries) => {
    if (!activityList) return;
    const key = entries ? entries.map(e => e.sequence + (e.summary || "")).join("|") : "";
    if (key === lastActivityKey) return;
    lastActivityKey = key;
    if (!entries || entries.length === 0) {
      activityList.innerHTML = "<li class=\"empty-state\"><span aria-hidden=\"true\">📊</span><strong>暂无活动记录</strong><p>给 Agent 发送任务后，它会在这里同步工作进展与交付物。</p></li>";
      return;
    }
    const reversed = entries.slice().reverse();
    const thinkingEntries = [];
    const normalEntries = [];
    reversed.forEach(e => {
      if ((e.title || "").includes("思考摘要更新") || (e.title || "").includes("思考摘要")) thinkingEntries.push(e);
      else normalEntries.push(e);
    });

    let html = "";
    normalEntries.forEach((entry, i) => {
      const detail = entry.detail ? `<div class="activity-detail">${escapeHtml(entry.detail)}</div>` : "";
      const tool = entry.tool_name ? `<span class="activity-tool">🔧 ${escapeHtml(entry.tool_name)}</span>` : "";
      const cat = getCardCategory(entry);
      const shortSummary = (entry.summary || "").length > 120 ? (entry.summary || "").substring(0, 120) + "..." : (entry.summary || "");
      const delay = `${Math.min(i * 0.03, 0.6)}s`;
      html += `<li class="timeline-card" style="animation-delay:${delay}" data-category="${cat}" data-sequence="${escapeHtml(entry.sequence)}"><div class="timeline-dot" aria-hidden="true"></div><time>${escapeHtml(entry.timestamp)}</time><strong>${escapeHtml(entry.title)}</strong>${tool}<p>${escapeHtml(shortSummary)}</p>${detail}</li>`;
    });

    if (thinkingEntries.length > 0) {
      const groupId = "thinking-group";
      const sampleSummary = thinkingEntries[thinkingEntries.length - 1]?.summary || "";
      const shortSample = (sampleSummary || "").length > 100 ? (sampleSummary || "").substring(0, 100) + "..." : sampleSummary;
      html += `<div class="activity-group-header open" id="${groupId}" data-category="other"><span class="group-count">${thinkingEntries.length}</span> 条思考过程 · ${escapeHtml(shortSample)}</div>`;
      html += `<div class="activity-group-body open">`;
      thinkingEntries.forEach((entry, i) => {
        html += `<li class="timeline-card thinking-entry" data-category="other" data-sequence="${escapeHtml(entry.sequence)}"><div class="timeline-dot" aria-hidden="true"></div><time>${escapeHtml(entry.timestamp)}</time><strong>${escapeHtml(entry.title)}</strong><p>${escapeHtml((entry.summary || "").substring(0, 100))}</p></li>`;
      });
      html += `</div>`;
    }

    activityList.innerHTML = html;
    applyActivityFilter();
    setupGroupToggles();
  };

  const setupGroupToggles = () => {
    activityList?.querySelectorAll(".activity-group-header").forEach(header => {
      header.onclick = () => {
        header.classList.toggle("open");
        const body = header.nextElementSibling;
        if (body && body.classList.contains("activity-group-body")) body.classList.toggle("open");
      };
    });
  };

  const replyStatusLabel = (status) => {
    if (status === "pending") return "🟡 待回复";
    if (status === "replied") return "✅ 已回复";
    return "📋 无需回复";
  };

  const replyFormHtml = (message, status) => {
    if (status !== "pending") return "";
    return `<form class="human-reply-form" method="post" action="/api/inbox"><input type="hidden" name="need_reply" value="false"><input type="hidden" name="reply_to_message_id" value="${escapeHtml(message.message_id)}"><label>回复 Agent <textarea name="content" rows="3" placeholder="输入你的回复，例如：同意，请继续推进。" required></textarea></label><button type="submit">📨 回复这条消息</button></form>`;
  };

  let lastHumanInboxKey = "";
  const renderHumanInbox = (messages) => {
    if (!humanInboxList) return;
    const key = messages ? messages.map(m => m.message_id + (m.reply_status || "")).join("|") : "";
    if (key === lastHumanInboxKey) return;
    lastHumanInboxKey = key;
    if (!messages || messages.length === 0) {
      humanInboxList.innerHTML = "<li class=\"empty-state\"><span aria-hidden=\"true\">📨</span><strong>暂无 Agent 发给你的消息</strong><p>当 Agent 需要你确认、补充或决策时，这里会形成一张可直接回复的卡片。</p></li>";
      return;
    }
    humanInboxList.innerHTML = messages.slice().reverse().map((message, i) => {
      const status = message.reply_status ?? (message.need_reply ? "pending" : "not_required");
      const replyTo = message.reply_to_message_id ? `<code>↩ reply_to: ${escapeHtml(message.reply_to_message_id)}</code>` : "";
      const content = (message.content || "");
      const isLong = content.length > 200;
      const displayContent = isLong ? content.substring(0, 200) + "..." : content;
      const toggleBtn = isLong ? `<span class="msg-toggle" onclick="this.parentElement.querySelector('.msg-body').classList.toggle('collapsed');this.classList.toggle('expanded')"></span>` : "";
      const fullContent = isLong ? `<div class="msg-body collapsed">${escapeHtml(content)}</div>` : "";
      const delay = `${Math.min(i * 0.04, 0.5)}s`;
      return `<li class="message-card" style="animation-delay:${delay}" data-message-id="${escapeHtml(message.message_id)}" data-reply-status="${escapeHtml(status)}"><div class="message-topline"><span class="inbox-status">${replyStatusLabel(status)}</span><strong>${escapeHtml(message.from)}</strong></div><p>${escapeHtml(displayContent)}${toggleBtn}</p>${fullContent}<div class="message-meta"><code>${escapeHtml(message.message_id)}</code>${replyTo}</div>${replyFormHtml(message, status)}</li>`;
    }).join("");
    applyInboxFilter();
  };

  const isEditingHumanReply = () => {
    const active = document.activeElement;
    return Boolean(active && humanInboxList?.contains(active) && active.closest?.(".human-reply-form"));
  };

  humanInboxList?.addEventListener("submit", async (event) => {
    const form = event.target;
    if (!(form instanceof HTMLFormElement) || !form.classList.contains("human-reply-form")) return;
    event.preventDefault();
    const submit = form.querySelector('button[type="submit"]');
    const content = String(new FormData(form).get("content") ?? "").trim();
    const replyToMessageId = String(new FormData(form).get("reply_to_message_id") ?? "").trim();
    if (!content || !replyToMessageId) return;
    if (submit) { submit.disabled = true; submit.textContent = "发送中..."; }
    try {
      const response = await fetch("/api/inbox", {
        method: "POST",
        headers: { "Accept": "application/json", "Content-Type": "application/json" },
        body: JSON.stringify({ content, need_reply: false, reply_to_message_id: replyToMessageId })
      });
      if (!response.ok) throw new Error("reply failed");
      const body = await response.json();
      renderHumanInbox(body.me?.human_inbox?.messages ?? []);
      renderActivity(body.me?.agent_activity?.entries ?? []);
      const nextHumanQueued = body.me?.human_inbox?.queued_count ?? 0;
      const nextQueued = body.me?.agent_inbox?.queued_count ?? 0;
      setText(humanQueuedCount, nextHumanQueued);
      setText(humanQueuedCountShadow, nextHumanQueued);
      setText(queuedCount, nextQueued);
      setText(queuedCountShadow, nextQueued);
      if (agentState && body.me?.agent_state?.state) {
        agentState.textContent = stateEmoji(body.me.agent_state.state);
        agentState.className = `state-chip state-${String(body.me.agent_state.state).toLowerCase()}`;
      }
      if (activityStatus) activityStatus.textContent = `✅ 已发送回复 · ${new Date().toLocaleTimeString()}`;
    } catch (_) {
      if (activityStatus) activityStatus.textContent = "❌ 回复发送失败，请重试";
      if (submit) { submit.disabled = false; submit.textContent = "📨 回复这条消息"; }
    }
  });

  // Filter tabs
  document.getElementById("inbox-filters")?.addEventListener("click", (e) => {
    const tab = e.target.closest(".filter-tab");
    if (!tab) return;
    document.querySelectorAll("#inbox-filters .filter-tab").forEach(t => t.classList.remove("active"));
    tab.classList.add("active");
    inboxFilter = tab.dataset.filter;
    applyInboxFilter();
  });

  document.getElementById("activity-filters")?.addEventListener("click", (e) => {
    const tab = e.target.closest(".filter-tab");
    if (!tab) return;
    document.querySelectorAll("#activity-filters .filter-tab").forEach(t => t.classList.remove("active"));
    tab.classList.add("active");
    activityFilter = tab.dataset.filter;
    applyActivityFilter();
  });

  // Expand/collapse buttons
  document.getElementById("inbox-expand")?.addEventListener("click", () => {
    inboxExpanded = !inboxExpanded;
    const scroll = document.getElementById("inbox-scroll");
    const btn = document.getElementById("inbox-expand");
    if (scroll) scroll.classList.toggle("expanded", inboxExpanded);
    if (btn) btn.textContent = inboxExpanded ? "📋 收起" : "📋 展开全部";
  });

  document.getElementById("activity-expand")?.addEventListener("click", () => {
    activityExpanded = !activityExpanded;
    const scroll = document.getElementById("activity-scroll");
    const btn = document.getElementById("activity-expand");
    if (scroll) scroll.classList.toggle("expanded", activityExpanded);
    if (btn) btn.textContent = activityExpanded ? "📋 收起" : "📋 展开全部";
  });

  let refreshTimer = null;
  const refresh = async () => {
    try {
      const response = await fetch("/api/me", { headers: { "Accept": "application/json" } });
      if (!response.ok) return;
      const me = await response.json();
      if (agentState && me.agent_state?.state) {
        agentState.textContent = stateEmoji(me.agent_state.state);
        agentState.className = `state-chip state-${String(me.agent_state.state).toLowerCase()}`;
      }
      setText(queuedCount, me.agent_inbox?.queued_count ?? 0);
      setText(queuedCountShadow, me.agent_inbox?.queued_count ?? 0);
      setText(humanQueuedCount, me.human_inbox?.queued_count ?? 0);
      setText(humanQueuedCountShadow, me.human_inbox?.queued_count ?? 0);
      const acScroll = document.getElementById("activity-scroll");
      const acScrollTop = acScroll ? acScroll.scrollTop : 0;
      renderActivity(me.agent_activity?.entries ?? []);
      if (acScroll) acScroll.scrollTop = acScrollTop;
      if (isEditingHumanReply()) {
        if (activityStatus) activityStatus.textContent = "✏️ 正在编辑回复...";
      } else {
        renderHumanInbox(me.human_inbox?.messages ?? []);
        const state = String(me.agent_state?.state || "unknown").toLowerCase();
        if (state === "working") {
          if (activityStatus) activityStatus.textContent = `⚙️ Agent 工作中 · ${new Date().toLocaleTimeString()}`;
        } else {
          if (activityStatus) activityStatus.textContent = `🔄 已更新 · ${new Date().toLocaleTimeString()}`;
        }
      }
    } catch (_) {
      if (activityStatus) activityStatus.textContent = "⚠️ 刷新失败，稍后重试";
    }
  };
  refreshTimer = setInterval(refresh, 1500);
  refresh();
})();
</script>"##
}
fn render_interview_page() -> String {
    format!(
        r#"<!doctype html>
<html lang="zh-CN"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>首次访谈 · Codex Office</title>{style}</head>
<body class="simple-page"><main class="form-shell"><section class="surface-card feature-form"><p class="eyebrow">🧭 Onboarding</p><h1>首次访谈：建立主人画像</h1><p class="hero-copy">让 Agent 先理解你的职责、擅长领域和沟通偏好。越清晰的画像，越少打扰、越快对齐。</p>
<div class="divider"></div>
<form method="post" action="/interview" class="stacked-form">
<label>🎯 你的角色 / 职责 <input name="role" required placeholder="例如：产品负责人 / CEO / 招聘负责人"></label>
<label>💪 能力标签（逗号分隔） <input name="capabilities" required placeholder="战略判断, 客户关系, 技术评审"></label>
<label>🚫 不希望 Agent 打扰你的事项（逗号分隔） <input name="avoid" placeholder="低优先级同步, 已归档项目"></label>
<label>📝 汇报偏好 <input name="report_preference" placeholder="先结论后细节"></label>
<div class="composer-actions" style="margin-top:8px"><button type="submit">💾 保存画像</button><a class="text-link" href="/me">← 返回仪表盘</a></div>
</form></section></main></body></html>"#,
        style = page_style()
    )
}
async fn blackboard_page(
    State(state): State<Arc<OfficeWebState>>,
    headers: HeaderMap,
) -> axum::response::Response {
    let Some(_token) = session_token(&state.config.session_cookie, &headers) else {
        return redirect_to_login().into_response();
    };
    let _directory = state.directory.lock().await;
    let mut blackboard_store = state._blackboard_store.lock().await;
    let _ = blackboard_store.reload();
    let blackboard = blackboard_store.blackboard();
    Html(render_blackboard_page(blackboard)).into_response()
}

async fn blackboard_json(
    State(state): State<Arc<OfficeWebState>>,
    headers: HeaderMap,
) -> axum::response::Response {
    let Some(_token) = session_token(&state.config.session_cookie, &headers) else {
        return error_response(StatusCode::UNAUTHORIZED, "not logged in", true);
    };
    let _directory = state.directory.lock().await;
    let mut blackboard_store = state._blackboard_store.lock().await;
    let _ = blackboard_store.reload();
    let blackboard = blackboard_store.blackboard();
    (StatusCode::OK, Json(blackboard)).into_response()
}

#[derive(Debug, Deserialize)]
struct BlackboardPostRequest {
    #[serde(default)]
    author_id: String,
    content: String,
}

async fn blackboard_post(
    State(state): State<Arc<OfficeWebState>>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    let wants_json = wants_json(&headers);
    let Some(_token) = session_token(&state.config.session_cookie, &headers) else {
        return error_response(StatusCode::UNAUTHORIZED, "not logged in", wants_json);
    };
    let request = match parse_request::<BlackboardPostRequest>(&headers, &body) {
        Ok(request) => request,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, message, wants_json),
    };
    if request.content.trim().is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "content required", wants_json);
    }
    let _directory = state.directory.lock().await;
    let mut blackboard_store = state._blackboard_store.lock().await;
    let note_id = Uuid::new_v4().to_string();
    let note = match blackboard_store.add_note(note_id, request.author_id, request.content) {
        Ok(note) => note,
        Err(err) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("persist failed: {err}"),
                wants_json,
            );
        }
    };
    if let Err(err) = blackboard_store.persist() {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("persist failed: {err}"),
            wants_json,
        );
    }
    if wants_json {
        (StatusCode::OK, Json(note)).into_response()
    } else {
        (StatusCode::SEE_OTHER, [(LOCATION, "/blackboard")]).into_response()
    }
}

fn render_blackboard_page(blackboard: &OfficeBlackboard) -> String {
    let notes_html = if blackboard.shared_notes.is_empty() {
        r#"<div class="empty-state"><span aria-hidden="true">📝</span><strong>暂无共享笔记</strong><p>Agent 处理结果和关键发现会展示在这里。</p></div>"#.to_string()
    } else {
        blackboard
            .shared_notes
            .iter()
            .rev()
            .map(|note| {
                format!(
                    r#"<article class="note-card">
                    <div class="note-meta">{author} · {time}</div>
                    <div class="note-content">{content}</div>
                    </article>"#,
                    author = html_escape(&note.author_id),
                    time = html_escape(&note.created_at.to_rfc3339()),
                    content = html_escape(&note.content)
                )
            })
            .collect::<Vec<_>>()
            .join("")
    };
    let goals_html = if blackboard.company_goals.is_empty() {
        r#"<li class="empty-row">暂无目标</li>"#.to_string()
    } else {
        blackboard
            .company_goals
            .iter()
            .map(|g| format!("<li>🎯 {}</li>", html_escape(g)))
            .collect::<Vec<_>>()
            .join("")
    };
    let projects_html = if blackboard.active_projects.is_empty() {
        r#"<li class="empty-row">暂无活跃项目</li>"#.to_string()
    } else {
        blackboard
            .active_projects
            .iter()
            .map(|project| {
                format!(
                    "<li><strong>📁 {}</strong><p>{}</p></li>",
                    html_escape(&project.title),
                    html_escape(&project.summary)
                )
            })
            .collect::<Vec<_>>()
            .join("")
    };
    format!(
        r#"<!doctype html>
<html lang="zh-CN"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>共享黑板 · Codex Office</title>{style}</head>
<body class="simple-page"><main class="board-shell">
<section class="hero-panel"><div><p class="eyebrow">📋 Shared Memory</p><h1>共享黑板</h1><p class="hero-copy">Agent 回复、项目进展和团队目标沉淀为可复用的协作记忆。越用越聪明。</p></div><a class="secondary-button" href="/me">← 返回我的 Agent</a></section>
<div class="workspace-grid board-grid">
<section class="surface-card"><div class="section-header"><div><p class="eyebrow">📝 Notes</p><h2>共享笔记</h2></div></div>{notes}</section>
<aside class="surface-card"><div class="section-header"><div><p class="eyebrow">🎯 Goals</p><h2>公司目标</h2></div></div><ul class="compact-list">{goals}</ul></aside>
<aside class="surface-card"><div class="section-header"><div><p class="eyebrow">📁 Projects</p><h2>活跃项目</h2></div></div><ul class="compact-list">{projects}</ul></aside>
</div>
</main></body></html>"#,
        style = page_style(),
        notes = notes_html,
        goals = goals_html,
        projects = projects_html,
    )
}
fn page_style() -> &'static str {
    r#"<style>
:root {
  color-scheme: dark;
  --bg: #08080a;
  --bg-elevated: #111114;
  --ink: #f4f4f3;
  --ink-dim: #a1a09a;
  --muted: #787770;
  --card: #161618;
  --card-hover: #1c1c1f;
  --line: rgba(255,255,255,.06);
  --line-strong: rgba(255,255,255,.1);
  --accent: #f59e0b;
  --accent-glow: rgba(245,158,11,.15);
  --accent-2: #8b5cf6;
  --accent-2-glow: rgba(139,92,246,.18);
  --accent-3: #06b6d4;
  --danger: #f43f5e;
  --danger-glow: rgba(244,63,94,.15);
  --warn: #f59e0b;
  --ok: #10b981;
  --ok-glow: rgba(16,185,129,.15);
  --info: #6366f1;
  --shadow: 0 20px 60px rgba(0,0,0,.45), 0 0 0 1px rgba(255,255,255,.04);
  --shadow-lg: 0 32px 80px rgba(0,0,0,.55), 0 0 0 1px rgba(255,255,255,.06);
  --shadow-soft: 0 12px 32px rgba(0,0,0,.3);
  --radius: 22px;
  --radius-sm: 14px;
  --radius-xs: 10px;
  --ease: cubic-bezier(.22,.6,.36,1);
  --ease-spring: cubic-bezier(.34,1.56,.64,1);
}
*,*::before,*::after{box-sizing:border-box;margin:0}
html{min-height:100%;scroll-behavior:smooth;-webkit-font-smoothing:antialiased;-moz-osx-font-smoothing:grayscale}
body{min-height:100%;font-family:"Inter","SF Pro Display","PingFang SC","Microsoft YaHei",ui-sans-serif,system-ui,-apple-system,sans-serif;background:#08080a;color:var(--ink);text-rendering:optimizeLegibility;overflow-x:hidden}
body::before{content:"";position:fixed;inset:0;pointer-events:none;z-index:0;background:radial-gradient(ellipse 80% 60% at 15% 0%,rgba(139,92,246,.12),transparent 50%),radial-gradient(ellipse 60% 70% at 85% 5%,rgba(6,182,212,.08),transparent 45%),radial-gradient(ellipse 50% 80% at 50% 100%,rgba(245,158,11,.06),transparent 55%)}
body::after{content:"";position:fixed;inset:0;pointer-events:none;z-index:0;background-image:radial-gradient(rgba(255,255,255,.03) 1px,transparent 1px);background-size:32px 32px;mask-image:linear-gradient(to bottom,rgba(0,0,0,.5) 0%,transparent 70%)}
main{position:relative;z-index:1}
a{color:inherit;text-decoration:none}
button,a,input,textarea{font:inherit}
h1,h2,h3,p{margin:0}
h1{font-size:clamp(32px,5vw,56px);font-weight:700;letter-spacing:-.04em;line-height:1.05;margin-bottom:12px;background:linear-gradient(135deg,var(--ink) 30%,rgba(244,244,243,.72));-webkit-background-clip:text;-webkit-text-fill-color:transparent;background-clip:text}
h2{font-size:clamp(18px,1.8vw,23px);font-weight:650;letter-spacing:-.02em;margin-bottom:6px;color:var(--ink)}
p{line-height:1.7;color:var(--ink-dim)}
label{display:block;color:var(--ink-dim);font-weight:550;font-size:.92em;letter-spacing:.01em}
input,textarea{width:100%;margin-top:6px;border:1px solid var(--line);border-radius:var(--radius-sm);background:rgba(22,22,24,.8);color:var(--ink);padding:12px 15px;outline:none;font-size:.95em;transition:border-color .2s var(--ease),box-shadow .2s var(--ease),background .2s var(--ease);backdrop-filter:blur(12px)}
textarea{resize:vertical;min-height:108px;line-height:1.6}
input:focus,textarea:focus{border-color:rgba(139,92,246,.5);box-shadow:0 0 0 3px rgba(139,92,246,.12),0 0 20px rgba(139,92,246,.08);background:rgba(28,28,31,.9)}
input::placeholder,textarea::placeholder{color:rgba(255,255,255,.2)}
button,.secondary-button,.ghost-button{display:inline-flex;align-items:center;justify-content:center;gap:8px;border:0;border-radius:999px;text-decoration:none;cursor:pointer;font-weight:600;transition:all .22s var(--ease);position:relative;overflow:hidden}
button:not(.ghost-button),.secondary-button{padding:11px 20px;font-size:.94em}
.stacked-form button,.agent-message-form button,.human-reply-form button{background:linear-gradient(135deg,#8b5cf6,#6366f1);color:white;box-shadow:0 8px 24px rgba(99,102,241,.35),0 0 0 1px rgba(139,92,246,.2)}
.stacked-form button:hover,.agent-message-form button:hover,.human-reply-form button:hover{transform:translateY(-2px);box-shadow:0 14px 32px rgba(99,102,241,.45),0 0 0 1px rgba(139,92,246,.35)}
.stacked-form button:active,.agent-message-form button:active,.human-reply-form button:active{transform:translateY(0) scale(.98)}
button:disabled{opacity:.4;cursor:not-allowed;transform:none!important}
.secondary-button{background:rgba(255,255,255,.06);color:var(--ink);border:1px solid var(--line);backdrop-filter:blur(12px)}
.secondary-button:hover{background:rgba(255,255,255,.1);border-color:var(--line-strong);transform:translateY(-1px)}
.ghost-button{padding:9px 14px;color:var(--ink-dim);background:transparent;border:1px solid transparent}
.ghost-button:hover{color:var(--ink);background:rgba(255,255,255,.05)}
.text-link{color:var(--accent-2);font-weight:600;text-decoration:none;transition:color .18s var(--ease)}
.text-link:hover{color:#a78bfa}
code{background:rgba(255,255,255,.06);border:1px solid var(--line);padding:3px 8px;border-radius:8px;font-size:.86em;color:var(--ink-dim);font-family:"JetBrains Mono","SF Mono","Fira Code",monospace}
.error{padding:12px 15px;border-radius:var(--radius-sm);background:rgba(244,63,94,.1);color:var(--danger);border:1px solid rgba(244,63,94,.2);font-size:.92em;margin-bottom:16px;backdrop-filter:blur(12px)}
.eyebrow{margin:0 0 8px;text-transform:uppercase;letter-spacing:.14em;font-size:11px;font-weight:700;color:var(--accent-2)}
.hero-copy{font-size:clamp(15px,1.8vw,17px);color:var(--ink-dim);max-width:620px;line-height:1.65}
.section-help{color:var(--muted);font-size:13px;line-height:1.6}
.surface-card,.command-card,.hero-panel,.auth-card{background:var(--card);border:1px solid var(--line);border-radius:var(--radius);box-shadow:var(--shadow);backdrop-filter:blur(20px);-webkit-backdrop-filter:blur(20px)}
.surface-card{padding:22px}
.auth-page{display:grid;place-items:center;min-height:100vh;padding:24px;position:relative;z-index:1}
.auth-shell{width:min(1080px,100%);display:grid;grid-template-columns:minmax(0,1.1fr) minmax(340px,.65fr);gap:22px;align-items:stretch}
.auth-hero{position:relative;min-height:560px;border-radius:28px;padding:40px;background:linear-gradient(155deg,#0c0c10 0%,#1a1028 35%,#0a1628 65%,#0c0c10 100%);color:white;overflow:hidden;box-shadow:var(--shadow-lg);border:1px solid rgba(255,255,255,.06)}
.auth-hero::before{content:"";position:absolute;inset:0;background:radial-gradient(ellipse 70% 50% at 30% 20%,rgba(139,92,246,.2),transparent),radial-gradient(ellipse 40% 40% at 80% 60%,rgba(6,182,212,.12),transparent);pointer-events:none}
.auth-hero::after{content:"";position:absolute;right:-80px;bottom:-120px;width:360px;height:360px;border-radius:999px;background:radial-gradient(circle,rgba(245,158,11,.18),transparent 65%);pointer-events:none}
.auth-hero .eyebrow,.auth-hero .hero-copy{color:rgba(255,255,255,.7)}
.auth-hero h1{-webkit-text-fill-color:white;background:none}
.brand-mark{width:52px;height:52px;border-radius:16px;display:grid;place-items:center;background:rgba(255,255,255,.1);border:1px solid rgba(255,255,255,.15);font-size:24px;margin-bottom:60px;backdrop-filter:blur(12px);transition:transform .3s var(--ease-spring),box-shadow .3s var(--ease)}
.brand-mark:hover{transform:scale(1.08) rotate(-4deg);box-shadow:0 0 30px rgba(139,92,246,.3)}
.auth-highlights{position:absolute;left:40px;right:40px;bottom:40px;display:flex;flex-wrap:wrap;gap:10px}
.auth-highlights span{padding:9px 14px;border-radius:999px;background:rgba(255,255,255,.08);border:1px solid rgba(255,255,255,.14);color:rgba(255,255,255,.8);font-size:13px;font-weight:550;backdrop-filter:blur(8px);transition:background .2s var(--ease)}
.auth-highlights span:hover{background:rgba(255,255,255,.14)}
.auth-card{align-self:center;padding:28px;border-radius:var(--radius);animation:fadeSlideUp .6s var(--ease-spring)}
@keyframes fadeSlideUp{from{opacity:0;transform:translateY(16px)}to{opacity:1;transform:translateY(0)}}
@keyframes pulse{0%,100%{opacity:1}50%{opacity:.55}}
@keyframes glow{0%,100%{box-shadow:0 0 5px var(--accent-glow)}50%{box-shadow:0 0 20px var(--accent-glow),0 0 40px rgba(245,158,11,.08)}}
@keyframes float{0%,100%{transform:translateY(0)}50%{transform:translateY(-6px)}}
.stacked-form{display:grid;gap:14px}
.app-shell{display:grid;grid-template-columns:220px minmax(0,1fr);gap:22px;width:min(1440px,100%);margin:0 auto;padding:20px;position:relative;z-index:1}
.side-rail{position:sticky;top:20px;height:calc(100vh - 40px);border-radius:24px;padding:18px;background:linear-gradient(175deg,rgba(17,17,20,.95),rgba(12,12,15,.98));color:#fff;box-shadow:var(--shadow-lg);border:1px solid var(--line);display:flex;flex-direction:column;backdrop-filter:blur(20px)}
.rail-brand{display:flex;align-items:center;gap:10px;color:white;text-decoration:none;margin-bottom:22px;padding:6px 0}
.rail-brand span{width:36px;height:36px;border-radius:12px;background:linear-gradient(135deg,rgba(139,92,246,.3),rgba(99,102,241,.2));border:1px solid rgba(139,92,246,.2);display:grid;place-items:center;font-size:18px;transition:transform .3s var(--ease-spring)}
.rail-brand:hover span{transform:rotate(-6deg) scale(1.1)}
.rail-brand strong{line-height:1.1;letter-spacing:-.02em;font-size:14px;font-weight:700}
.side-rail nav{display:grid;gap:4px;flex:1}
.side-rail nav a{padding:10px 12px;border-radius:12px;text-decoration:none;color:rgba(255,255,255,.55);font-size:.9em;font-weight:550;transition:all .22s var(--ease);position:relative}
.side-rail nav a:hover{background:rgba(255,255,255,.06);color:rgba(255,255,255,.85)}
.side-rail nav a.active{background:rgba(139,92,246,.15);color:white;box-shadow:0 0 20px rgba(139,92,246,.1)}
.side-rail nav a::before{content:"";position:absolute;left:0;top:50%;transform:translateY(-50%);width:3px;height:0;border-radius:3px;background:var(--accent-2);transition:height .25s var(--ease)}
.side-rail nav a.active::before,.side-rail nav a:hover::before{height:16px}
.rail-logout{margin-top:auto;padding-top:12px;border-top:1px solid var(--line)}
.office-main{display:grid;gap:20px;min-width:0}
.hero-panel{display:flex;align-items:flex-end;justify-content:space-between;gap:18px;padding:28px 30px;position:relative;overflow:hidden;animation:fadeSlideUp .5s var(--ease-spring)}
.hero-panel::after{content:"";position:absolute;right:-40px;top:-60px;width:200px;height:200px;border-radius:999px;background:radial-gradient(circle,rgba(139,92,246,.08),transparent 70%);pointer-events:none}
.hero-status{display:flex;gap:10px;align-items:center;flex-wrap:wrap;justify-content:flex-end}
.state-chip,.count-pill,.inbox-status,.metric{display:inline-flex;align-items:center;gap:7px;border-radius:999px;border:1px solid var(--line);background:rgba(22,22,24,.8);padding:7px 12px;font-weight:600;font-size:.88em;backdrop-filter:blur(8px)}
.state-chip::before,.inbox-status::before{content:"";width:7px;height:7px;border-radius:50%;background:currentColor}
.state-idle,.state-ready{color:var(--ok);background:rgba(16,185,129,.1);border-color:rgba(16,185,129,.2)}.state-idle::before,.state-ready::before{animation:pulse 2s infinite}
.state-working,.state-running{color:var(--accent-2);background:rgba(139,92,246,.1);border-color:rgba(139,92,246,.2)}.state-working::before,.state-running::before{animation:pulse 1.5s infinite}
.state-waiting,.state-waiting_for_human,.state-pending{color:var(--warn);background:rgba(245,158,11,.1);border-color:rgba(245,158,11,.2)}.state-waiting::before,.state-waiting_for_human::before,.state-pending::before{animation:pulse 2.2s infinite}
.state-error,.state-blocked{color:var(--danger);background:rgba(244,63,94,.1);border-color:rgba(244,63,94,.2)}
.metric{flex-direction:column;align-items:flex-start;gap:2px;min-width:88px;padding:10px 13px}
.metric strong{font-size:24px;line-height:1;font-weight:750;color:var(--ink)}
.metric small{color:var(--muted);font-weight:550;font-size:.82em}
.count-pill{padding:6px 11px;font-size:.85em}
.identity-grid{display:grid;grid-template-columns:minmax(0,.75fr) minmax(0,1.25fr);gap:20px}
.card-heading,.section-header,.message-topline,.composer-actions{display:flex;align-items:center;justify-content:space-between;gap:14px;flex-wrap:wrap}
.avatar{width:50px;height:50px;border-radius:16px;background:linear-gradient(135deg,#8b5cf6,#06b6d4);color:white;display:grid;place-items:center;font-weight:750;font-size:22px;box-shadow:0 10px 25px rgba(139,92,246,.3);flex-shrink:0}
.meta-grid{display:grid;gap:8px;margin-top:16px}
.meta-grid span{display:flex;justify-content:space-between;gap:10px;color:var(--ink-dim);padding:8px 0;border-top:1px solid var(--line);font-size:.9em}
.tag-cloud{display:flex;gap:8px;flex-wrap:wrap;margin:14px 0}
.tag-cloud span{padding:8px 12px;border-radius:999px;background:rgba(139,92,246,.08);border:1px solid rgba(139,92,246,.15);color:#c4b5fd;font-weight:550;font-size:.88em;transition:all .2s var(--ease)}
.tag-cloud span:hover{background:rgba(139,92,246,.15);border-color:rgba(139,92,246,.25);transform:translateY(-1px)}
.command-card{position:relative;overflow:hidden;display:grid;grid-template-columns:minmax(220px,.68fr) minmax(300px,1.32fr);gap:22px;padding:26px;animation:fadeSlideUp .6s var(--ease-spring)}
.command-card::after{content:"";position:absolute;inset:auto 16px -50px auto;width:180px;height:180px;border-radius:999px;background:radial-gradient(circle,rgba(139,92,246,.1),transparent 70%);pointer-events:none}
.agent-message-form{display:grid;gap:10px;position:relative}
.composer-actions{align-items:center}
.composer-actions span{color:var(--muted);font-size:12px;font-weight:500}
.workspace-grid{display:grid;grid-template-columns:minmax(0,1.05fr) minmax(0,1.05fr) minmax(260px,.7fr);gap:20px;align-items:start}
.activity,.human-inbox{list-style:none;padding:0;margin:16px 0 0;display:grid;gap:10px}
.message-card,.timeline-card{position:relative;border:1px solid var(--line);background:rgba(22,22,24,.7);border-radius:var(--radius-sm);padding:15px;box-shadow:0 8px 20px rgba(0,0,0,.2);transition:all .25s var(--ease);backdrop-filter:blur(8px)}
.message-card:hover,.timeline-card:hover{transform:translateY(-2px);border-color:var(--line-strong);box-shadow:0 14px 32px rgba(0,0,0,.35);background:var(--card-hover)}
.message-card:focus-within,.timeline-card:focus-within{border-color:rgba(139,92,246,.3);box-shadow:0 0 20px rgba(139,92,246,.08)}
.human-inbox p,.activity-detail,.note-content{white-space:pre-wrap;color:var(--ink-dim);line-height:1.6}
.message-meta{display:flex;gap:6px;flex-wrap:wrap;margin:8px 0;font-size:.85em}
.inbox-status{padding:4px 8px;font-size:11px;font-weight:650}
.human-inbox li[data-reply-status="pending"] .inbox-status{background:rgba(245,158,11,.12);color:var(--warn);border-color:rgba(245,158,11,.25)}
.human-inbox li[data-reply-status="replied"] .inbox-status{background:rgba(99,102,241,.1);color:var(--info);border-color:rgba(99,102,241,.2)}
.human-reply-form{display:grid;gap:8px;margin-top:12px;padding-top:12px;border-top:1px dashed var(--line)}
.activity{position:relative}
.timeline-card{padding-left:36px;animation:fadeSlideUp .4s var(--ease) both}
.timeline-dot{position:absolute;left:14px;top:18px;width:9px;height:9px;border-radius:50%;background:linear-gradient(135deg,var(--accent-2),var(--accent-3));box-shadow:0 0 0 4px rgba(139,92,246,.15),0 0 12px rgba(139,92,246,.25)}
.activity time{display:block;color:var(--muted);font-size:11px;margin-bottom:3px;font-weight:500}
.activity strong,.human-inbox strong{display:block;font-size:.94em}
.activity-status{color:var(--muted);font-size:12px;margin:0;font-weight:500;display:flex;align-items:center;gap:6px}
.activity-status::before{content:"";width:5px;height:5px;border-radius:50%;background:var(--accent-2);animation:pulse 1.8s infinite}
.activity-tool{display:inline-flex;margin:5px 0;padding:3px 8px;border-radius:999px;background:rgba(99,102,241,.1);color:var(--info);font-size:11px;font-weight:650;border:1px solid rgba(99,102,241,.15)}
.empty-state{display:grid;gap:8px;justify-items:start;border:1px dashed var(--line-strong);background:rgba(22,22,24,.4);border-radius:var(--radius-sm);padding:20px;color:var(--muted)}
.empty-state span{font-size:28px;color:var(--accent-2);opacity:.6}
.empty-state strong{color:var(--ink-dim)}
.empty-state p{font-size:.9em}
.artifact-panel{display:grid;gap:12px}
.quick-link{display:block;text-decoration:none;border:1px solid var(--line);border-radius:var(--radius-sm);padding:14px;background:rgba(22,22,24,.5);transition:all .22s var(--ease);position:relative;overflow:hidden}
.quick-link::after{content:"→";position:absolute;right:14px;top:50%;transform:translateY(-50%) translateX(8px);opacity:0;transition:all .22s var(--ease);color:var(--accent-2)}
.quick-link:hover{background:rgba(139,92,246,.06);border-color:rgba(139,92,246,.2);padding-right:36px}
.quick-link:hover::after{opacity:1;transform:translateY(-50%) translateX(0)}
.quick-link strong{display:block;margin-bottom:3px;font-size:.92em}
.quick-link span{color:var(--muted);font-size:13px}
.reflection-form{display:grid;gap:8px;border:1px solid var(--line);border-radius:var(--radius-sm);padding:14px;background:rgba(22,22,24,.5)}
.stream-panel{display:flex;flex-direction:column;padding:0;overflow:hidden}
.stream-panel .section-header{padding:20px 20px 0}
.stream-panel .section-help{padding:0 20px}
.stream-panel .divider{margin:12px 20px}
.stream-toolbar{display:flex;align-items:center;justify-content:space-between;padding:8px 20px;gap:10px;border-bottom:1px solid var(--line);flex-shrink:0}
.filter-tabs{display:flex;gap:4px;background:rgba(255,255,255,.03);border-radius:999px;padding:3px}
.filter-tab{border:0;border-radius:999px;padding:6px 13px;font-size:.8em;font-weight:580;color:var(--muted);background:transparent;cursor:pointer;transition:all .2s var(--ease);white-space:nowrap}
.filter-tab:hover{color:var(--ink);background:rgba(255,255,255,.05)}
.filter-tab.active{color:white;background:rgba(139,92,246,.25);box-shadow:0 0 12px rgba(139,92,246,.1)}
.stream-expand{font-size:.78em;padding:5px 10px;white-space:nowrap;flex-shrink:0}
.stream-scroll{flex:1;overflow-y:auto;overflow-x:hidden;max-height:420px;padding:8px 20px 16px;scroll-behavior:smooth;-webkit-overflow-scrolling:touch}
.stream-scroll::-webkit-scrollbar{width:4px}
.stream-scroll::-webkit-scrollbar-thumb{background:rgba(255,255,255,.06);border-radius:2px}
.stream-scroll::-webkit-scrollbar-thumb:hover{background:rgba(255,255,255,.12)}
.stream-scroll.expanded{max-height:none}
.human-inbox .message-card,.activity .timeline-card{animation:fadeSlideUp .35s var(--ease) both}
.human-inbox .message-card[hidden],.activity .timeline-card[hidden]{display:none}
.human-inbox .message-card.collapsed .msg-body,.activity .timeline-card.collapsed .activity-detail{display:none}
.human-inbox .message-card.collapsed .msg-toggle::after{content:"▸ 展开"}
.human-inbox .message-card .msg-toggle::after{content:"▾ 收起"}
.msg-toggle{cursor:pointer;font-size:.78em;color:var(--accent-2);font-weight:600;margin-top:6px;display:inline-block;user-select:none}
.msg-body{margin-top:8px}
.activity-group-header{display:flex;align-items:center;gap:8px;padding:10px 14px;margin:8px 0 4px;border-radius:var(--radius-xs);background:rgba(139,92,246,.06);border:1px solid var(--line);cursor:pointer;transition:all .2s var(--ease);font-size:.82em;color:var(--muted)}
.activity-group-header:hover{background:rgba(139,92,246,.1);color:var(--ink-dim)}
.activity-group-header .group-count{font-weight:700;color:var(--accent-2)}
.activity-group-header::before{content:"▸";transition:transform .2s var(--ease);font-size:.9em}
.activity-group-header.open::before{transform:rotate(90deg)}
.activity-group-body{display:none}
.activity-group-body.open{display:block}
.activity-group-body .timeline-card{padding:8px 12px 8px 28px;font-size:.82em;background:transparent;border:0;box-shadow:none;margin:0}
.activity-group-body .timeline-card .timeline-dot{left:8px;top:12px;width:5px;height:5px;box-shadow:0 0 0 2px rgba(139,92,246,.08)}
.activity-group-body .timeline-card time{font-size:10px}
.activity-group-body .timeline-card p{font-size:.85em}
.activity-group-body .timeline-card:hover{background:rgba(255,255,255,.02);transform:none}
.simple-page{padding:24px;position:relative;z-index:1}
.form-shell,.board-shell{width:min(1140px,100%);margin:0 auto}
.feature-form{max-width:780px;margin:5vh auto;padding:28px;animation:fadeSlideUp .5s var(--ease-spring)}
.board-shell{display:grid;gap:20px}
.board-grid{grid-template-columns:minmax(0,1.35fr) minmax(240px,.75fr) minmax(240px,.75fr)}
.note-card{border:1px solid var(--line);border-radius:var(--radius-sm);padding:16px;background:rgba(22,22,24,.5);margin:12px 0;transition:all .22s var(--ease);backdrop-filter:blur(8px)}
.note-card:hover{background:var(--card-hover);border-color:var(--line-strong);transform:translateY(-1px)}
.note-meta{color:var(--muted);font-size:11px;margin-bottom:6px;font-weight:500}
.compact-list{display:grid;gap:8px;padding-left:16px;color:var(--ink-dim)}
.compact-list li{line-height:1.6;font-size:.93em}
.compact-list p{margin:4px 0 0;color:var(--muted);font-size:.88em}
.empty-row{color:var(--muted);font-style:italic}
.side-rail,.surface-card,.command-card,.hero-panel,.auth-card{transition:border-color .3s var(--ease),box-shadow .3s var(--ease)}
.surface-card:hover,.hero-panel:hover{border-color:var(--line-strong)}
.live-badge{display:inline-flex;align-items:center;gap:5px;padding:4px 9px;border-radius:999px;background:rgba(139,92,246,.1);border:1px solid rgba(139,92,246,.2);color:var(--accent-2);font-size:11px;font-weight:650}
.live-badge::before{content:"";width:5px;height:5px;border-radius:50%;background:var(--accent-2);animation:pulse 1.5s infinite}
.divider{height:1px;background:var(--line);margin:12px 0}
::-webkit-scrollbar{width:6px;height:6px}
::-webkit-scrollbar-track{background:transparent}
::-webkit-scrollbar-thumb{background:rgba(255,255,255,.1);border-radius:3px}
::-webkit-scrollbar-thumb:hover{background:rgba(255,255,255,.18)}
*:focus-visible{outline:2px solid rgba(139,92,246,.5);outline-offset:2px;border-radius:4px}
@media (max-width:1100px){.app-shell{grid-template-columns:1fr}.side-rail{position:relative;top:0;height:auto;flex-direction:row;align-items:center;gap:16px;overflow-x:auto;border-radius:20px;padding:12px 16px}.side-rail nav{display:flex;gap:2px}.rail-logout{margin-left:auto;margin-top:0;border-top:0;padding-top:0;padding-left:12px;border-left:1px solid var(--line)}.rail-brand{margin-bottom:0}.workspace-grid,.board-grid{grid-template-columns:1fr}.identity-grid,.command-card{grid-template-columns:1fr}}
@media (max-width:760px){.auth-page,.simple-page,.app-shell{padding:12px}.auth-shell{grid-template-columns:1fr}.auth-hero{min-height:auto;padding:24px;border-radius:22px}.brand-mark{margin-bottom:32px}.auth-highlights{position:static;margin-top:22px}.hero-panel,.card-heading,.section-header,.composer-actions{align-items:flex-start;flex-direction:column}.hero-status{justify-content:flex-start}.side-rail{border-radius:16px;overflow-x:auto}.side-rail nav a{white-space:nowrap;font-size:.82em}.surface-card,.command-card,.hero-panel,.auth-card{border-radius:18px;padding:18px}h1{font-size:34px}}
@media (prefers-reduced-motion:reduce){*,*::before,*::after{animation-duration:.01ms!important;animation-iteration-count:1!important;scroll-behavior:auto!important;transition:none!important}}
</style>"#
}
fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn only_text(input: Vec<UserInput>) -> String {
        match input.into_iter().next().expect("user input exists") {
            UserInput::Text { text, .. } => text,
            other => panic!("expected text input, got {other:?}"),
        }
    }

    #[test]
    fn owner_delivery_includes_dynamic_turn_context_only() {
        let text = only_text(owner_delivery_user_input(&OfficeOwnerDelivery {
            agent_id: OFFICE_CEO_AGENT_ID.to_string(),
            owner_user_id: "user_ceo".to_string(),
            message_id: "owner-msg-1".to_string(),
            reply_to_message_id: None,
            content: "评估是否上线".to_string(),
            need_reply: true,
            owner_reply_target_message_id: None,
        }));

        assert!(text.contains("office owner message from [user_ceo]"));
        assert!(text.contains("reply_to[none]"));
        assert!(text.contains("message_id[owner-msg-1]"));
        assert!(text.contains("need_reply[true]"));
        assert!(text.contains("## Office Turn Context"));
        assert!(text.contains("target_agent_id: agent_ceo"));
        assert!(text.contains("owner_message_id: owner-msg-1"));
        assert!(text.contains("owner_message_needs_reply: true"));
        assert!(text.contains("owner_reply_target_message_id: owner-msg-1"));
        assert!(!text.contains("## Office Collaboration Rules"));
        assert!(!text.contains("### Human collaboration"));
        assert!(!text.contains("### Owner completion"));
    }

    #[test]
    fn office_developer_prompt_includes_directory_owner_identity_and_collaboration_rules() {
        let temp = tempfile::tempdir().expect("tempdir");
        let directory = PersistentPilotDirectory::open(temp.path().join("store.json"))
            .expect("directory opens");
        let owner_context =
            office_owner_context_for_agent(&directory, "agent_b").expect("owner context");
        let text = office_developer_instructions_for_agent(&directory, &owner_context);
        assert!(text.contains("# Office Agent Developer Instructions"));
        assert!(text.contains("<office_owner_binding schema_version=\"1\">"));
        assert!(text.contains("agent_id: agent_b"));
        assert!(text.contains("owner_user_id: user_b"));
        assert!(!text.contains("agent_role:"));
        assert!(!text.contains("owner_profile_summary"));
        assert!(!text.contains("report_preference"));
        assert!(text.contains("## Office Directory"));
        assert!(text.contains("### Fixed agent roster"));
        assert!(text.contains("### Human owners"));
        assert!(text.contains("## Office Collaboration Rules"));
        assert!(text.contains("### Routing"));
        assert!(text.contains("### Owner completion"));
        assert!(text.contains("### Human collaboration"));
        assert!(text.contains("When the question depends on your own domain owner"));
        assert!(text.contains("user_b: owner of agent_b"));
        assert!(!text.contains("capabilities: 部署"));
        assert!(text.contains("binding_version: 1"));
        assert!(!text.contains("## Owner Reply Contract"));
        assert!(!text.contains("## Working with humans"));
    }

    #[test]
    fn wait_targets_label_uses_clear_language_for_unscoped_waits() {
        assert_eq!(wait_targets_label(&[]), "next inbox message");
        assert_eq!(wait_targets_detail(&[]), "targets: any inbox message");
    }
}
