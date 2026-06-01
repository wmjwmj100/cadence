use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use chrono::DateTime;
use chrono::Utc;
use codex_protocol::ThreadId;
use serde::Deserialize;
use serde::Serialize;

pub const OFFICE_ACTIVITY_LIMIT_PER_AGENT: usize = 50;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRuntimeState {
    Idle,
    Working,
    Waiting,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentRuntimeRecord {
    pub agent_id: String,
    pub owner_user_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<ThreadId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rollout_path: Option<PathBuf>,
    pub state: AgentRuntimeState,
    pub last_seen_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OfficeActivityKind {
    OwnerMessageQueued,
    RuntimeThreadResumed,
    RuntimeTurnStarted,
    RuntimeReasoning,
    RuntimePlan,
    RuntimeAgentMessage,
    RuntimeToolStarted,
    RuntimeToolFinished,
    RuntimeCollab,
    RuntimeWait,
    RuntimeError,
    OwnerReplyReady,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OfficeActivityEntry {
    pub sequence: u64,
    pub agent_id: String,
    pub timestamp: DateTime<Utc>,
    pub kind: OfficeActivityKind,
    pub source: String,
    pub title: String,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
}

impl AgentRuntimeRecord {
    pub fn idle(agent_id: impl Into<String>, owner_user_id: impl Into<String>) -> Self {
        Self {
            agent_id: agent_id.into(),
            owner_user_id: owner_user_id.into(),
            thread_id: None,
            rollout_path: None,
            state: AgentRuntimeState::Idle,
            last_seen_at: Utc::now(),
            waiting_reason: None,
        }
    }

    pub fn mark_thread_bound(&mut self, thread_id: ThreadId) {
        self.thread_id = Some(thread_id);
        self.state = AgentRuntimeState::Working;
        self.last_seen_at = Utc::now();
        self.waiting_reason = None;
    }

    pub fn mark_message_queued(&mut self, answered: bool) {
        self.last_seen_at = Utc::now();
        if !answered {
            self.state = AgentRuntimeState::Working;
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OfficeRuntimeSnapshot {
    #[serde(default)]
    pub agents: HashMap<String, AgentRuntimeRecord>,
    #[serde(default)]
    pub activity_by_agent: HashMap<String, Vec<OfficeActivityEntry>>,
    #[serde(default)]
    pub owner_message_status_by_agent: HashMap<String, HashMap<String, OwnerMessageStatus>>,
    #[serde(default)]
    pub next_activity_sequence: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct OwnerMessageStatus {
    pub queued: bool,
    pub reply_ready: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_content: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum OfficeRuntimeStoreError {
    #[error("failed to create office runtime directory `{path}`: {source}")]
    CreateDir { path: PathBuf, source: io::Error },
    #[error("failed to read office runtime store `{path}`: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("failed to write office runtime store `{path}`: {source}")]
    Write { path: PathBuf, source: io::Error },
    #[error("failed to serialize office runtime store `{path}`: {source}")]
    Serialize {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("failed to parse office runtime store `{path}`: {source}")]
    Deserialize {
        path: PathBuf,
        source: serde_json::Error,
    },
}

#[derive(Clone, Debug)]
pub struct OfficeRuntimeStore {
    path: PathBuf,
    snapshot: OfficeRuntimeSnapshot,
}

impl OfficeRuntimeStore {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, OfficeRuntimeStoreError> {
        let path = path.into();
        if path.exists() {
            let raw =
                fs::read_to_string(&path).map_err(|source| OfficeRuntimeStoreError::Read {
                    path: path.clone(),
                    source,
                })?;
            let snapshot = serde_json::from_str(&raw).map_err(|source| {
                OfficeRuntimeStoreError::Deserialize {
                    path: path.clone(),
                    source,
                }
            })?;
            Ok(Self { path, snapshot })
        } else {
            let store = Self {
                path,
                snapshot: OfficeRuntimeSnapshot::default(),
            };
            store.persist()?;
            Ok(store)
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn reload(&mut self) -> Result<(), OfficeRuntimeStoreError> {
        let refreshed = Self::open(self.path.clone())?;
        self.snapshot = refreshed.snapshot;
        Ok(())
    }

    pub fn snapshot(&self) -> &OfficeRuntimeSnapshot {
        &self.snapshot
    }

    pub fn get(&self, agent_id: &str) -> Option<&AgentRuntimeRecord> {
        self.snapshot.agents.get(agent_id)
    }

    pub fn activity_for_agent(&self, agent_id: &str) -> &[OfficeActivityEntry] {
        self.snapshot
            .activity_by_agent
            .get(agent_id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn has_owner_message_queued(&self, agent_id: &str, message_id: &str) -> bool {
        self.snapshot
            .owner_message_status_by_agent
            .get(agent_id)
            .and_then(|messages| messages.get(message_id))
            .is_some_and(|status| status.queued)
            || self.activity_for_agent(agent_id).iter().any(|entry| {
                entry.kind == OfficeActivityKind::OwnerMessageQueued
                    && entry.message_id.as_deref() == Some(message_id)
            })
    }

    pub fn has_owner_reply_ready(&self, agent_id: &str, message_id: &str) -> bool {
        self.snapshot
            .owner_message_status_by_agent
            .get(agent_id)
            .and_then(|messages| messages.get(message_id))
            .is_some_and(|status| status.reply_ready)
            || self.activity_for_agent(agent_id).iter().any(|entry| {
                entry.kind == OfficeActivityKind::OwnerReplyReady
                    && entry.message_id.as_deref() == Some(message_id)
            })
    }

    pub fn latest_unanswered_owner_message_id(&self, agent_id: &str) -> Option<String> {
        let from_activity = self
            .activity_for_agent(agent_id)
            .iter()
            .rev()
            .filter(|entry| entry.kind == OfficeActivityKind::OwnerMessageQueued)
            .filter_map(|entry| entry.message_id.as_deref())
            .find(|message_id| !self.has_owner_reply_ready(agent_id, message_id))
            .map(str::to_string);
        if from_activity.is_some() {
            return from_activity;
        }
        self.snapshot
            .owner_message_status_by_agent
            .get(agent_id)
            .and_then(|messages| {
                messages
                    .iter()
                    .find(|(_, status)| status.queued && !status.reply_ready)
                    .map(|(message_id, _)| message_id.clone())
            })
    }

    pub fn ensure_agent(
        &mut self,
        agent_id: impl Into<String>,
        owner_user_id: impl Into<String>,
    ) -> &mut AgentRuntimeRecord {
        let agent_id = agent_id.into();
        let owner_user_id = owner_user_id.into();
        let record = self
            .snapshot
            .agents
            .entry(agent_id.clone())
            .or_insert_with(|| AgentRuntimeRecord::idle(agent_id, owner_user_id.clone()));
        if record.owner_user_id != owner_user_id {
            record.owner_user_id = owner_user_id;
        }
        record
    }

    pub fn mark_owner_message_queued(
        &mut self,
        agent_id: &str,
        owner_user_id: &str,
        message_id: &str,
        content: &str,
    ) -> Result<AgentRuntimeRecord, OfficeRuntimeStoreError> {
        let answered = self.has_owner_reply_ready(agent_id, message_id);
        let already_queued = self.has_owner_message_queued(agent_id, message_id);
        self.snapshot
            .owner_message_status_by_agent
            .entry(agent_id.to_string())
            .or_default()
            .entry(message_id.to_string())
            .or_default()
            .queued = true;
        let record = self.ensure_agent(agent_id.to_string(), owner_user_id.to_string());
        record.mark_message_queued(answered);
        let record = record.clone();
        if !already_queued {
            self.append_activity(
                agent_id,
                OfficeActivityKind::OwnerMessageQueued,
                "owner_inbox",
                "收到主人请求",
                format!("已将主人消息 `{message_id}` 排入 Agent inbox"),
                Some(content.to_string()),
                Some(message_id.to_string()),
                None,
            );
        }
        self.persist()?;
        Ok(record)
    }

    pub fn bind_thread(
        &mut self,
        agent_id: &str,
        owner_user_id: &str,
        thread_id: ThreadId,
    ) -> Result<AgentRuntimeRecord, OfficeRuntimeStoreError> {
        let has_unanswered_owner_message =
            self.latest_unanswered_owner_message_id(agent_id).is_some();
        let record = self.ensure_agent(agent_id.to_string(), owner_user_id.to_string());
        record.mark_thread_bound(thread_id);
        if !has_unanswered_owner_message {
            record.state = AgentRuntimeState::Idle;
        }
        let record = record.clone();
        self.append_activity(
            agent_id,
            OfficeActivityKind::RuntimeThreadResumed,
            "runtime_bridge",
            "已恢复运行线程",
            format!("运行时桥接已绑定线程 `{thread_id}`，正在把请求交给 Agent 处理"),
            None,
            None,
            None,
        );
        self.persist()?;
        Ok(record)
    }

    pub fn mark_owner_reply_ready(
        &mut self,
        agent_id: &str,
        owner_user_id: &str,
        message_id: &str,
        content: &str,
    ) -> Result<AgentRuntimeRecord, OfficeRuntimeStoreError> {
        let status = self
            .snapshot
            .owner_message_status_by_agent
            .entry(agent_id.to_string())
            .or_default()
            .entry(message_id.to_string())
            .or_default();
        status.queued = true;
        status.reply_ready = true;
        status.reply_content = Some(content.to_string());
        let record = self.ensure_agent(agent_id.to_string(), owner_user_id.to_string());
        record.state = AgentRuntimeState::Idle;
        record.last_seen_at = Utc::now();
        record.waiting_reason = None;
        let record = record.clone();
        let already_recorded = self.activity_for_agent(agent_id).iter().any(|entry| {
            entry.kind == OfficeActivityKind::OwnerReplyReady
                && entry.message_id.as_deref() == Some(message_id)
                && entry.detail.as_deref() == Some(content)
        });
        if !already_recorded {
            self.append_activity(
                agent_id,
                OfficeActivityKind::OwnerReplyReady,
                "runtime_bridge",
                "Agent 已回复主人",
                format!("Agent 已完成主人消息 `{message_id}` 的处理"),
                Some(content.to_string()),
                Some(message_id.to_string()),
                None,
            );
        }
        self.persist()?;
        Ok(record)
    }

    pub fn record_runtime_event(
        &mut self,
        agent_id: &str,
        owner_user_id: &str,
        kind: OfficeActivityKind,
        source: impl Into<String>,
        title: impl Into<String>,
        summary: impl Into<String>,
        detail: Option<String>,
        message_id: Option<String>,
        tool_name: Option<String>,
    ) -> Result<AgentRuntimeRecord, OfficeRuntimeStoreError> {
        let source = source.into();
        let title = title.into();
        let summary = summary.into();
        let has_unanswered_owner_message =
            self.latest_unanswered_owner_message_id(agent_id).is_some();
        let record = self.ensure_agent(agent_id.to_string(), owner_user_id.to_string());
        match kind {
            OfficeActivityKind::RuntimeTurnStarted
            | OfficeActivityKind::RuntimeReasoning
            | OfficeActivityKind::RuntimePlan
            | OfficeActivityKind::RuntimeAgentMessage
            | OfficeActivityKind::RuntimeToolStarted
            | OfficeActivityKind::RuntimeToolFinished
            | OfficeActivityKind::RuntimeCollab
            | OfficeActivityKind::RuntimeError => {
                if has_unanswered_owner_message {
                    record.state = AgentRuntimeState::Working;
                }
                record.waiting_reason = None;
            }
            OfficeActivityKind::RuntimeWait => {
                if has_unanswered_owner_message {
                    record.state = AgentRuntimeState::Waiting;
                    record.waiting_reason = Some(summary.clone());
                } else {
                    record.waiting_reason = None;
                }
                record.last_seen_at = Utc::now();
                let record = record.clone();
                self.append_activity(
                    agent_id, kind, source, title, summary, detail, message_id, tool_name,
                );
                self.persist()?;
                return Ok(record);
            }
            OfficeActivityKind::OwnerMessageQueued
            | OfficeActivityKind::RuntimeThreadResumed
            | OfficeActivityKind::OwnerReplyReady => {}
        }
        record.last_seen_at = Utc::now();
        let record = record.clone();
        self.append_activity(
            agent_id, kind, source, title, summary, detail, message_id, tool_name,
        );
        self.persist()?;
        Ok(record)
    }

    fn append_activity(
        &mut self,
        agent_id: &str,
        kind: OfficeActivityKind,
        source: impl Into<String>,
        title: impl Into<String>,
        summary: impl Into<String>,
        detail: Option<String>,
        message_id: Option<String>,
        tool_name: Option<String>,
    ) {
        let sequence = self.snapshot.next_activity_sequence.saturating_add(1);
        self.snapshot.next_activity_sequence = sequence;
        let entries = self
            .snapshot
            .activity_by_agent
            .entry(agent_id.to_string())
            .or_default();
        entries.push(OfficeActivityEntry {
            sequence,
            agent_id: agent_id.to_string(),
            timestamp: Utc::now(),
            kind,
            source: source.into(),
            title: title.into(),
            summary: summary.into(),
            detail,
            message_id,
            tool_name,
        });
        let overflow = entries
            .len()
            .saturating_sub(OFFICE_ACTIVITY_LIMIT_PER_AGENT);
        if overflow > 0 {
            entries.drain(0..overflow);
        }
    }

    pub fn persist(&self) -> Result<(), OfficeRuntimeStoreError> {
        if let Some(parent) = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).map_err(|source| OfficeRuntimeStoreError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let body = serde_json::to_string_pretty(&self.snapshot).map_err(|source| {
            OfficeRuntimeStoreError::Serialize {
                path: self.path.clone(),
                source,
            }
        })?;
        let tmp_path = self.path.with_extension("tmp");
        fs::write(&tmp_path, body).map_err(|source| OfficeRuntimeStoreError::Write {
            path: tmp_path.clone(),
            source,
        })?;
        fs::rename(&tmp_path, &self.path).map_err(|source| OfficeRuntimeStoreError::Write {
            path: self.path.clone(),
            source,
        })?;
        Ok(())
    }
}
