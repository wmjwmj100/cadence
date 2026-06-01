use codex_protocol::config_types::CollaborationModeMask;
use codex_protocol::config_types::ModeKind;

use crate::swarm::complex_root_swarm_developer_instructions;
use crate::swarm::standard_root_swarm_developer_instructions;

pub(crate) fn builtin_collaboration_mode_presets() -> Vec<CollaborationModeMask> {
    vec![swarm_preset(), swarm_complex_preset()]
}

fn swarm_preset() -> CollaborationModeMask {
    CollaborationModeMask {
        name: ModeKind::Swarm.display_name().to_string(),
        mode: Some(ModeKind::Swarm),
        model: None,
        reasoning_effort: None,
        developer_instructions: Some(Some(
            standard_root_swarm_developer_instructions().to_string(),
        )),
    }
}

fn swarm_complex_preset() -> CollaborationModeMask {
    CollaborationModeMask {
        name: "Swarm Complex".to_string(),
        mode: Some(ModeKind::Swarm),
        model: None,
        reasoning_effort: None,
        developer_instructions: Some(Some(
            complex_root_swarm_developer_instructions().to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::swarm::standard_root_swarm_developer_instructions;
    use crate::swarm::swarm_developer_instructions_for_session_source as resolve_swarm_prompt;
    use codex_protocol::protocol::SessionSource;
    use codex_protocol::protocol::SubAgentSource;

    #[test]
    fn preset_names_use_mode_display_names() {
        pretty_assertions::assert_eq!(swarm_preset().name, ModeKind::Swarm.display_name());
        pretty_assertions::assert_eq!(swarm_complex_preset().name, "Swarm Complex");
    }

    #[test]
    fn swarm_preset_includes_instructions() {
        let instructions = swarm_preset()
            .developer_instructions
            .expect("swarm preset should include instructions")
            .expect("swarm instructions should be set");
        assert!(instructions.contains("# Collaboration Mode: Swarm"));
    }

    #[test]
    fn swarm_complex_preset_includes_complex_instructions() {
        let instructions = swarm_complex_preset()
            .developer_instructions
            .expect("swarm complex preset should include instructions")
            .expect("swarm complex instructions should be set");
        assert!(instructions.contains("# Collaboration Mode: Swarm Complex"));
    }

    #[test]
    fn swarm_session_source_uses_main_or_sub_prompt() {
        pretty_assertions::assert_eq!(
            resolve_swarm_prompt(&SessionSource::Exec, false),
            standard_root_swarm_developer_instructions()
        );
        pretty_assertions::assert_ne!(
            resolve_swarm_prompt(&SessionSource::SubAgent(SubAgentSource::Review), false),
            resolve_swarm_prompt(&SessionSource::SubAgent(SubAgentSource::Review), true)
        );
        assert!(
            resolve_swarm_prompt(&SessionSource::SubAgent(SubAgentSource::Review), true)
                .contains("Verification Status Vocabulary")
        );
    }
}
