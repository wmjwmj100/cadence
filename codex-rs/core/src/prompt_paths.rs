use std::path::Path;
use std::path::PathBuf;

pub(crate) const LOGICAL_WORKSPACE_ROOT: &str = "/workspace";

pub(crate) fn prompt_cwd(real_cwd: &Path) -> PathBuf {
    prompt_cwd_for_gateway(real_cwd, http_tool_gateway_enabled_from_env())
}

pub(crate) fn prompt_cwd_for_gateway(real_cwd: &Path, gateway_enabled: bool) -> PathBuf {
    if gateway_enabled {
        PathBuf::from(LOGICAL_WORKSPACE_ROOT)
    } else {
        real_cwd.to_path_buf()
    }
}

pub(crate) fn http_tool_gateway_enabled_from_env() -> bool {
    std::env::var("CODEX_TOOL_GATEWAY_URL")
        .ok()
        .is_some_and(|value| !value.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_cwd_uses_real_cwd_without_gateway() {
        assert_eq!(
            prompt_cwd_for_gateway(Path::new("/repo/project"), false),
            PathBuf::from("/repo/project")
        );
    }

    #[test]
    fn prompt_cwd_uses_workspace_with_gateway() {
        assert_eq!(
            prompt_cwd_for_gateway(Path::new("/repo/project"), true),
            PathBuf::from(LOGICAL_WORKSPACE_ROOT)
        );
    }
}
