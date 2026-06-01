//! AI office product domain primitives.
//!
//! This module is intentionally independent from model sampling and sandbox execution.
//! It gives the office product a stable world model while Codex Swarm remains the
//! canonical collaboration runtime underneath.

mod blackboard;
mod memory;
mod message;
mod persistence;
mod pilot;
mod prompts;
mod runtime;
mod scheduler;
mod snapshot;
mod timeline;
mod tools;
mod wait_policy;
mod web;
mod workspace;

pub use blackboard::BlackboardNote;
pub use blackboard::OfficeBlackboard;
pub use blackboard::OfficeBlackboardError;
pub use blackboard::OfficeBlackboardStore;
pub use blackboard::ProjectState;
pub use memory::AgentMemoryError;
pub use memory::AgentMemoryPaths;
pub use memory::AgentMemoryStore;
pub use message::CollabMessage;
pub use message::MessageDelivery;
pub use message::MessageHubError;
pub use message::OfficeMessageHub;
pub use message::OfficeMessageHubSnapshot;
pub use message::Participant;
pub use message::ParticipantId;
pub use message::ParticipantKind;
pub use message::ReplyObligation;
pub use message::ReplyObligationBook;
pub use persistence::PersistentPilotDirectory;
pub use persistence::PilotStoreError;
pub use persistence::PilotStoreSnapshot;
pub use persistence::PilotWebSession;
pub use pilot::AgentOwnerBinding;
pub use pilot::AgentOwnerBindingStatus;
pub use pilot::AgentOwnerBindingType;
pub use pilot::AgentProfile;
pub use pilot::DailyReflection;
pub use pilot::HumanProfile;
pub use pilot::PilotAccount;
pub use pilot::PilotDirectory;
pub use pilot::PilotSession;
pub use pilot::UserProfileEvidence;
pub use pilot::UserProfileFact;
pub use pilot::UserProfileFactCategory;
pub use pilot::UserProfileFactStatus;
pub use runtime::AgentRuntimeRecord;
pub use runtime::AgentRuntimeState;
pub use runtime::OfficeActivityEntry;
pub use runtime::OfficeActivityKind;
pub use runtime::OfficeRuntimeSnapshot;
pub use runtime::OfficeRuntimeStore;
pub use runtime::OfficeRuntimeStoreError;
pub use scheduler::AgentSummary;
pub use scheduler::CollaborationOpportunity;
pub use scheduler::OpportunityScheduler;
pub use snapshot::OfficeAgentContextSnapshot;
pub use timeline::OfficeSummary;
pub use timeline::OfficeTimeline;
pub use timeline::OfficeTimelineEvent;
pub use timeline::StepSummaryClock;
pub use tools::GatewayDecision;
pub use tools::ToolCapabilityPolicy;
pub use wait_policy::HumanWaitBook;
pub use wait_policy::HumanWaitTicket;
pub use wait_policy::WaitOutcome;
pub use wait_policy::WaitPolicy;
pub use web::OfficeAgentExternalState;
pub use web::OfficeOwnerDelivery;
pub use web::OfficeRuntimeBridge;
pub use web::OfficeWebApp;
pub use web::OfficeWebConfig;
pub use web::OfficeWebError;
pub use workspace::AgentWorkspace;
pub use workspace::WorkspaceArea;
pub use workspace::WorkspaceItem;
pub use workspace::WorkspaceItemKind;
pub use workspace::WorkspacePolicy;

pub const OFFICE_AGENT_SYSTEM_PROMPT_RULES: &str = r#"你拥有私有工作空间和公共工作空间。主人上传的文件先进入私有空间；只有接收主人材料、抽取个人材料、或完全个人化且与团队无关的临时处理，才留在私有空间。只要工作和团队协作、共同任务、共享产物、其他 Agent 或人类可能受益的信息有关，就默认进入公共空间完成。若私有材料后来与公共任务相关，必须把相关摘要、结论或产物迁移到公共空间。"#;

pub const OFFICE_SCHEDULER_SYSTEM_PROMPT: &str = r#"你的价值不是提醒简单阻塞或催促普通回复；每个 Agent 自己能感知这些。你的价值是基于 summary 列表发现 1+1 大于 2 的交流机会：两个 Agent 或人类可能互相启发、合并成果、减少重复劳动、或产生更高价值判断时，才提出协作建议。"#;

pub(crate) const OFFICE_FIXED_AGENT_IDS: [&str; 6] = [
    "agent_ceo",
    "agent_a",
    "agent_b",
    "agent_c",
    "agent_d",
    "agent_e",
];
