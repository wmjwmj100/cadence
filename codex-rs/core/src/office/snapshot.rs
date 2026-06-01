use serde::Deserialize;
use serde::Serialize;

use super::AgentMemoryPaths;
use super::AgentRuntimeRecord;
use super::OfficeBlackboard;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OfficeAgentContextSnapshot {
    pub agent_id: String,
    pub owner_user_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime: Option<AgentRuntimeRecord>,
    pub inbox_queued_count: usize,
    pub memory_identity_path: String,
    pub memory_path: String,
    pub owner_profile_path: String,
    pub peer_notes_path: String,
    pub blackboard: OfficeBlackboard,
}

impl OfficeAgentContextSnapshot {
    pub fn new(
        agent_id: impl Into<String>,
        owner_user_id: impl Into<String>,
        runtime: Option<AgentRuntimeRecord>,
        inbox_queued_count: usize,
        memory_paths: AgentMemoryPaths,
        blackboard: OfficeBlackboard,
    ) -> Self {
        Self {
            agent_id: agent_id.into(),
            owner_user_id: owner_user_id.into(),
            runtime,
            inbox_queued_count,
            memory_identity_path: memory_paths.identity.display().to_string(),
            memory_path: memory_paths.memory.display().to_string(),
            owner_profile_path: memory_paths.owner_profile.display().to_string(),
            peer_notes_path: memory_paths.peer_notes.display().to_string(),
            blackboard,
        }
    }
}
