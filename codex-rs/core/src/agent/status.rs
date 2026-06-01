use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentExternalState {
    Idle,
    Working,
    Waiting,
}

impl AgentExternalState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Waiting => "waiting",
        }
    }
}

pub(crate) fn external_state_from_status(
    status: &AgentStatus,
    has_pending_required_reply: bool,
) -> AgentExternalState {
    if has_pending_required_reply {
        return AgentExternalState::Waiting;
    }

    match status {
        AgentStatus::Running | AgentStatus::PendingInit => AgentExternalState::Working,
        AgentStatus::Interrupted
        | AgentStatus::Completed(_)
        | AgentStatus::Errored(_)
        | AgentStatus::Shutdown
        | AgentStatus::NotFound => AgentExternalState::Idle,
    }
}

/// Derive the next agent status from a single emitted event.
/// Returns `None` when the event does not affect status tracking.
pub(crate) fn agent_status_from_event(msg: &EventMsg) -> Option<AgentStatus> {
    match msg {
        EventMsg::TurnStarted(_) => Some(AgentStatus::Running),
        EventMsg::TurnComplete(ev) => Some(AgentStatus::Completed(ev.last_agent_message.clone())),
        EventMsg::TurnAborted(ev) => match ev.reason {
            codex_protocol::protocol::TurnAbortReason::Interrupted => {
                Some(AgentStatus::Interrupted)
            }
            _ => Some(AgentStatus::Errored(format!("{:?}", ev.reason))),
        },
        EventMsg::Error(ev) => Some(AgentStatus::Errored(ev.message.clone())),
        EventMsg::ShutdownComplete => Some(AgentStatus::Shutdown),
        _ => None,
    }
}

pub(crate) fn is_final(status: &AgentStatus) -> bool {
    !matches!(
        status,
        AgentStatus::PendingInit | AgentStatus::Running | AgentStatus::Interrupted
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_state_mapping_has_only_idle_working_waiting() {
        let cases = [
            (AgentStatus::PendingInit, false, "working"),
            (AgentStatus::Running, false, "working"),
            (AgentStatus::Interrupted, false, "idle"),
            (
                AgentStatus::Completed(Some("done".to_string())),
                false,
                "idle",
            ),
            (AgentStatus::Errored("boom".to_string()), false, "idle"),
            (AgentStatus::Shutdown, false, "idle"),
            (AgentStatus::NotFound, false, "idle"),
            (AgentStatus::Running, true, "waiting"),
            (AgentStatus::Completed(None), true, "waiting"),
        ];

        for (status, has_pending_required_reply, expected) in cases {
            assert_eq!(
                external_state_from_status(&status, has_pending_required_reply).as_str(),
                expected
            );
        }
    }
}
