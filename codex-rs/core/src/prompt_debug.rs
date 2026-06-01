use std::sync::Arc;

use crate::error::Result as CodexResult;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::user_input::UserInput;

use crate::AuthManager;
use crate::codex::build_prompt_debug_input;
use crate::config::Config;
use crate::thread_manager::ThreadManager;

/// Build the model-visible `input` list for a single debug turn.
#[doc(hidden)]
pub async fn build_prompt_input(
    mut config: Config,
    input: Vec<UserInput>,
) -> CodexResult<Vec<ResponseItem>> {
    config.ephemeral = true;

    let auth_manager = AuthManager::shared(
        config.codex_home.clone(),
        /*enable_codex_api_key_env*/ false,
        config.cli_auth_credentials_store_mode,
    );
    auth_manager.set_forced_chatgpt_workspace_id(config.forced_chatgpt_workspace_id.clone());

    let thread_manager = ThreadManager::new(
        config.codex_home.clone(),
        Arc::clone(&auth_manager),
        SessionSource::Exec,
    );
    let thread = thread_manager.start_thread(config).await?;

    let output = build_prompt_debug_input(Arc::clone(thread.thread.session()), input).await;
    let _ = thread.thread.submit(crate::protocol::Op::Shutdown).await;
    let _ = thread_manager.remove_thread(&thread.thread_id).await;
    output
}

#[cfg(test)]
mod tests {
    use super::build_prompt_input;
    use crate::config::test_config;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::ResponseItem;
    use codex_protocol::user_input::UserInput;
    use pretty_assertions::assert_eq;

    #[tokio::test]
    async fn build_prompt_input_includes_context_and_user_message() {
        let codex_home = tempfile::tempdir().expect("create codex home");
        let cwd = tempfile::tempdir().expect("create cwd");
        let mut config = test_config();
        config.codex_home = codex_home.path().to_path_buf();
        config.cwd = cwd.path().canonicalize().expect("absolute cwd");
        config.user_instructions = Some("Project-specific test instructions".to_string());

        let input = build_prompt_input(
            config,
            vec![UserInput::Text {
                text: "hello from debug prompt".to_string(),
                text_elements: Vec::new(),
            }],
        )
        .await
        .expect("build prompt input");

        let last_item = input.last().expect("user message present");
        let ResponseItem::Message { role, content, .. } = last_item else {
            panic!("expected user message, got {last_item:?}");
        };
        assert_eq!(role, "user");
        assert!(matches!(
            content.as_slice(),
            [ContentItem::InputText { text }]
                if text.contains("hello from debug prompt")
                    && text.contains("First, analyze the core objective and complexity of the task.")
        ));
        assert!(input.iter().any(|item| {
            let ResponseItem::Message { content, .. } = item else {
                return false;
            };
            content.iter().any(|content_item| {
                matches!(
                    content_item,
                    ContentItem::InputText { text } | ContentItem::OutputText { text }
                        if text.contains("Project-specific test instructions")
                )
            })
        }));
    }
}
