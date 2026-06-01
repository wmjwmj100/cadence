use std::time::Duration;

use serde::Deserialize;
use serde::Serialize;

use super::CollabMessage;
use super::ParticipantId;
use super::ParticipantKind;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitOutcome {
    Delivered(CollabMessage),
    AgentTimeout { timeout: Duration },
    HumanSuspended,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HumanWaitTicket {
    pub waiting_agent_id: ParticipantId,
    pub human_id: ParticipantId,
    pub requested_message_id: Option<String>,
}

impl HumanWaitTicket {
    pub fn matches_message(&self, message: &CollabMessage) -> bool {
        message.from == self.human_id
            && message.to == self.waiting_agent_id
            && self.requested_message_id.as_ref().is_none_or(|message_id| {
                message.reply_to_message_id.as_deref() == Some(message_id.as_str())
            })
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HumanWaitBook {
    tickets: Vec<HumanWaitTicket>,
}

impl HumanWaitBook {
    pub fn suspend(
        &mut self,
        waiting_agent_id: impl Into<ParticipantId>,
        human_id: impl Into<ParticipantId>,
        requested_message_id: Option<String>,
    ) -> HumanWaitTicket {
        let ticket = HumanWaitTicket {
            waiting_agent_id: waiting_agent_id.into(),
            human_id: human_id.into(),
            requested_message_id,
        };
        self.tickets.push(ticket.clone());
        ticket
    }

    pub fn wake_for_message(&mut self, message: &CollabMessage) -> Option<HumanWaitTicket> {
        let index = self
            .tickets
            .iter()
            .position(|ticket| ticket.matches_message(message))?;
        Some(self.tickets.remove(index))
    }

    pub fn pending_for_agent(&self, agent_id: &ParticipantId) -> Vec<&HumanWaitTicket> {
        self.tickets
            .iter()
            .filter(|ticket| &ticket.waiting_agent_id == agent_id)
            .collect()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WaitPolicy {
    pub agent_default_timeout: Duration,
}

impl Default for WaitPolicy {
    fn default() -> Self {
        Self {
            agent_default_timeout: Duration::from_secs(7 * 60),
        }
    }
}

impl WaitPolicy {
    pub fn wait_for(
        &self,
        target_id: &ParticipantId,
        target_kind: ParticipantKind,
        inbox: &mut Vec<CollabMessage>,
    ) -> WaitOutcome {
        if let Some(index) = inbox.iter().position(|message| &message.from == target_id) {
            return WaitOutcome::Delivered(inbox.remove(index));
        }
        match target_kind {
            ParticipantKind::Agent => WaitOutcome::AgentTimeout {
                timeout: self.agent_default_timeout,
            },
            ParticipantKind::Human => WaitOutcome::HumanSuspended,
        }
    }
}
