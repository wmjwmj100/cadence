const OWNER_BINDING_TEMPLATE: &str = include_str!("../../templates/office/owner_binding.md");
const OFFICE_COLLABORATION_RULES_TEMPLATE: &str =
    include_str!("../../templates/office/office_collaboration_rules.md");
const FIXED_ROSTER_TEMPLATE: &str = include_str!("../../templates/office/fixed_roster.md");
const OWNER_TURN_CONTEXT_TEMPLATE: &str =
    include_str!("../../templates/office/owner_turn_context.md");

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OfficeOwnerPromptContext {
    pub agent_id: String,
    pub owner_user_id: String,
    pub binding_type: String,
    pub binding_status: String,
    pub binding_version: u64,
}

pub(crate) fn render_office_agent_developer_instructions(
    owner_context: &OfficeOwnerPromptContext,
    human_participants: &str,
) -> String {
    [
        "# Office Agent Developer Instructions".to_string(),
        render_owner_binding(owner_context),
        render_office_directory(human_participants),
        OFFICE_COLLABORATION_RULES_TEMPLATE.trim().to_string(),
    ]
    .join("\n\n")
}

pub(crate) fn merge_office_developer_instructions(
    existing: Option<String>,
    office_prompt: String,
) -> Option<String> {
    match existing {
        Some(existing) if !existing.trim().is_empty() => {
            Some(format!("{}\n\n{}", existing.trim_end(), office_prompt))
        }
        _ => Some(office_prompt),
    }
}

pub(crate) fn render_owner_turn_context(
    agent_id: &str,
    owner_user_id: &str,
    owner_message_id: &str,
    reply_to_message_id: &str,
    owner_message_needs_reply: bool,
    owner_reply_target_message_id: Option<&str>,
) -> String {
    let rendered = OWNER_TURN_CONTEXT_TEMPLATE
        .replace("{{AGENT_ID}}", &escape_xml_text(agent_id))
        .replace("{{OWNER_USER_ID}}", &escape_xml_text(owner_user_id))
        .replace(
            "{{INBOUND_REPLY_TO_MESSAGE_ID}}",
            &escape_xml_text(reply_to_message_id),
        )
        .replace("{{OWNER_MESSAGE_ID}}", &escape_xml_text(owner_message_id))
        .replace(
            "{{OWNER_MESSAGE_NEEDS_REPLY}}",
            if owner_message_needs_reply {
                "true"
            } else {
                "false"
            },
        )
        .replace(
            "{{OWNER_REPLY_TARGET_MESSAGE_ID}}",
            &escape_xml_text(owner_reply_target_message_id.unwrap_or("none")),
        );
    format!("\n\n{}", rendered.trim())
}

fn render_owner_binding(context: &OfficeOwnerPromptContext) -> String {
    OWNER_BINDING_TEMPLATE
        .replace("{{AGENT_ID}}", &escape_xml_text(&context.agent_id))
        .replace(
            "{{OWNER_USER_ID}}",
            &escape_xml_text(&context.owner_user_id),
        )
        .replace("{{BINDING_TYPE}}", &escape_xml_text(&context.binding_type))
        .replace(
            "{{BINDING_STATUS}}",
            &escape_xml_text(&context.binding_status),
        )
        .replace("{{BINDING_VERSION}}", &context.binding_version.to_string())
        .trim()
        .to_string()
}

fn render_office_directory(human_participants: &str) -> String {
    FIXED_ROSTER_TEMPLATE
        .replace("{{HUMAN_PARTICIPANTS}}", human_participants.trim())
        .trim()
        .to_string()
}

fn escape_xml_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_context() -> OfficeOwnerPromptContext {
        OfficeOwnerPromptContext {
            agent_id: "agent_b".to_string(),
            owner_user_id: "user_b".to_string(),
            binding_type: "PrimaryOwner".to_string(),
            binding_status: "Active".to_string(),
            binding_version: 3,
        }
    }

    #[test]
    fn office_developer_instructions_are_composed_from_md_templates() {
        let text = render_office_agent_developer_instructions(
            &sample_context(),
            "- user_b: owner of agent_b; role: Infra.",
        );

        assert!(text.contains("# Office Agent Developer Instructions"));
        assert!(text.contains("<office_owner_binding schema_version=\"1\">"));
        assert!(text.contains("agent_id: agent_b"));
        assert!(text.contains("owner_user_id: user_b"));
        assert!(text.contains("binding_version: 3"));
        assert!(!text.contains("agent_role:"));
        assert!(!text.contains("owner_profile_summary"));
        assert!(!text.contains("report_preference"));
        assert!(text.contains("## Office Directory"));
        assert!(text.contains("### Fixed agent roster"));
        assert!(text.contains("agent_ceo: CEO"));
        assert!(text.contains("### Human owners"));
        assert!(text.contains("## Office Collaboration Rules"));
        assert!(text.contains("### Routing"));
        assert!(text.contains("### Owner completion"));
        assert!(text.contains("### Human collaboration"));
        assert!(text.contains("Humans are strong working partners"));
        assert!(text.contains("Do not reduce human interaction to permission requests"));
        assert!(text.contains("human challenge review after agent-agent synthesis"));
        assert!(text.contains("State your current understanding or working hypothesis"));
        assert!(text.contains("user_b: owner of agent_b"));
        assert!(!text.contains("{{HUMAN_PARTICIPANTS}}"));
    }

    #[test]
    fn owner_turn_context_renders_dynamic_message_metadata_only() {
        let text = render_owner_turn_context(
            "agent_b",
            "user_b",
            "owner-msg-1",
            "reply-msg-0",
            true,
            Some("owner-msg-1"),
        );

        assert!(text.contains("## Office Turn Context"));
        assert!(text.contains("target_agent_id: agent_b"));
        assert!(text.contains("owner_user_id: user_b"));
        assert!(text.contains("inbound_reply_to_message_id: reply-msg-0"));
        assert!(text.contains("owner_message_id: owner-msg-1"));
        assert!(text.contains("owner_message_needs_reply: true"));
        assert!(text.contains("owner_reply_target_message_id: owner-msg-1"));
    }
}
