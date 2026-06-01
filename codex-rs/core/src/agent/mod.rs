pub(crate) mod context;
pub(crate) mod control;
mod guards;
pub(crate) mod role;
pub(crate) mod status;

pub(crate) use codex_protocol::protocol::AgentStatus;
pub(crate) use context::build_agent_context_developer_instructions_for_agent;
pub(crate) use context::refresh_agent_automatic_prompt_once_per_day_for_agent;
pub(crate) use context::write_agent_owner_profile_prompt_for_agent;
pub(crate) use control::AgentControl;
pub(crate) use control::PRIMARY_AGENT_NAME;
pub(crate) use control::UNNAMED_AGENT_NAME;
#[cfg(test)]
pub(crate) use guards::MAX_THREAD_SPAWN_DEPTH;
pub(crate) use guards::exceeds_thread_spawn_depth_limit;
pub(crate) use guards::next_thread_spawn_depth;
pub(crate) use role::AgentRole;
pub(crate) use status::agent_status_from_event;
pub(crate) use status::external_state_from_status;
