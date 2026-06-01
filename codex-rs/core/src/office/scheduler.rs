use std::collections::BTreeSet;

use serde::Deserialize;
use serde::Serialize;

use super::OFFICE_SCHEDULER_SYSTEM_PROMPT;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentSummary {
    pub agent_id: String,
    pub summary: String,
    pub topics: BTreeSet<String>,
}

impl AgentSummary {
    pub fn new(
        agent_id: impl Into<String>,
        summary: impl Into<String>,
        topics: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            agent_id: agent_id.into(),
            summary: summary.into(),
            topics: topics.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CollaborationOpportunity {
    pub participants: (String, String),
    pub shared_topics: Vec<String>,
    pub reason: String,
    pub expected_gain: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OpportunityScheduler;

impl OpportunityScheduler {
    pub fn system_prompt(&self) -> &'static str {
        OFFICE_SCHEDULER_SYSTEM_PROMPT
    }

    pub fn discover(&self, summaries: &[AgentSummary]) -> Vec<CollaborationOpportunity> {
        let mut opportunities = Vec::new();
        for (left_index, left) in summaries.iter().enumerate() {
            if is_low_value_blocker(&left.summary) {
                continue;
            }
            for right in summaries.iter().skip(left_index + 1) {
                if is_low_value_blocker(&right.summary) {
                    continue;
                }
                let shared_topics = left
                    .topics
                    .intersection(&right.topics)
                    .cloned()
                    .collect::<Vec<_>>();
                if shared_topics.is_empty() {
                    continue;
                }
                opportunities.push(CollaborationOpportunity {
                    participants: (left.agent_id.clone(), right.agent_id.clone()),
                    reason: format!("shared high-value topics: {}", shared_topics.join(", ")),
                    expected_gain: format!(
                        "{} and {} may combine findings around {} for a higher-value outcome",
                        left.agent_id,
                        right.agent_id,
                        shared_topics.join(", ")
                    ),
                    shared_topics,
                });
            }
        }
        opportunities
    }
}

fn is_low_value_blocker(summary: &str) -> bool {
    let lowered = summary.to_ascii_lowercase();
    lowered.contains("waiting")
        || lowered.contains("blocked")
        || summary.contains("等待")
        || summary.contains("阻塞")
        || summary.contains("催促")
}
