use std::collections::HashMap;
use std::collections::VecDeque;

use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct ParticipantId(String);

impl ParticipantId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for ParticipantId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for ParticipantId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParticipantKind {
    Agent,
    Human,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Participant {
    pub id: ParticipantId,
    pub kind: ParticipantKind,
    pub display_name: String,
}

impl Participant {
    pub fn agent(id: impl Into<ParticipantId>, display_name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            kind: ParticipantKind::Agent,
            display_name: display_name.into(),
        }
    }

    pub fn human(id: impl Into<ParticipantId>, display_name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            kind: ParticipantKind::Human,
            display_name: display_name.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CollabMessage {
    pub message_id: String,
    pub from: ParticipantId,
    pub to: ParticipantId,
    pub content: String,
    pub need_reply: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to_message_id: Option<String>,
}

impl CollabMessage {
    pub fn new(
        message_id: impl Into<String>,
        from: impl Into<ParticipantId>,
        to: impl Into<ParticipantId>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            message_id: message_id.into(),
            from: from.into(),
            to: to.into(),
            content: content.into(),
            need_reply: false,
            reply_to_message_id: None,
        }
    }

    pub fn need_reply(mut self) -> Self {
        self.need_reply = true;
        self
    }

    pub fn reply_to(mut self, message_id: impl Into<String>) -> Self {
        self.reply_to_message_id = Some(message_id.into());
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageDelivery {
    AgentQueued { target_id: ParticipantId },
    HumanQueued { target_id: ParticipantId },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageHubError {
    UnknownSender { sender_id: ParticipantId },
    UnknownTarget { target_id: ParticipantId },
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OfficeMessageHub {
    participants: HashMap<ParticipantId, Participant>,
    inboxes: HashMap<ParticipantId, VecDeque<CollabMessage>>,
    obligations: ReplyObligationBook,
}

impl OfficeMessageHub {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, participant: Participant) -> Option<Participant> {
        self.participants
            .insert(participant.id.clone(), participant)
    }

    pub fn resolve(&self, target_id: &ParticipantId) -> Option<&Participant> {
        self.participants.get(target_id)
    }

    pub fn send(&mut self, message: CollabMessage) -> Result<MessageDelivery, MessageHubError> {
        if !self.participants.contains_key(&message.from) {
            return Err(MessageHubError::UnknownSender {
                sender_id: message.from,
            });
        }

        let target = self.participants.get(&message.to).cloned().ok_or_else(|| {
            MessageHubError::UnknownTarget {
                target_id: message.to.clone(),
            }
        })?;

        self.obligations.record_message(&message);
        self.inboxes
            .entry(message.to.clone())
            .or_default()
            .push_back(message);

        match target.kind {
            ParticipantKind::Agent => Ok(MessageDelivery::AgentQueued {
                target_id: target.id,
            }),
            ParticipantKind::Human => Ok(MessageDelivery::HumanQueued {
                target_id: target.id,
            }),
        }
    }

    pub fn pop_for(&mut self, target_id: &ParticipantId) -> Option<CollabMessage> {
        self.inboxes
            .get_mut(target_id)
            .and_then(VecDeque::pop_front)
    }

    pub fn pop_for_from(
        &mut self,
        target_id: &ParticipantId,
        sender_id: &ParticipantId,
    ) -> Option<CollabMessage> {
        let inbox = self.inboxes.get_mut(target_id)?;
        let index = inbox
            .iter()
            .position(|message| &message.from == sender_id)?;
        inbox.remove(index)
    }

    pub fn queued_count(&self, target_id: &ParticipantId) -> usize {
        self.inboxes.get(target_id).map_or(0, VecDeque::len)
    }

    pub fn messages_for(&self, target_id: &ParticipantId) -> Vec<CollabMessage> {
        self.inboxes
            .get(target_id)
            .map(|messages| messages.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn reply_obligation(&self, message_id: &str) -> Option<&ReplyObligation> {
        self.obligations.get(message_id)
    }

    pub fn snapshot(&self) -> OfficeMessageHubSnapshot {
        OfficeMessageHubSnapshot {
            participants: self
                .participants
                .iter()
                .map(|(id, participant)| (id.as_str().to_string(), participant.clone()))
                .collect(),
            inboxes: self
                .inboxes
                .iter()
                .map(|(id, messages)| (id.as_str().to_string(), messages.clone()))
                .collect(),
            obligations: self.obligations.clone(),
        }
    }

    pub fn from_snapshot(snapshot: OfficeMessageHubSnapshot) -> Self {
        Self {
            participants: snapshot
                .participants
                .into_iter()
                .map(|(id, participant)| (ParticipantId::new(id), participant))
                .collect(),
            inboxes: snapshot
                .inboxes
                .into_iter()
                .map(|(id, messages)| (ParticipantId::new(id), messages))
                .collect(),
            obligations: snapshot.obligations,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OfficeMessageHubSnapshot {
    pub participants: HashMap<String, Participant>,
    pub inboxes: HashMap<String, VecDeque<CollabMessage>>,
    pub obligations: ReplyObligationBook,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReplyObligation {
    pub message_id: String,
    pub requester: ParticipantId,
    pub target: ParticipantId,
    pub resolved_by: Option<String>,
}

impl ReplyObligation {
    pub fn resolved(&self) -> bool {
        self.resolved_by.is_some()
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ReplyObligationBook {
    obligations: HashMap<String, ReplyObligation>,
}

impl ReplyObligationBook {
    pub fn record_message(&mut self, message: &CollabMessage) {
        if message.need_reply {
            self.obligations.insert(
                message.message_id.clone(),
                ReplyObligation {
                    message_id: message.message_id.clone(),
                    requester: message.from.clone(),
                    target: message.to.clone(),
                    resolved_by: None,
                },
            );
        }
        if let Some(reply_to_message_id) = &message.reply_to_message_id {
            if let Some(obligation) = self.obligations.get_mut(reply_to_message_id) {
                if obligation.requester == message.to && obligation.target == message.from {
                    obligation.resolved_by = Some(message.message_id.clone());
                }
            }
        }
    }

    pub fn get(&self, message_id: &str) -> Option<&ReplyObligation> {
        self.obligations.get(message_id)
    }
}
