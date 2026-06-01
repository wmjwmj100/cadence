use crate::config::Config;
use codex_protocol::openai_models::ReasoningEffort;
use serde::Deserialize;
use serde::Serialize;

/// Base instructions for the orchestrator role.
const ORCHESTRATOR_PROMPT: &str = include_str!("../../templates/agents/orchestrator.md");
/// Base instructions for the coordinator role.
const COORDINATOR_PROMPT: &str = include_str!("../../templates/agents/coordinator.md");
/// Default model override used.
// TODO(jif) update when we have something smarter.
const EXPLORER_MODEL: &str = "gpt-5.1-codex-mini";

/// Enumerated list of all supported agent roles.
const ALL_ROLES: [AgentRole; 5] = [
    AgentRole::Default,
    AgentRole::Explorer,
    AgentRole::Worker,
    AgentRole::Verifier,
    AgentRole::Coordinator,
];

/// Hard-coded agent role selection used when spawning sub-agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    /// Inherit the parent agent's configuration unchanged.
    Default,
    /// Coordination-only agent that delegates to workers.
    Orchestrator,
    /// Task-executing agent with a fixed model override.
    Worker,
    /// Task-executing agent with a fixed model override.
    Explorer,
    /// Verification-oriented agent for acceptance and artifact checking.
    Verifier,
    /// Coordination agent for multi-agent tasks.
    Coordinator,
}

/// Immutable profile data that drives per-agent configuration overrides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AgentProfile {
    /// Optional base instructions override.
    pub base_instructions: Option<&'static str>,
    /// Optional model override.
    pub model: Option<&'static str>,
    /// Optional reasoning effort override.
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Description to include in the tool specs.
    pub description: &'static str,
}

impl AgentRole {
    /// Returns the string values used by JSON schema enums.
    pub fn enum_values() -> Vec<String> {
        ALL_ROLES
            .iter()
            .filter_map(|role| {
                let description = role.profile().description;
                serde_json::to_string(role)
                    .map(|role| {
                        let description = if !description.is_empty() {
                            format!(r#", "description": {description}"#)
                        } else {
                            String::new()
                        };
                        format!(r#"{{ "name": {role}{description}}}"#)
                    })
                    .ok()
            })
            .collect()
    }

    /// Returns the hard-coded profile for this role.
    pub fn profile(self) -> AgentProfile {
        match self {
            AgentRole::Default => AgentProfile::default(),
            AgentRole::Orchestrator => AgentProfile {
                base_instructions: Some(ORCHESTRATOR_PROMPT),
                ..Default::default()
            },
            AgentRole::Worker => AgentProfile {
                description: r#"Use for execution and production work.
Typical tasks:
- Implement part of a feature
- Fix tests or bugs
- Split large refactors into independent chunks
Rules:
- Explicitly assign **ownership** of the task (files / responsibility).
- Always tell workers they are **not alone in the codebase**, and they should ignore edits made by others without touching them"#,
                ..Default::default()
            },
            AgentRole::Explorer => AgentProfile {
                model: Some(EXPLORER_MODEL),
                reasoning_effort: Some(ReasoningEffort::Medium),
                description: r#"Use `explorer` for reconnaissance, codebase scanning, and trace mapping.
Explorers are fast and authoritative.
Rules:
- Ask explorers first and precisely.
- Do not re-read or re-search code they cover.
- Prefer mapping and evidence over edits.
- Run explorers in parallel only when the slices are genuinely distinct.
- Reuse existing explorers for related questions.
                "#,
                ..Default::default()
            },
            AgentRole::Verifier => AgentProfile {
                description: r#"Use for verifier-facing validation and acceptance checks.
Typical tasks:
- Confirm exact artifact paths, outputs, and schemas
- Re-check logs, entrypoints, or benchmark constraints
- Report `verified`, `repair_request`, `not_verified`, or `blocked`
Rules:
- Prefer direct evidence over theory
- Inherit the primary agent's permissions and verify against the current artifact state
- Do not green-light work without the required check"#,
                ..Default::default()
            },
            AgentRole::Coordinator => AgentProfile {
                base_instructions: Some(COORDINATOR_PROMPT),
                description: r#"Use for coordinating multiple agents on complex tasks.
Responsibilities:
- Monitor agent progress and identify blockers
- Facilitate communication between agents
- Resolve conflicts and duplicate work
- Provide strategic guidance when needed"#,
                ..Default::default()
            },
        }
    }

    /// Applies this role's profile onto the provided config.
    pub fn apply_to_config(self, config: &mut Config) -> Result<(), String> {
        let profile = self.profile();
        if let Some(base_instructions) = profile.base_instructions {
            config.base_instructions = Some(base_instructions.to_string());
        }
        if let Some(model) = profile.model {
            config.model = Some(model.to_string());
        }
        if let Some(reasoning_effort) = profile.reasoning_effort {
            config.model_reasoning_effort = Some(reasoning_effort)
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifier_inherits_parent_sandbox_policy() {
        let mut config = crate::config::test_config();
        let inherited_policy = config.permissions.sandbox_policy.get().clone();

        AgentRole::Verifier
            .apply_to_config(&mut config)
            .expect("verifier config should apply");

        assert_eq!(config.permissions.sandbox_policy.get(), &inherited_policy);
    }
}
