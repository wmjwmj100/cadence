use codex_protocol::openai_models::ConfigShellToolType;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelInstructionsVariables;
use codex_protocol::openai_models::ModelMessages;
use codex_protocol::openai_models::ModelVisibility;
use codex_protocol::openai_models::TruncationMode;
use codex_protocol::openai_models::TruncationPolicyConfig;
use codex_protocol::openai_models::default_input_modalities;

use crate::config::Config;
use crate::features::Feature;
use crate::truncate::approx_bytes_for_tokens;
use tracing::warn;

pub const BASE_INSTRUCTIONS: &str = include_str!("../../prompt.md");
pub(crate) const PERSISTENCE_VALIDATION_GUIDANCE: &str = "## Persistence and verification\n\n- Do not treat immediate success after execution as completion. For services, deployments, and environment changes, verify persistence in a separate check using the same entrypoint, protocol, and visibility that the user or verifier will use.\n- For live services that must outlive your final answer, use a handoff-safe owner such as an existing supervisor/service manager or a detached session/process group (for example `setsid`/`os.setsid` on Linux); a normal background job, `nohup`, `disown`, or an active tool/PTY session is not enough. If no handoff-safe owner is available, state the blocker instead of claiming completion.";
const DEFAULT_PERSONALITY_HEADER: &str = "You are Codex, a coding agent based on GPT-5. You and the user share the same workspace and collaborate to achieve the user's goals.";
const LOCAL_FRIENDLY_TEMPLATE: &str =
    "You optimize for team morale and being a supportive teammate as much as code quality.";
const LOCAL_PRAGMATIC_TEMPLATE: &str = "You are a deeply pragmatic, effective software engineer.";
const PERSONALITY_PLACEHOLDER: &str = "{{ personality }}";

fn append_persistence_validation_guidance(text: &str) -> String {
    if text.contains(PERSISTENCE_VALIDATION_GUIDANCE) {
        text.to_string()
    } else if text.trim().is_empty() {
        PERSISTENCE_VALIDATION_GUIDANCE.to_string()
    } else {
        format!("{text}\n\n{PERSISTENCE_VALIDATION_GUIDANCE}")
    }
}

pub(crate) fn with_config_overrides(mut model: ModelInfo, config: &Config) -> ModelInfo {
    if let Some(supports_reasoning_summaries) = config.model_supports_reasoning_summaries {
        model.supports_reasoning_summaries = supports_reasoning_summaries;
    }
    if let Some(context_window) = config.model_context_window {
        model.context_window = Some(context_window);
    }
    if let Some(auto_compact_token_limit) = config.model_auto_compact_token_limit {
        model.auto_compact_token_limit = Some(auto_compact_token_limit);
    }
    if let Some(token_limit) = config.tool_output_token_limit {
        model.truncation_policy = match model.truncation_policy.mode {
            TruncationMode::Bytes => {
                let byte_limit =
                    i64::try_from(approx_bytes_for_tokens(token_limit)).unwrap_or(i64::MAX);
                TruncationPolicyConfig::bytes(byte_limit)
            }
            TruncationMode::Tokens => {
                let limit = i64::try_from(token_limit).unwrap_or(i64::MAX);
                TruncationPolicyConfig::tokens(limit)
            }
        };
    }

    if let Some(base_instructions) = &config.base_instructions {
        model.base_instructions = base_instructions.clone();
        model.model_messages = None;
    } else if !config.features.enabled(Feature::Personality) {
        model.model_messages = None;
    }

    if config.base_instructions.is_none() {
        model.base_instructions = append_persistence_validation_guidance(&model.base_instructions);
        if let Some(model_messages) = model.model_messages.as_mut()
            && let Some(template) = model_messages.instructions_template.as_mut()
        {
            *template = append_persistence_validation_guidance(template);
        }
    }

    model
}

/// Build a minimal fallback model descriptor for missing/unknown slugs.
pub(crate) fn model_info_from_slug(slug: &str) -> ModelInfo {
    warn!("Unknown model {slug} is used. This will use fallback model metadata.");
    ModelInfo {
        slug: slug.to_string(),
        display_name: slug.to_string(),
        description: None,
        default_reasoning_level: None,
        supported_reasoning_levels: Vec::new(),
        shell_type: ConfigShellToolType::Default,
        visibility: ModelVisibility::None,
        supported_in_api: true,
        priority: 99,
        upgrade: None,
        base_instructions: BASE_INSTRUCTIONS.to_string(),
        model_messages: local_personality_messages_for_slug(slug),
        supports_reasoning_summaries: false,
        support_verbosity: false,
        default_verbosity: None,
        apply_patch_tool_type: None,
        truncation_policy: TruncationPolicyConfig::bytes(10_000),
        supports_parallel_tool_calls: false,
        context_window: Some(272_000),
        auto_compact_token_limit: None,
        effective_context_window_percent: 95,
        experimental_supported_tools: Vec::new(),
        input_modalities: default_input_modalities(),
        prefer_websockets: false,
    }
}

fn local_personality_messages_for_slug(slug: &str) -> Option<ModelMessages> {
    match slug {
        "gpt-5.2-codex" | "exp-codex-personality" => Some(ModelMessages {
            instructions_template: Some(format!(
                "{DEFAULT_PERSONALITY_HEADER}\n\n{PERSONALITY_PLACEHOLDER}\n\n{BASE_INSTRUCTIONS}"
            )),
            instructions_variables: Some(ModelInstructionsVariables {
                personality_default: Some(String::new()),
                personality_friendly: Some(LOCAL_FRIENDLY_TEMPLATE.to_string()),
                personality_pragmatic: Some(LOCAL_PRAGMATIC_TEMPLATE.to_string()),
            }),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_config;
    use crate::features::Feature;

    #[test]
    fn with_config_overrides_appends_persistence_guidance_to_base_instructions() {
        let config = test_config();
        let model = with_config_overrides(model_info_from_slug("gpt-5.1"), &config);

        assert!(
            model
                .base_instructions
                .contains(PERSISTENCE_VALIDATION_GUIDANCE)
        );
        assert_eq!(
            model
                .base_instructions
                .matches(PERSISTENCE_VALIDATION_GUIDANCE)
                .count(),
            1
        );
    }

    #[test]
    fn with_config_overrides_appends_persistence_guidance_to_personality_templates() {
        let mut config = test_config();
        config.features.enable(Feature::Personality);
        let model = with_config_overrides(model_info_from_slug("gpt-5.2-codex"), &config);
        let template = model
            .model_messages
            .and_then(|messages| messages.instructions_template)
            .expect("personality template");

        assert!(template.contains(PERSISTENCE_VALIDATION_GUIDANCE));
        assert_eq!(template.matches(PERSISTENCE_VALIDATION_GUIDANCE).count(), 1);
    }

    #[test]
    fn explicit_base_instructions_override_skips_persistence_guidance() {
        let mut config = test_config();
        config.base_instructions = Some("override instructions".to_string());
        let model = with_config_overrides(model_info_from_slug("gpt-5.1"), &config);

        assert_eq!(model.base_instructions, "override instructions");
        assert!(model.model_messages.is_none());
    }
}
