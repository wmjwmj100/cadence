use std::env;

use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SpawnedAgentType;
use codex_protocol::protocol::SubAgentSource;

const WECODE_SWARM_COMPLEX_ENV_VAR: &str = "WECODE_SWARM_COMPLEX";
const CODEX_SWARM_COMPLEX_ENV_VAR: &str = "CODEX_SWARM_COMPLEX";

const COLLABORATION_MODE_SWARM_MAIN: &str =
    include_str!("../../templates/collaboration_mode/swarm_main.md");
const COLLABORATION_MODE_SWARM_MAIN_COMPLEX: &str =
    include_str!("../../templates/collaboration_mode/swarm_main_complex.md");
const COLLABORATION_MODE_SWARM_SUB: &str =
    include_str!("../../templates/collaboration_mode/swarm_sub.md");
const COLLABORATION_MODE_SWARM_SUB_COMPLEX: &str =
    include_str!("../../templates/collaboration_mode/swarm_sub_complex.md");
const COLLABORATION_MODE_SWARM_SUB_WORKER: &str =
    include_str!("../../templates/collaboration_mode/swarm_sub_worker.md");
const COLLABORATION_MODE_SWARM_SUB_WORKER_COMPLEX: &str =
    include_str!("../../templates/collaboration_mode/swarm_sub_worker_complex.md");
const COLLABORATION_MODE_SWARM_SUB_EXPLORER: &str =
    include_str!("../../templates/collaboration_mode/swarm_sub_explorer.md");
const COLLABORATION_MODE_SWARM_SUB_EXPLORER_COMPLEX: &str =
    include_str!("../../templates/collaboration_mode/swarm_sub_explorer_complex.md");
const COLLABORATION_MODE_SWARM_SUB_VERIFIER: &str =
    include_str!("../../templates/collaboration_mode/swarm_sub_verifier.md");
const COLLABORATION_MODE_SWARM_SUB_VERIFIER_COMPLEX: &str =
    include_str!("../../templates/collaboration_mode/swarm_sub_verifier_complex.md");

fn env_truthy(key: &str) -> bool {
    let Ok(raw) = env::var(key) else {
        return false;
    };
    !matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "" | "0" | "false" | "no" | "off"
    )
}

pub fn complex_swarm_enabled_from_env() -> bool {
    env_truthy(WECODE_SWARM_COMPLEX_ENV_VAR) || env_truthy(CODEX_SWARM_COMPLEX_ENV_VAR)
}

fn subagent_prompt_for_type(
    root_complex: bool,
    agent_type: Option<SpawnedAgentType>,
) -> &'static str {
    match (root_complex, agent_type) {
        (true, Some(SpawnedAgentType::Worker)) => COLLABORATION_MODE_SWARM_SUB_WORKER_COMPLEX,
        (false, Some(SpawnedAgentType::Worker)) => COLLABORATION_MODE_SWARM_SUB_WORKER,
        (true, Some(SpawnedAgentType::Explorer)) => COLLABORATION_MODE_SWARM_SUB_EXPLORER_COMPLEX,
        (false, Some(SpawnedAgentType::Explorer)) => COLLABORATION_MODE_SWARM_SUB_EXPLORER,
        (true, Some(SpawnedAgentType::Verifier)) => COLLABORATION_MODE_SWARM_SUB_VERIFIER_COMPLEX,
        (false, Some(SpawnedAgentType::Verifier)) => COLLABORATION_MODE_SWARM_SUB_VERIFIER,
        (true, _) => COLLABORATION_MODE_SWARM_SUB_COMPLEX,
        (false, _) => COLLABORATION_MODE_SWARM_SUB,
    }
}

pub(crate) fn swarm_developer_instructions_for_session_source(
    session_source: &SessionSource,
    root_complex: bool,
) -> &'static str {
    match session_source {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn { agent_type, .. }) => {
            subagent_prompt_for_type(root_complex, *agent_type)
        }
        SessionSource::SubAgent(_) => subagent_prompt_for_type(root_complex, None),
        _ if root_complex => COLLABORATION_MODE_SWARM_MAIN_COMPLEX,
        _ => COLLABORATION_MODE_SWARM_MAIN,
    }
}

pub(crate) fn default_root_swarm_is_complex() -> bool {
    complex_swarm_enabled_from_env()
}

pub(crate) fn complex_root_swarm_developer_instructions() -> &'static str {
    COLLABORATION_MODE_SWARM_MAIN_COMPLEX
}

pub(crate) fn standard_root_swarm_developer_instructions() -> &'static str {
    COLLABORATION_MODE_SWARM_MAIN
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::protocol::SubAgentSource;

    struct EnvVarGuard {
        key: &'static str,
        old: Option<String>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let old = env::var(key).ok();
            unsafe { env::set_var(key, value) };
            Self { key, old }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.old {
                Some(value) => unsafe { env::set_var(self.key, value) },
                None => unsafe { env::remove_var(self.key) },
            }
        }
    }

    #[test]
    fn default_root_prompt_uses_standard_swarm_when_env_disabled() {
        let _guard_a = EnvVarGuard::set(WECODE_SWARM_COMPLEX_ENV_VAR, "0");
        let _guard_b = EnvVarGuard::set(CODEX_SWARM_COMPLEX_ENV_VAR, "0");
        assert_eq!(default_root_swarm_is_complex(), false);
        assert_eq!(
            swarm_developer_instructions_for_session_source(&SessionSource::Exec, false),
            COLLABORATION_MODE_SWARM_MAIN
        );
    }

    #[test]
    fn default_root_prompt_uses_complex_swarm_when_env_enabled() {
        let _guard = EnvVarGuard::set(WECODE_SWARM_COMPLEX_ENV_VAR, "1");
        assert_eq!(default_root_swarm_is_complex(), true);
        assert_eq!(
            swarm_developer_instructions_for_session_source(&SessionSource::Exec, true),
            COLLABORATION_MODE_SWARM_MAIN_COMPLEX
        );
    }

    #[test]
    fn session_source_uses_sub_prompt_for_subagents() {
        assert_eq!(
            swarm_developer_instructions_for_session_source(
                &SessionSource::SubAgent(SubAgentSource::Review),
                false,
            ),
            COLLABORATION_MODE_SWARM_SUB
        );
        assert_eq!(
            swarm_developer_instructions_for_session_source(
                &SessionSource::SubAgent(SubAgentSource::Review),
                true,
            ),
            COLLABORATION_MODE_SWARM_SUB_COMPLEX
        );
    }
}
