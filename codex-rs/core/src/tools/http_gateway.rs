use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use crate::office::GatewayDecision;
use crate::office::ToolCapabilityPolicy;
use crate::tools::gateway::GatewayToolPayload;
use crate::tools::gateway::GatewayToolRequest;
use crate::tools::gateway::GatewayToolResponse;
use crate::tools::spec::ApplyPatchToolArgs;
use async_trait::async_trait;
use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::header::AUTHORIZATION;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::routing::post;
use codex_apply_patch::ApplyPatchAction;
use codex_apply_patch::ApplyPatchFileChange;
use codex_apply_patch::Hunk;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::SandboxPermissions;
use codex_protocol::models::ShellCommandToolCallParams;
use codex_protocol::models::ShellToolCallParams;
use serde::Deserialize;
use serde::Serialize;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::time::Instant;
use tokio::time::timeout;

const DEFAULT_DOCKER_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_DOCKER_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const DOCKER_TIMEOUT_EXIT_CODE: i32 = 124;
const CONTAINER_WORKSPACE_ROOT: &str = "/workspace";
const READ_FILE_SCRIPT: &str = r#"
import sys

path = sys.argv[1]
offset = int(sys.argv[2])
limit = int(sys.argv[3])
seen = 0
returned = 0
last_returned = None

with open(path, "rb") as handle:
    for raw in handle:
        seen += 1
        if seen < offset:
            continue
        if returned >= limit:
            continue
        line = raw.rstrip(b"\r\n").decode("utf-8", "replace")
        if len(line) > 500:
            line = line[:500]
        print(f"L{seen}: {line}")
        returned += 1
        last_returned = seen

if seen < offset:
    print("offset exceeds file length", file=sys.stderr)
    sys.exit(2)
if last_returned is not None and last_returned < seen:
    print(f"Note: file continues past line {last_returned}; file has {seen} total lines. Call read_file with offset={last_returned + 1} limit={limit} to read more.")
"#;
const LIST_DIR_SCRIPT: &str = r#"
import os
import sys

root = sys.argv[1]
offset = int(sys.argv[2])
limit = int(sys.argv[3])
max_depth = int(sys.argv[4])
entries = []

for current, dirs, files in os.walk(root):
    rel = os.path.relpath(current, root)
    depth = 0 if rel == "." else rel.count(os.sep) + 1
    if depth >= max_depth:
        dirs[:] = []
    names = [(name, True) for name in dirs] + [(name, False) for name in files]
    for name, is_dir in names:
        child = os.path.join(current, name)
        child_rel = os.path.relpath(child, root)
        child_depth = 0 if os.path.dirname(child_rel) == "" else os.path.dirname(child_rel).count(os.sep) + 1
        display = ("  " * child_depth) + name + ("/" if is_dir else "")
        entries.append((child_rel.replace(os.sep, "/"), display))

entries.sort(key=lambda item: item[0])
if offset < 1 or limit < 1 or max_depth < 1:
    print("offset, limit, and depth must be greater than zero", file=sys.stderr)
    sys.exit(2)
if offset - 1 >= len(entries):
    print("offset exceeds directory entry count", file=sys.stderr)
    sys.exit(2)
print(f"Absolute path: {root}")
selected = entries[offset - 1:offset - 1 + limit]
for _, display in selected:
    print(display[:500])
if offset - 1 + limit < len(entries):
    print(f"More than {len(selected)} entries found")
"#;
const GREP_FILES_SCRIPT: &str = r#"
import fnmatch
import os
import re
import sys

pattern = sys.argv[1]
include = sys.argv[2] or None
root = sys.argv[3]
limit = int(sys.argv[4])
regex = re.compile(pattern)
found = []

for current, _, files in os.walk(root):
    for name in files:
        if include and not fnmatch.fnmatch(name, include):
            continue
        path = os.path.join(current, name)
        try:
            with open(path, "r", encoding="utf-8", errors="ignore") as handle:
                text = handle.read()
        except OSError:
            continue
        if regex.search(text):
            found.append(path)
            if len(found) >= limit:
                break
    if len(found) >= limit:
        break

for path in found:
    print(path)
"#;

#[async_trait]
pub trait GatewayBackend: Send + Sync + 'static {
    async fn dispatch(
        &self,
        request: GatewayToolRequest,
    ) -> Result<GatewayToolResponse, GatewayBackendError>;
}

#[derive(Debug, thiserror::Error)]
pub enum GatewayBackendError {
    #[error("tool `{0}` is not supported by this middleware gateway")]
    UnsupportedTool(String),
    #[error("{0}")]
    Rejected(String),
    #[error("{0}")]
    Internal(String),
}

#[derive(Clone)]
pub struct DockerGatewayBackend {
    config: DockerGatewayConfig,
    runner: Arc<dyn DockerCommandRunner>,
}

#[derive(Clone, Debug)]
pub struct DockerGatewayConfig {
    pub image: String,
    pub workspace_root: PathBuf,
    pub docker_binary: String,
    pub uid_gid: Option<String>,
    pub memory_limit: String,
    pub cpu_limit: String,
    pub pids_limit: u32,
    pub default_timeout: Duration,
    pub max_timeout: Duration,
    pub default_company_id: String,
    pub container_name_prefix: String,
}

impl DockerGatewayConfig {
    pub fn new(image: impl Into<String>, workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            image: image.into(),
            workspace_root: workspace_root.into(),
            docker_binary: "docker".to_string(),
            uid_gid: Some("1000:1000".to_string()),
            memory_limit: "512m".to_string(),
            cpu_limit: "1.0".to_string(),
            pids_limit: 256,
            default_timeout: DEFAULT_DOCKER_TIMEOUT,
            max_timeout: MAX_DOCKER_TIMEOUT,
            default_company_id: "default".to_string(),
            container_name_prefix: "codex-company".to_string(),
        }
    }
}

impl DockerGatewayBackend {
    pub fn new(image: impl Into<String>, workspace_root: impl Into<PathBuf>) -> Self {
        Self::with_config(DockerGatewayConfig::new(image, workspace_root))
    }

    pub fn with_config(config: DockerGatewayConfig) -> Self {
        Self {
            config,
            runner: Arc::new(TokioDockerCommandRunner),
        }
    }

    pub fn with_docker_binary(mut self, docker_binary: impl Into<String>) -> Self {
        self.config.docker_binary = docker_binary.into();
        self
    }

    pub fn with_uid_gid(mut self, uid_gid: impl Into<Option<String>>) -> Self {
        self.config.uid_gid = uid_gid.into();
        self
    }

    pub fn with_limits(
        mut self,
        memory_limit: impl Into<String>,
        cpu_limit: impl Into<String>,
        pids_limit: u32,
    ) -> Self {
        self.config.memory_limit = memory_limit.into();
        self.config.cpu_limit = cpu_limit.into();
        self.config.pids_limit = pids_limit;
        self
    }

    pub fn with_default_timeout(mut self, timeout: Duration) -> Self {
        self.config.default_timeout = timeout;
        self
    }

    pub fn with_max_timeout(mut self, timeout: Duration) -> Self {
        self.config.max_timeout = timeout;
        self
    }

    pub fn with_default_company_id(mut self, company_id: impl Into<String>) -> Self {
        self.config.default_company_id = company_id.into();
        self
    }

    pub fn with_container_name_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.config.container_name_prefix = prefix.into();
        self
    }

    #[cfg(test)]
    fn with_runner<R>(mut self, runner: Arc<R>) -> Self
    where
        R: DockerCommandRunner,
    {
        self.runner = runner;
        self
    }
}

#[async_trait]
impl GatewayBackend for DockerGatewayBackend {
    async fn dispatch(
        &self,
        request: GatewayToolRequest,
    ) -> Result<GatewayToolResponse, GatewayBackendError> {
        let outputs_custom = matches!(request.payload, GatewayToolPayload::Custom { .. });
        let workspace_root = self.workspace_root()?;
        let scope = self.scope_for_request(&request, &workspace_root)?;
        self.prepare_workspace_partitions(&scope)?;
        let plan = self.plan_tool_request(&request, &workspace_root, &scope)?;
        self.ensure_company_container(&scope, &workspace_root)
            .await?;
        let args = self.docker_exec_args(&scope.container_name, &plan);
        let command_request = DockerCommandRequest {
            executable: self.config.docker_binary.clone(),
            args,
            timeout: plan.timeout,
            container_name: scope.container_name.clone(),
        };

        let output =
            self.runner.run(command_request).await.map_err(|err| {
                GatewayBackendError::Internal(format!("docker exec failed: {err}"))
            })?;

        let response = DockerExecutionResponse {
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            exit_code: output.exit_code,
        };
        let body = serde_json::to_string(&response).map_err(|err| {
            GatewayBackendError::Internal(format!("failed to serialize docker result: {err}"))
        })?;

        if outputs_custom {
            Ok(GatewayToolResponse::Custom {
                output: body,
                call_id: None,
            })
        } else {
            Ok(GatewayToolResponse::Function {
                output: FunctionCallOutputPayload {
                    body: FunctionCallOutputBody::Text(body),
                    success: Some(output.exit_code == 0),
                },
                call_id: None,
            })
        }
    }
}

impl DockerGatewayBackend {
    fn workspace_root(&self) -> Result<PathBuf, GatewayBackendError> {
        std::fs::create_dir_all(&self.config.workspace_root).map_err(|err| {
            GatewayBackendError::Rejected(format!(
                "docker workspace_root `{}` is not accessible: {err}",
                self.config.workspace_root.display()
            ))
        })?;
        std::fs::canonicalize(&self.config.workspace_root).map_err(|err| {
            GatewayBackendError::Rejected(format!(
                "docker workspace_root `{}` is not accessible: {err}",
                self.config.workspace_root.display()
            ))
        })
    }

    fn scope_for_request(
        &self,
        request: &GatewayToolRequest,
        workspace_root: &Path,
    ) -> Result<DockerRequestScope, GatewayBackendError> {
        let company_id = sanitize_scope_component(
            request
                .company_id
                .as_deref()
                .unwrap_or(&self.config.default_company_id),
            "company_id",
        )?;
        let project_id = request
            .project_id
            .as_deref()
            .map(|value| sanitize_scope_component(value, "project_id"))
            .transpose()?;
        let agent_id = sanitize_scope_component(
            request.agent_id.as_deref().unwrap_or("default-agent"),
            "agent_id",
        )?;
        let container_name =
            company_container_name(&self.config.container_name_prefix, &company_id)?;
        let public_host_root = workspace_root.join("public");
        let agent_private_host_root = workspace_root
            .join("agents")
            .join(&agent_id)
            .join("private");
        let project_host_root = project_id
            .as_ref()
            .map(|project_id| workspace_root.join("projects").join(project_id));
        Ok(DockerRequestScope {
            container_name,
            public_host_root,
            agent_private_host_root,
            project_host_root,
        })
    }

    fn prepare_workspace_partitions(
        &self,
        scope: &DockerRequestScope,
    ) -> Result<(), GatewayBackendError> {
        for path in [&scope.public_host_root, &scope.agent_private_host_root] {
            std::fs::create_dir_all(path).map_err(|err| {
                GatewayBackendError::Rejected(format!(
                    "failed to create docker workspace partition `{}`: {err}",
                    path.display()
                ))
            })?;
        }
        if let Some(project_host_root) = &scope.project_host_root {
            std::fs::create_dir_all(project_host_root).map_err(|err| {
                GatewayBackendError::Rejected(format!(
                    "failed to create docker project workspace `{}`: {err}",
                    project_host_root.display()
                ))
            })?;
        }
        Ok(())
    }

    fn request_cwd(
        &self,
        request: &GatewayToolRequest,
        workspace_root: &Path,
        scope: &DockerRequestScope,
    ) -> Result<PathBuf, GatewayBackendError> {
        let logical_workspace_root = Path::new(CONTAINER_WORKSPACE_ROOT);
        let requested = Path::new(&request.cwd);
        if let Some(project_host_root) = &scope.project_host_root {
            if requested == logical_workspace_root {
                return canonical_existing_path_inside_workspace(
                    project_host_root,
                    workspace_root,
                    "cwd",
                );
            }
            if let Ok(relative) = requested.strip_prefix(workspace_root)
                && relative.as_os_str().is_empty()
            {
                return canonical_existing_path_inside_workspace(
                    project_host_root,
                    workspace_root,
                    "cwd",
                );
            }
        }
        canonical_path_inside_workspace(&request.cwd, workspace_root, "cwd")
    }

    fn plan_tool_request(
        &self,
        request: &GatewayToolRequest,
        workspace_root: &Path,
        scope: &DockerRequestScope,
    ) -> Result<DockerToolPlan, GatewayBackendError> {
        let cwd = self.request_cwd(request, workspace_root, scope)?;

        match request.tool_name.as_str() {
            "shell" | "container.exec" => {
                let GatewayToolPayload::Function { arguments } = &request.payload else {
                    return Err(GatewayBackendError::Rejected(
                        "shell requires a function payload".to_string(),
                    ));
                };
                let params: ShellToolCallParams = parse_gateway_arguments(arguments)?;
                ensure_sandbox_permissions(params.sandbox_permissions)?;
                let command = validate_command(params.command)?;
                let workdir = resolve_workdir(&cwd, params.workdir.as_deref(), &workspace_root)?;
                Ok(DockerToolPlan::new(
                    command,
                    container_path(&workdir, &workspace_root)?,
                    true,
                    self.timeout_from_ms(params.timeout_ms)?,
                ))
            }
            "local_shell" => {
                let GatewayToolPayload::LocalShell {
                    command,
                    workdir,
                    timeout_ms,
                    sandbox_permissions,
                    ..
                } = &request.payload
                else {
                    return Err(GatewayBackendError::Rejected(
                        "local_shell requires a local_shell payload".to_string(),
                    ));
                };
                ensure_gateway_sandbox_permissions(sandbox_permissions.as_deref())?;
                let command = validate_command(command.clone())?;
                let workdir = resolve_workdir(&cwd, workdir.as_deref(), &workspace_root)?;
                Ok(DockerToolPlan::new(
                    command,
                    container_path(&workdir, &workspace_root)?,
                    true,
                    self.timeout_from_ms(*timeout_ms)?,
                ))
            }
            "shell_command" => {
                let GatewayToolPayload::Function { arguments } = &request.payload else {
                    return Err(GatewayBackendError::Rejected(
                        "shell_command requires a function payload".to_string(),
                    ));
                };
                let params: ShellCommandToolCallParams = parse_gateway_arguments(arguments)?;
                ensure_sandbox_permissions(params.sandbox_permissions)?;
                let command = params.command.trim();
                if command.is_empty() {
                    return Err(GatewayBackendError::Rejected(
                        "shell_command command must not be empty".to_string(),
                    ));
                }
                reject_nul(command, "shell_command command")?;
                let workdir = resolve_workdir(&cwd, params.workdir.as_deref(), &workspace_root)?;
                Ok(DockerToolPlan::new(
                    vec![
                        "/bin/sh".to_string(),
                        "-lc".to_string(),
                        command.to_string(),
                    ],
                    container_path(&workdir, &workspace_root)?,
                    true,
                    self.timeout_from_ms(params.timeout_ms)?,
                ))
            }
            "exec_command" => {
                let GatewayToolPayload::Function { arguments } = &request.payload else {
                    return Err(GatewayBackendError::Rejected(
                        "exec_command requires a function payload".to_string(),
                    ));
                };
                let params: DockerExecCommandArgs = parse_gateway_arguments(arguments)?;
                ensure_sandbox_permissions(Some(params.sandbox_permissions))?;
                let command = params.cmd.trim();
                if command.is_empty() {
                    return Err(GatewayBackendError::Rejected(
                        "exec_command cmd must not be empty".to_string(),
                    ));
                }
                reject_nul(command, "exec_command cmd")?;
                let shell = params
                    .shell
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or("/bin/sh");
                reject_nul(shell, "exec_command shell")?;
                let workdir = resolve_workdir(&cwd, params.workdir.as_deref(), &workspace_root)?;
                Ok(DockerToolPlan::new(
                    vec![shell.to_string(), "-lc".to_string(), command.to_string()],
                    container_path(&workdir, &workspace_root)?,
                    true,
                    self.timeout_from_ms(params.timeout_ms)?,
                ))
            }
            "apply_patch" => {
                let patch_input = extract_apply_patch_input(&request.payload)?;
                reject_nul(&patch_input, "apply_patch input")?;
                let patch_input = rewrite_apply_patch_paths_to_host(&patch_input, &workspace_root)?;
                preflight_apply_patch_paths(&patch_input, &cwd, &workspace_root)?;

                let command = vec!["apply_patch".to_string(), patch_input];
                let action =
                    match codex_apply_patch::maybe_parse_apply_patch_verified(&command, &cwd) {
                        codex_apply_patch::MaybeApplyPatchVerified::Body(action) => action,
                        codex_apply_patch::MaybeApplyPatchVerified::CorrectnessError(
                            parse_error,
                        ) => {
                            return Err(GatewayBackendError::Rejected(format!(
                                "apply_patch verification failed: {parse_error}"
                            )));
                        }
                        codex_apply_patch::MaybeApplyPatchVerified::ShellParseError(error) => {
                            tracing::trace!("Failed to parse apply_patch input, {error:?}");
                            return Err(GatewayBackendError::Rejected(
                                "apply_patch handler received invalid patch input".to_string(),
                            ));
                        }
                        codex_apply_patch::MaybeApplyPatchVerified::NotApplyPatch => {
                            return Err(GatewayBackendError::Rejected(
                                "apply_patch handler received non-apply_patch input".to_string(),
                            ));
                        }
                    };

                let action_cwd = validate_apply_patch_action(&action, &workspace_root)?;
                let patch = rewrite_apply_patch_absolute_paths(&action.patch, &workspace_root)?;
                Ok(DockerToolPlan::new(
                    vec!["apply_patch".to_string(), patch],
                    container_path(&action_cwd, &workspace_root)?,
                    true,
                    self.config.default_timeout,
                ))
            }
            "read_file" => {
                let GatewayToolPayload::Function { arguments } = &request.payload else {
                    return Err(GatewayBackendError::Rejected(
                        "read_file requires a function payload".to_string(),
                    ));
                };
                let params: DockerReadFileArgs = parse_gateway_arguments(arguments)?;
                let file = canonical_path_inside_workspace(
                    &resolve_input_path(&cwd, &params.file_path, &workspace_root)?,
                    &workspace_root,
                    "file_path",
                )?;
                let offset = params.offset.unwrap_or(1);
                let limit = params.limit.unwrap_or(2000);
                ensure_positive(offset, "offset")?;
                ensure_positive(limit, "limit")?;
                Ok(DockerToolPlan::new(
                    vec![
                        "python3".to_string(),
                        "-c".to_string(),
                        READ_FILE_SCRIPT.to_string(),
                        container_path(&file, &workspace_root)?,
                        offset.to_string(),
                        limit.to_string(),
                    ],
                    CONTAINER_WORKSPACE_ROOT.to_string(),
                    false,
                    self.config.default_timeout,
                ))
            }
            "list_dir" => {
                let GatewayToolPayload::Function { arguments } = &request.payload else {
                    return Err(GatewayBackendError::Rejected(
                        "list_dir requires a function payload".to_string(),
                    ));
                };
                let params: DockerListDirArgs = parse_gateway_arguments(arguments)?;
                let dir = canonical_path_inside_workspace(
                    &resolve_input_path(&cwd, &params.dir_path, &workspace_root)?,
                    &workspace_root,
                    "dir_path",
                )?;
                let offset = params.offset.unwrap_or(1);
                let limit = params.limit.unwrap_or(25);
                let depth = params.depth.unwrap_or(2);
                ensure_positive(offset, "offset")?;
                ensure_positive(limit, "limit")?;
                ensure_positive(depth, "depth")?;
                Ok(DockerToolPlan::new(
                    vec![
                        "python3".to_string(),
                        "-c".to_string(),
                        LIST_DIR_SCRIPT.to_string(),
                        container_path(&dir, &workspace_root)?,
                        offset.to_string(),
                        limit.to_string(),
                        depth.to_string(),
                    ],
                    CONTAINER_WORKSPACE_ROOT.to_string(),
                    false,
                    self.config.default_timeout,
                ))
            }
            "grep_files" => {
                let GatewayToolPayload::Function { arguments } = &request.payload else {
                    return Err(GatewayBackendError::Rejected(
                        "grep_files requires a function payload".to_string(),
                    ));
                };
                let params: DockerGrepFilesArgs = parse_gateway_arguments(arguments)?;
                let pattern = params.pattern.trim();
                if pattern.is_empty() {
                    return Err(GatewayBackendError::Rejected(
                        "pattern must not be empty".to_string(),
                    ));
                }
                reject_nul(pattern, "pattern")?;
                let limit = params.limit.unwrap_or(100).min(2000);
                ensure_positive(limit, "limit")?;
                let search_path = params.path.unwrap_or_else(|| ".".to_string());
                let path = canonical_path_inside_workspace(
                    &resolve_input_path(&cwd, &search_path, &workspace_root)?,
                    &workspace_root,
                    "path",
                )?;
                let include = params.include.unwrap_or_default();
                reject_nul(&include, "include")?;
                Ok(DockerToolPlan::new(
                    vec![
                        "python3".to_string(),
                        "-c".to_string(),
                        GREP_FILES_SCRIPT.to_string(),
                        pattern.to_string(),
                        include,
                        container_path(&path, &workspace_root)?,
                        limit.to_string(),
                    ],
                    CONTAINER_WORKSPACE_ROOT.to_string(),
                    false,
                    self.config.default_timeout,
                ))
            }
            other => Err(GatewayBackendError::UnsupportedTool(other.to_string())),
        }
    }

    async fn ensure_company_container(
        &self,
        scope: &DockerRequestScope,
        workspace_root: &Path,
    ) -> Result<(), GatewayBackendError> {
        let inspect = DockerCommandRequest {
            executable: self.config.docker_binary.clone(),
            args: vec![
                "inspect".to_string(),
                "-f".to_string(),
                "{{.State.Running}}".to_string(),
                scope.container_name.clone(),
            ],
            timeout: self.config.default_timeout.min(self.config.max_timeout),
            container_name: scope.container_name.clone(),
        };
        let inspect_output = self.runner.run(inspect).await.map_err(|err| {
            GatewayBackendError::Internal(format!("docker inspect failed: {err}"))
        })?;
        if inspect_output.exit_code == 0 {
            let running = String::from_utf8_lossy(&inspect_output.stdout)
                .trim()
                .eq_ignore_ascii_case("true");
            if running {
                return Ok(());
            }
            return self.start_company_container(scope).await;
        }
        self.create_company_container(scope, workspace_root).await
    }

    async fn start_company_container(
        &self,
        scope: &DockerRequestScope,
    ) -> Result<(), GatewayBackendError> {
        let request = DockerCommandRequest {
            executable: self.config.docker_binary.clone(),
            args: vec!["start".to_string(), scope.container_name.clone()],
            timeout: self.config.default_timeout.min(self.config.max_timeout),
            container_name: scope.container_name.clone(),
        };
        let output =
            self.runner.run(request).await.map_err(|err| {
                GatewayBackendError::Internal(format!("docker start failed: {err}"))
            })?;
        if output.exit_code == 0 {
            Ok(())
        } else {
            Err(GatewayBackendError::Internal(format!(
                "docker start `{}` failed with exit code {}: {}",
                scope.container_name,
                output.exit_code,
                String::from_utf8_lossy(&output.stderr)
            )))
        }
    }

    async fn create_company_container(
        &self,
        scope: &DockerRequestScope,
        workspace_root: &Path,
    ) -> Result<(), GatewayBackendError> {
        let args = self.docker_create_args(&scope.container_name, workspace_root);
        let request = DockerCommandRequest {
            executable: self.config.docker_binary.clone(),
            args,
            timeout: self.config.default_timeout.min(self.config.max_timeout),
            container_name: scope.container_name.clone(),
        };
        let output =
            self.runner.run(request).await.map_err(|err| {
                GatewayBackendError::Internal(format!("docker run failed: {err}"))
            })?;
        if output.exit_code == 0 {
            Ok(())
        } else {
            Err(GatewayBackendError::Internal(format!(
                "docker run `{}` failed with exit code {}: {}",
                scope.container_name,
                output.exit_code,
                String::from_utf8_lossy(&output.stderr)
            )))
        }
    }

    fn docker_create_args(&self, container_name: &str, workspace_root: &Path) -> Vec<String> {
        let mount = format!("{}:{CONTAINER_WORKSPACE_ROOT}:rw", workspace_root.display());
        vec![
            "run".to_string(),
            "-d".to_string(),
            "--name".to_string(),
            container_name.to_string(),
            "--network".to_string(),
            "none".to_string(),
            "--read-only".to_string(),
            "--tmpfs".to_string(),
            "/tmp:rw,noexec,nosuid,nodev,size=64m".to_string(),
            "--cap-drop".to_string(),
            "ALL".to_string(),
            "--security-opt".to_string(),
            "no-new-privileges".to_string(),
            "--memory".to_string(),
            self.config.memory_limit.clone(),
            "--cpus".to_string(),
            self.config.cpu_limit.clone(),
            "--pids-limit".to_string(),
            self.config.pids_limit.to_string(),
            "-v".to_string(),
            mount,
            self.config.image.clone(),
            "sleep".to_string(),
            "infinity".to_string(),
        ]
    }

    fn docker_exec_args(&self, container_name: &str, plan: &DockerToolPlan) -> Vec<String> {
        let mut args = vec![
            "exec".to_string(),
            "-w".to_string(),
            plan.workdir_in_container.clone(),
        ];
        if let Some(uid_gid) = &self.config.uid_gid {
            args.push("-u".to_string());
            args.push(uid_gid.clone());
        }
        args.push(container_name.to_string());
        args.extend(plan.command.clone());
        args
    }

    fn timeout_from_ms(&self, timeout_ms: Option<u64>) -> Result<Duration, GatewayBackendError> {
        let Some(timeout_ms) = timeout_ms else {
            return Ok(self.config.default_timeout.min(self.config.max_timeout));
        };
        if timeout_ms == 0 {
            return Err(GatewayBackendError::Rejected(
                "timeout_ms must be greater than zero".to_string(),
            ));
        }
        Ok(Duration::from_millis(timeout_ms).min(self.config.max_timeout))
    }
}

#[derive(Debug, Clone)]
struct DockerRequestScope {
    container_name: String,
    public_host_root: PathBuf,
    agent_private_host_root: PathBuf,
    project_host_root: Option<PathBuf>,
}

#[derive(Debug)]
struct DockerToolPlan {
    command: Vec<String>,
    workdir_in_container: String,
    timeout: Duration,
}

impl DockerToolPlan {
    fn new(
        command: Vec<String>,
        workdir_in_container: String,
        _mount_writable: bool,
        timeout: Duration,
    ) -> Self {
        Self {
            command,
            workdir_in_container,
            timeout,
        }
    }
}

#[derive(Debug, Clone)]
struct DockerCommandRequest {
    executable: String,
    args: Vec<String>,
    timeout: Duration,
    container_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DockerCommandOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit_code: i32,
}

#[async_trait]
trait DockerCommandRunner: Send + Sync + 'static {
    async fn run(&self, request: DockerCommandRequest) -> std::io::Result<DockerCommandOutput>;
}

struct TokioDockerCommandRunner;

#[async_trait]
impl DockerCommandRunner for TokioDockerCommandRunner {
    async fn run(&self, request: DockerCommandRequest) -> std::io::Result<DockerCommandOutput> {
        let DockerCommandRequest {
            executable,
            args,
            timeout: run_timeout,
            container_name: _container_name,
        } = request;
        let mut child = Command::new(&executable)
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let mut stdout = child.stdout.take().expect("stdout was piped");
        let mut stderr = child.stderr.take().expect("stderr was piped");
        let stdout_task = tokio::spawn(async move { read_all(&mut stdout).await });
        let stderr_task = tokio::spawn(async move { read_all(&mut stderr).await });
        let started = Instant::now();
        let (did_timeout, exit_code) = match timeout(run_timeout, child.wait()).await {
            Ok(status) => {
                let status = status?;
                (false, status.code().unwrap_or(1))
            }
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                (true, DOCKER_TIMEOUT_EXIT_CODE)
            }
        };

        let stdout = stdout_task.await.unwrap_or_else(join_error_to_io)?;
        let mut stderr = stderr_task.await.unwrap_or_else(join_error_to_io)?;
        if did_timeout {
            stderr.extend_from_slice(
                format!(
                    "\ncommand timed out after {} milliseconds",
                    started.elapsed().as_millis()
                )
                .as_bytes(),
            );
        }

        Ok(DockerCommandOutput {
            stdout,
            stderr,
            exit_code,
        })
    }
}

async fn read_all<R>(reader: &mut R) -> std::io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut buffer = Vec::new();
    reader.read_to_end(&mut buffer).await?;
    Ok(buffer)
}

fn join_error_to_io(err: tokio::task::JoinError) -> std::io::Result<Vec<u8>> {
    Err(std::io::Error::other(format!(
        "docker output reader task failed: {err}"
    )))
}

#[derive(Debug, Serialize)]
struct DockerExecutionResponse {
    stdout: String,
    stderr: String,
    exit_code: i32,
}

#[derive(Debug, Deserialize)]
struct DockerExecCommandArgs {
    cmd: String,
    #[serde(default)]
    workdir: Option<String>,
    #[serde(default)]
    shell: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    sandbox_permissions: SandboxPermissions,
}

#[derive(Debug, Deserialize)]
struct DockerReadFileArgs {
    file_path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct DockerListDirArgs {
    dir_path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    depth: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct DockerGrepFilesArgs {
    pattern: String,
    #[serde(default)]
    include: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

fn parse_gateway_arguments<T>(arguments: &str) -> Result<T, GatewayBackendError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_str(arguments)
        .map_err(|err| GatewayBackendError::Rejected(format!("invalid tool arguments: {err}")))
}

fn ensure_sandbox_permissions(
    permissions: Option<SandboxPermissions>,
) -> Result<(), GatewayBackendError> {
    if permissions
        .unwrap_or_default()
        .requires_escalated_permissions()
    {
        return Err(GatewayBackendError::Rejected(
            "middleware gateway does not allow escalated sandbox permissions".to_string(),
        ));
    }
    Ok(())
}

fn ensure_gateway_sandbox_permissions(
    permissions: Option<&str>,
) -> Result<(), GatewayBackendError> {
    if permissions.is_some_and(|value| value.contains("RequireEscalated")) {
        return Err(GatewayBackendError::Rejected(
            "middleware gateway does not allow escalated sandbox permissions".to_string(),
        ));
    }
    Ok(())
}

fn validate_command(command: Vec<String>) -> Result<Vec<String>, GatewayBackendError> {
    if command.is_empty() {
        return Err(GatewayBackendError::Rejected(
            "command must not be empty".to_string(),
        ));
    }
    if command.iter().any(|arg| arg.is_empty()) {
        return Err(GatewayBackendError::Rejected(
            "command arguments must not be empty".to_string(),
        ));
    }
    for arg in &command {
        reject_nul(arg, "command argument")?;
    }
    Ok(command)
}

fn extract_apply_patch_input(payload: &GatewayToolPayload) -> Result<String, GatewayBackendError> {
    match payload {
        GatewayToolPayload::Function { arguments } => {
            let args: ApplyPatchToolArgs = parse_gateway_arguments(arguments)?;
            Ok(args.input)
        }
        GatewayToolPayload::Custom { input } => Ok(input.clone()),
        _ => Err(GatewayBackendError::Rejected(
            "apply_patch requires a function or custom payload".to_string(),
        )),
    }
}

fn preflight_apply_patch_paths(
    patch_input: &str,
    cwd: &Path,
    workspace_root: &Path,
) -> Result<(), GatewayBackendError> {
    let parsed = codex_apply_patch::parse_patch(patch_input).map_err(|err| {
        GatewayBackendError::Rejected(format!("apply_patch verification failed: {err}"))
    })?;
    for hunk in &parsed.hunks {
        match hunk {
            Hunk::AddFile { path, .. } | Hunk::DeleteFile { path } => {
                validate_patch_path(cwd, path, workspace_root, "apply_patch target")?;
            }
            Hunk::UpdateFile {
                path, move_path, ..
            } => {
                validate_patch_path(cwd, path, workspace_root, "apply_patch target")?;
                if let Some(move_path) = move_path {
                    validate_patch_path(cwd, move_path, workspace_root, "apply_patch move target")?;
                }
            }
        }
    }
    Ok(())
}

fn validate_apply_patch_action(
    action: &ApplyPatchAction,
    workspace_root: &Path,
) -> Result<PathBuf, GatewayBackendError> {
    let cwd = canonical_path_inside_workspace(
        &path_to_utf8(&action.cwd, "apply_patch cwd")?,
        workspace_root,
        "apply_patch cwd",
    )?;
    for (path, change) in action.changes() {
        validate_workspace_target_path(path, workspace_root, "apply_patch target")?;
        if let ApplyPatchFileChange::Update {
            move_path: Some(move_path),
            ..
        } = change
        {
            validate_workspace_target_path(move_path, workspace_root, "apply_patch move target")?;
        }
    }
    Ok(cwd)
}

fn validate_patch_path(
    cwd: &Path,
    patch_path: &Path,
    workspace_root: &Path,
    label: &str,
) -> Result<(), GatewayBackendError> {
    let resolved = if patch_path.is_absolute() {
        gateway_path_to_host_path(patch_path, workspace_root)
    } else {
        cwd.join(patch_path)
    };
    validate_workspace_target_path(&resolved, workspace_root, label)
}

fn validate_workspace_target_path(
    raw_path: &Path,
    workspace_root: &Path,
    label: &str,
) -> Result<(), GatewayBackendError> {
    if !raw_path.is_absolute() {
        return Err(GatewayBackendError::Rejected(format!(
            "{label} must resolve to an absolute path"
        )));
    }

    let normalized = normalize_absolute_path(raw_path);
    if !normalized.starts_with(workspace_root) {
        return Err(GatewayBackendError::Rejected(format!(
            "{label} must stay inside docker workspace_root"
        )));
    }

    let mut ancestor = normalized.as_path();
    loop {
        if ancestor.exists() {
            let canonical = std::fs::canonicalize(ancestor).map_err(|err| {
                GatewayBackendError::Rejected(format!(
                    "{label} `{}` is not accessible: {err}",
                    ancestor.display()
                ))
            })?;
            if !canonical.starts_with(workspace_root) {
                return Err(GatewayBackendError::Rejected(format!(
                    "{label} must stay inside docker workspace_root"
                )));
            }
            return Ok(());
        }
        ancestor = ancestor.parent().ok_or_else(|| {
            GatewayBackendError::Rejected(format!("{label} has no accessible parent"))
        })?;
    }
}

fn normalize_absolute_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}

fn rewrite_apply_patch_absolute_paths(
    patch: &str,
    workspace_root: &Path,
) -> Result<String, GatewayBackendError> {
    let mut rewritten = Vec::new();
    for line in patch.lines() {
        rewritten.push(rewrite_apply_patch_marker_line(line, workspace_root)?);
    }
    Ok(rewritten.join("\n"))
}

fn rewrite_apply_patch_marker_line(
    line: &str,
    workspace_root: &Path,
) -> Result<String, GatewayBackendError> {
    const MARKERS: [&str; 4] = [
        "*** Add File: ",
        "*** Delete File: ",
        "*** Update File: ",
        "*** Move to: ",
    ];

    for marker in MARKERS {
        if let Some(path) = line.strip_prefix(marker) {
            let path = Path::new(path);
            if path.is_absolute() {
                let container = container_path(&normalize_absolute_path(path), workspace_root)?;
                return Ok(format!("{marker}{container}"));
            }
            return Ok(line.to_string());
        }
    }
    Ok(line.to_string())
}

fn reject_nul(value: &str, label: &str) -> Result<(), GatewayBackendError> {
    if value.contains('\0') {
        return Err(GatewayBackendError::Rejected(format!(
            "{label} must not contain NUL bytes"
        )));
    }
    Ok(())
}

fn ensure_positive(value: usize, label: &str) -> Result<(), GatewayBackendError> {
    if value == 0 {
        return Err(GatewayBackendError::Rejected(format!(
            "{label} must be greater than zero"
        )));
    }
    Ok(())
}

fn canonical_path_inside_workspace(
    raw_path: &str,
    workspace_root: &Path,
    label: &str,
) -> Result<PathBuf, GatewayBackendError> {
    reject_nul(raw_path, label)?;
    let path = PathBuf::from(raw_path);
    if !path.is_absolute() {
        return Err(GatewayBackendError::Rejected(format!(
            "{label} must be an absolute path"
        )));
    }
    let path = gateway_path_to_host_path(&path, workspace_root);
    let canonical = std::fs::canonicalize(&path).map_err(|err| {
        GatewayBackendError::Rejected(format!(
            "{label} `{}` is not accessible: {err}",
            path.display()
        ))
    })?;
    if !canonical.starts_with(workspace_root) {
        return Err(GatewayBackendError::Rejected(format!(
            "{label} must stay inside docker workspace_root"
        )));
    }
    Ok(canonical)
}

fn canonical_existing_path_inside_workspace(
    path: &Path,
    workspace_root: &Path,
    label: &str,
) -> Result<PathBuf, GatewayBackendError> {
    let canonical = std::fs::canonicalize(path).map_err(|err| {
        GatewayBackendError::Rejected(format!(
            "{label} `{}` is not accessible: {err}",
            path.display()
        ))
    })?;
    if !canonical.starts_with(workspace_root) {
        return Err(GatewayBackendError::Rejected(format!(
            "{label} must stay inside docker workspace_root"
        )));
    }
    Ok(canonical)
}

fn path_to_utf8(path: &Path, label: &str) -> Result<String, GatewayBackendError> {
    path.to_str()
        .map(str::to_string)
        .ok_or_else(|| GatewayBackendError::Rejected(format!("{label} must be valid UTF-8")))
}

fn resolve_input_path(
    cwd: &Path,
    value: &str,
    workspace_root: &Path,
) -> Result<String, GatewayBackendError> {
    reject_nul(value, "path")?;
    let path = PathBuf::from(value);
    let resolved = if path.is_absolute() {
        gateway_path_to_host_path(&path, workspace_root)
    } else {
        cwd.join(path)
    };
    resolved
        .to_str()
        .map(str::to_string)
        .ok_or_else(|| GatewayBackendError::Rejected("path must be valid UTF-8".to_string()))
}

fn resolve_workdir(
    cwd: &Path,
    workdir: Option<&str>,
    workspace_root: &Path,
) -> Result<PathBuf, GatewayBackendError> {
    match workdir.map(str::trim).filter(|value| !value.is_empty()) {
        Some(workdir) => canonical_path_inside_workspace(
            &resolve_input_path(cwd, workdir, workspace_root)?,
            workspace_root,
            "workdir",
        ),
        None => Ok(cwd.to_path_buf()),
    }
}

fn container_path(path: &Path, workspace_root: &Path) -> Result<String, GatewayBackendError> {
    let relative = path.strip_prefix(workspace_root).map_err(|_| {
        GatewayBackendError::Rejected("path must stay inside docker workspace_root".to_string())
    })?;
    if relative.as_os_str().is_empty() {
        return Ok(CONTAINER_WORKSPACE_ROOT.to_string());
    }
    let mut container_path = PathBuf::from(CONTAINER_WORKSPACE_ROOT);
    container_path.push(relative);
    Ok(container_path.to_string_lossy().replace('\\', "/"))
}

fn gateway_path_to_host_path(path: &Path, workspace_root: &Path) -> PathBuf {
    let logical_workspace_root = Path::new(CONTAINER_WORKSPACE_ROOT);
    match path.strip_prefix(logical_workspace_root) {
        Ok(relative) => workspace_root.join(relative),
        Err(_) => path.to_path_buf(),
    }
}

fn rewrite_apply_patch_paths_to_host(
    patch: &str,
    workspace_root: &Path,
) -> Result<String, GatewayBackendError> {
    let mut rewritten = Vec::new();
    for line in patch.lines() {
        rewritten.push(rewrite_apply_patch_marker_line_to_host(
            line,
            workspace_root,
        )?);
    }
    Ok(rewritten.join("\n"))
}

fn rewrite_apply_patch_marker_line_to_host(
    line: &str,
    workspace_root: &Path,
) -> Result<String, GatewayBackendError> {
    const MARKERS: [&str; 4] = [
        "*** Add File: ",
        "*** Delete File: ",
        "*** Update File: ",
        "*** Move to: ",
    ];

    for marker in MARKERS {
        if let Some(path) = line.strip_prefix(marker) {
            let path = Path::new(path);
            if path.is_absolute() {
                let host = gateway_path_to_host_path(path, workspace_root);
                return Ok(format!(
                    "{marker}{}",
                    host.to_string_lossy().replace('\\', "/")
                ));
            }
            return Ok(line.to_string());
        }
    }
    Ok(line.to_string())
}

fn company_container_name(prefix: &str, company_id: &str) -> Result<String, GatewayBackendError> {
    let prefix = sanitize_docker_name_component(prefix, "container_name_prefix")?;
    let company = sanitize_docker_name_component(company_id, "company_id")?;
    Ok(format!("{prefix}-{company}"))
}

fn sanitize_scope_component(value: &str, label: &str) -> Result<String, GatewayBackendError> {
    let sanitized = sanitize_component(value, 64);
    if sanitized.is_empty() {
        return Err(GatewayBackendError::Rejected(format!(
            "{label} must contain at least one ASCII letter or digit"
        )));
    }
    Ok(sanitized)
}

fn sanitize_docker_name_component(value: &str, label: &str) -> Result<String, GatewayBackendError> {
    let sanitized = sanitize_component(value, 48);
    if sanitized.is_empty() {
        return Err(GatewayBackendError::Rejected(format!(
            "{label} must contain at least one ASCII letter or digit"
        )));
    }
    Ok(sanitized)
}

fn sanitize_component(value: &str, max_len: usize) -> String {
    let mut output = String::new();
    let mut last_was_separator = false;
    for ch in value.trim().chars() {
        let mapped = if ch.is_ascii_alphanumeric() {
            Some(ch.to_ascii_lowercase())
        } else if matches!(ch, '-' | '_' | '.') {
            Some(ch)
        } else if ch.is_whitespace() || matches!(ch, '/' | '\\' | ':' | '@' | '#') {
            Some('-')
        } else {
            None
        };
        let Some(mapped) = mapped else {
            continue;
        };
        let is_separator = matches!(mapped, '-' | '_' | '.');
        if is_separator && (output.is_empty() || last_was_separator) {
            continue;
        }
        output.push(mapped);
        last_was_separator = is_separator;
        if output.len() >= max_len {
            break;
        }
    }
    while output.ends_with(['-', '_', '.']) {
        output.pop();
    }
    output
}

#[derive(Clone)]
pub struct GatewayHttpServer {
    state: Arc<GatewayHttpState>,
}

impl GatewayHttpServer {
    pub fn new(backend: impl GatewayBackend) -> Self {
        let backend: Arc<dyn GatewayBackend> = Arc::new(backend);
        Self {
            state: Arc::new(GatewayHttpState {
                bearer_token: None,
                capability_policy: ToolCapabilityPolicy::allow_all(),
                backend,
            }),
        }
    }

    pub fn with_bearer_token(mut self, bearer_token: impl Into<String>) -> Self {
        Arc::make_mut(&mut self.state).bearer_token = Some(bearer_token.into());
        self
    }

    pub fn with_capability_policy(mut self, capability_policy: ToolCapabilityPolicy) -> Self {
        Arc::make_mut(&mut self.state).capability_policy = capability_policy;
        self
    }

    pub fn into_router(self) -> Router {
        Router::new()
            .route("/healthz", get(healthz))
            .route("/tools/dispatch", post(dispatch_tool))
            .with_state(self.state)
    }

    pub async fn serve(self, addr: SocketAddr) -> std::io::Result<()> {
        let listener = TcpListener::bind(addr).await?;
        axum::serve(listener, self.into_router()).await
    }
}

struct GatewayHttpState {
    bearer_token: Option<String>,
    capability_policy: ToolCapabilityPolicy,
    backend: Arc<dyn GatewayBackend>,
}

impl Clone for GatewayHttpState {
    fn clone(&self) -> Self {
        Self {
            bearer_token: self.bearer_token.clone(),
            capability_policy: self.capability_policy.clone(),
            backend: Arc::clone(&self.backend),
        }
    }
}

#[derive(Serialize)]
struct GatewayErrorBody {
    error: String,
}

async fn healthz() -> &'static str {
    "ok"
}

async fn dispatch_tool(
    State(state): State<Arc<GatewayHttpState>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    if !authorized(&state, &headers) {
        return gateway_error(StatusCode::UNAUTHORIZED, "missing or invalid bearer token");
    }

    let request = match serde_json::from_slice::<GatewayToolRequest>(&body) {
        Ok(request) => request,
        Err(err) => {
            return gateway_error(
                StatusCode::BAD_REQUEST,
                format!("invalid request JSON: {err}"),
            );
        }
    };

    if let GatewayDecision::Denied { reason } =
        state.capability_policy.check(request.tool_name.as_str())
    {
        return gateway_error(StatusCode::FORBIDDEN, reason);
    }

    match state.backend.dispatch(request).await {
        Ok(response) => (StatusCode::OK, Json(response)).into_response(),
        Err(GatewayBackendError::UnsupportedTool(tool_name)) => gateway_error(
            StatusCode::BAD_REQUEST,
            GatewayBackendError::UnsupportedTool(tool_name).to_string(),
        ),
        Err(GatewayBackendError::Rejected(message)) => {
            gateway_error(StatusCode::FORBIDDEN, message)
        }
        Err(GatewayBackendError::Internal(message)) => {
            gateway_error(StatusCode::INTERNAL_SERVER_ERROR, message)
        }
    }
}

fn authorized(state: &GatewayHttpState, headers: &HeaderMap) -> bool {
    let Some(expected_token) = state.bearer_token.as_ref() else {
        return true;
    };
    headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.strip_prefix("Bearer ") == Some(expected_token.as_str()))
}

fn gateway_error(status: StatusCode, error: impl Into<String>) -> axum::response::Response {
    (
        status,
        Json(GatewayErrorBody {
            error: error.into(),
        }),
    )
        .into_response()
}

#[derive(Clone, Default)]
pub struct InMemoryGatewayBackend {
    responses: HashMap<String, GatewayToolResponse>,
}

impl InMemoryGatewayBackend {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_text_response(
        mut self,
        tool_name: impl Into<String>,
        output: impl Into<String>,
    ) -> Self {
        self.responses.insert(
            tool_name.into(),
            GatewayToolResponse::Function {
                output: FunctionCallOutputPayload {
                    body: FunctionCallOutputBody::Text(output.into()),
                    success: Some(true),
                },
                call_id: None,
            },
        );
        self
    }

    pub fn with_response(
        mut self,
        tool_name: impl Into<String>,
        response: GatewayToolResponse,
    ) -> Self {
        self.responses.insert(tool_name.into(), response);
        self
    }
}

#[async_trait]
impl GatewayBackend for InMemoryGatewayBackend {
    async fn dispatch(
        &self,
        request: GatewayToolRequest,
    ) -> Result<GatewayToolResponse, GatewayBackendError> {
        self.responses
            .get(&request.tool_name)
            .cloned()
            .ok_or(GatewayBackendError::UnsupportedTool(request.tool_name))
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::time::Duration;

    use async_trait::async_trait;
    use chrono::TimeZone;
    use codex_protocol::ThreadId;
    use codex_protocol::config_types::ModeKind;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::FunctionCallOutputBody;
    use codex_protocol::models::FunctionCallOutputPayload;
    use codex_protocol::models::ResponseItem;
    use codex_protocol::protocol::DebugTraceContextChannel;
    use codex_protocol::protocol::DebugTraceRole;
    use codex_protocol::protocol::SessionSource;
    use reqwest::Client;
    use serde_json::Value;
    use serde_json::json;
    use tempfile::tempdir;
    use tokio::task::JoinHandle;

    use super::*;
    use crate::agent::context::AgentAutomaticPromptSection;
    use crate::agent::context::AgentAutomaticPromptUpdateStatus;
    use crate::agent::context::AgentContextStore;
    use crate::agent::context::AgentMemoryCandidate;
    use crate::agent::context::AgentMemoryProvenance;
    use crate::debug_trace;
    use crate::debug_trace::DebugTraceContext;
    use crate::office::OfficeAgentExternalState;
    use crate::office::OfficeWebApp;
    use crate::office::OfficeWebConfig;
    use crate::office::PersistentPilotDirectory;
    use crate::office::ToolCapabilityPolicy;
    use crate::tools::gateway::GatewayToolPayload;

    #[derive(Default)]
    struct RecordingBackend {
        requests: Mutex<Vec<GatewayToolRequest>>,
        response: Mutex<Option<Result<GatewayToolResponse, GatewayBackendError>>>,
    }

    impl RecordingBackend {
        fn with_response(response: Result<GatewayToolResponse, GatewayBackendError>) -> Self {
            Self {
                requests: Mutex::new(Vec::new()),
                response: Mutex::new(Some(response)),
            }
        }

        fn request_count(&self) -> usize {
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len()
        }
    }

    #[async_trait]
    impl GatewayBackend for Arc<RecordingBackend> {
        async fn dispatch(
            &self,
            request: GatewayToolRequest,
        ) -> Result<GatewayToolResponse, GatewayBackendError> {
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(request);
            self.response
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
                .unwrap_or_else(|| {
                    Ok(GatewayToolResponse::Error {
                        message: "missing test response".to_string(),
                    })
                })
        }
    }

    async fn spawn_server(server: GatewayHttpServer) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr: SocketAddr = listener.local_addr().expect("local addr");
        let router = server.into_router();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        (format!("http://{addr}"), handle)
    }

    fn request_json(tool_name: &str) -> Value {
        json!({
            "session_id": "session-1",
            "turn_id": "turn-1",
            "cwd": "/tmp/work",
            "call_id": "call-1",
            "tool_name": tool_name,
            "payload": {
                "kind": "function",
                "arguments": "{}"
            }
        })
    }

    async fn spawn_office_web_app(app: OfficeWebApp) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind office listener");
        let addr: SocketAddr = listener.local_addr().expect("office local addr");
        let router = app.into_router();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        (format!("http://{addr}"), handle)
    }

    fn response_session_cookie(response: &reqwest::Response) -> String {
        response
            .headers()
            .get("set-cookie")
            .expect("set-cookie header")
            .to_str()
            .expect("set-cookie text")
            .split(';')
            .next()
            .expect("cookie pair")
            .to_string()
    }

    fn task14_memory_candidate(
        section: AgentAutomaticPromptSection,
        text: &str,
    ) -> AgentMemoryCandidate {
        AgentMemoryCandidate {
            section: Some(section),
            text: text.to_string(),
            stability: 0.95,
            reuse_value: 0.90,
            owner_relevance: 0.85,
            confidence: 0.88,
            provenance: AgentMemoryProvenance {
                source_type: "task14_acceptance_journal".to_string(),
                source_path: None,
                note: Some("task #14 integrated acceptance evidence".to_string()),
            },
        }
    }

    #[tokio::test]
    async fn healthz_is_public() {
        let (base_url, handle) = spawn_server(
            GatewayHttpServer::new(InMemoryGatewayBackend::new()).with_bearer_token("secret"),
        )
        .await;

        let response = Client::new()
            .get(format!("{base_url}/healthz"))
            .send()
            .await
            .expect("healthz response");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.text().await.expect("body"), "ok");
        handle.abort();
    }

    #[tokio::test]
    async fn dispatch_success_passes_request_to_backend() {
        let backend = Arc::new(RecordingBackend::with_response(Ok(
            GatewayToolResponse::Function {
                output: FunctionCallOutputPayload {
                    body: FunctionCallOutputBody::Text("middleware ok".to_string()),
                    success: Some(true),
                },
                call_id: None,
            },
        )));
        let (base_url, handle) =
            spawn_server(GatewayHttpServer::new(Arc::clone(&backend)).with_bearer_token("secret"))
                .await;

        let response = Client::new()
            .post(format!("{base_url}/tools/dispatch"))
            .bearer_auth("secret")
            .json(&request_json("shell"))
            .send()
            .await
            .expect("dispatch response");

        assert_eq!(response.status(), StatusCode::OK);
        let body: GatewayToolResponse = response.json().await.expect("gateway response");
        match body {
            GatewayToolResponse::Function { output, call_id } => {
                assert_eq!(output.text_content(), Some("middleware ok"));
                assert_eq!(output.success, None);
                assert_eq!(call_id, None);
            }
            other => panic!("expected function response, got {other:?}"),
        }
        assert_eq!(backend.request_count(), 1);
        let request = backend
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop()
            .expect("recorded request");
        assert_eq!(request.tool_name, "shell");
        assert_eq!(
            request.payload,
            GatewayToolPayload::Function {
                arguments: "{}".to_string()
            }
        );
        handle.abort();
    }

    #[tokio::test]
    async fn missing_auth_is_unauthorized_before_backend() {
        let backend = Arc::new(RecordingBackend::default());
        let (base_url, handle) =
            spawn_server(GatewayHttpServer::new(Arc::clone(&backend)).with_bearer_token("secret"))
                .await;

        let response = Client::new()
            .post(format!("{base_url}/tools/dispatch"))
            .json(&request_json("shell"))
            .send()
            .await
            .expect("dispatch response");

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(backend.request_count(), 0);
        handle.abort();
    }

    #[tokio::test]
    async fn invalid_json_is_bad_request_before_backend() {
        let backend = Arc::new(RecordingBackend::default());
        let (base_url, handle) =
            spawn_server(GatewayHttpServer::new(Arc::clone(&backend)).with_bearer_token("secret"))
                .await;

        let response = Client::new()
            .post(format!("{base_url}/tools/dispatch"))
            .bearer_auth("secret")
            .header("content-type", "application/json")
            .body("{not valid json")
            .send()
            .await
            .expect("dispatch response");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(backend.request_count(), 0);
        handle.abort();
    }

    #[tokio::test]
    async fn capability_denial_is_forbidden_before_backend() {
        let backend = Arc::new(RecordingBackend::default());
        let (base_url, handle) = spawn_server(
            GatewayHttpServer::new(Arc::clone(&backend))
                .with_bearer_token("secret")
                .with_capability_policy(ToolCapabilityPolicy::default().allow("read_file")),
        )
        .await;

        let response = Client::new()
            .post(format!("{base_url}/tools/dispatch"))
            .bearer_auth("secret")
            .json(&request_json("shell"))
            .send()
            .await
            .expect("dispatch response");

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(backend.request_count(), 0);
        handle.abort();
    }

    #[tokio::test]
    async fn backend_rejection_is_forbidden() {
        let backend = Arc::new(RecordingBackend::with_response(Err(
            GatewayBackendError::Rejected("denied by backend".to_string()),
        )));
        let (base_url, handle) =
            spawn_server(GatewayHttpServer::new(Arc::clone(&backend)).with_bearer_token("secret"))
                .await;

        let response = Client::new()
            .post(format!("{base_url}/tools/dispatch"))
            .bearer_auth("secret")
            .json(&request_json("shell"))
            .send()
            .await
            .expect("dispatch response");

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body: Value = response.json().await.expect("error body");
        assert_eq!(body["error"], "denied by backend");
        assert_eq!(backend.request_count(), 1);
        handle.abort();
    }

    #[derive(Default)]
    struct FakeDockerRunner {
        requests: Mutex<Vec<DockerCommandRequest>>,
        responses: Mutex<Vec<std::io::Result<DockerCommandOutput>>>,
    }

    impl FakeDockerRunner {
        fn with_responses(responses: Vec<std::io::Result<DockerCommandOutput>>) -> Self {
            Self {
                requests: Mutex::new(Vec::new()),
                responses: Mutex::new(responses.into_iter().rev().collect()),
            }
        }

        fn request(&self) -> DockerCommandRequest {
            self.requests().last().expect("request recorded").clone()
        }

        fn requests(&self) -> Vec<DockerCommandRequest> {
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        fn request_count(&self) -> usize {
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len()
        }
    }

    #[async_trait]
    impl DockerCommandRunner for FakeDockerRunner {
        async fn run(&self, request: DockerCommandRequest) -> std::io::Result<DockerCommandOutput> {
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(request);
            self.responses
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop()
                .unwrap_or_else(|| {
                    Ok(DockerCommandOutput {
                        stdout: Vec::new(),
                        stderr: Vec::new(),
                        exit_code: 0,
                    })
                })
        }
    }

    fn docker_output(
        stdout: impl Into<Vec<u8>>,
        stderr: impl Into<Vec<u8>>,
        exit_code: i32,
    ) -> std::io::Result<DockerCommandOutput> {
        Ok(DockerCommandOutput {
            stdout: stdout.into(),
            stderr: stderr.into(),
            exit_code,
        })
    }

    fn inspect_missing() -> std::io::Result<DockerCommandOutput> {
        docker_output(Vec::new(), b"not found".to_vec(), 1)
    }

    fn inspect_running() -> std::io::Result<DockerCommandOutput> {
        docker_output(b"true\n".to_vec(), Vec::new(), 0)
    }

    fn create_ok() -> std::io::Result<DockerCommandOutput> {
        docker_output(b"container-id\n".to_vec(), Vec::new(), 0)
    }

    fn exec_ok(stdout: impl Into<Vec<u8>>) -> std::io::Result<DockerCommandOutput> {
        docker_output(stdout.into(), Vec::new(), 0)
    }

    fn gateway_request(
        tool_name: &str,
        payload: GatewayToolPayload,
        cwd: impl Into<String>,
    ) -> GatewayToolRequest {
        GatewayToolRequest {
            session_id: "session-1".to_string(),
            turn_id: "turn-1".to_string(),
            cwd: cwd.into(),
            company_id: Some("acme".to_string()),
            project_id: None,
            agent_id: Some("agent-a".to_string()),
            call_id: "call-1".to_string(),
            tool_name: tool_name.to_string(),
            payload,
        }
    }

    fn gateway_request_with_scope(
        tool_name: &str,
        payload: GatewayToolPayload,
        cwd: impl Into<String>,
        company_id: &str,
        project_id: Option<&str>,
        agent_id: &str,
    ) -> GatewayToolRequest {
        let mut request = gateway_request(tool_name, payload, cwd);
        request.company_id = Some(company_id.to_string());
        request.project_id = project_id.map(str::to_string);
        request.agent_id = Some(agent_id.to_string());
        request
    }

    fn assert_company_container_lifecycle(requests: &[DockerCommandRequest]) {
        assert!(
            requests.len() >= 3,
            "expected inspect, create, exec requests"
        );
        assert_eq!(
            requests[0].args,
            ["inspect", "-f", "{{.State.Running}}", "codex-company-acme"]
        );
        assert_eq!(requests[1].args[0], "run");
        assert_eq!(requests[1].args[1], "-d");
        assert!(requests[1].args.iter().any(|arg| arg == "--name"));
        assert!(
            requests[1]
                .args
                .iter()
                .any(|arg| arg == "codex-company-acme")
        );
        assert!(requests[1].args.iter().any(|arg| arg == "codex-test-image"));
        assert!(
            requests[1]
                .args
                .iter()
                .any(|arg| arg.ends_with(":/workspace:rw"))
        );
        assert_eq!(requests[2].args[0], "exec");
        assert!(
            requests[2]
                .args
                .iter()
                .any(|arg| arg == "codex-company-acme")
        );
    }

    fn command_after_container<'a>(
        request: &'a DockerCommandRequest,
        container_name: &str,
    ) -> &'a [String] {
        let container_pos = request
            .args
            .iter()
            .position(|arg| arg == container_name)
            .expect("container name present");
        &request.args[container_pos + 1..]
    }

    #[tokio::test]
    async fn office_task_14_end_to_end_production_acceptance_scenario() -> anyhow::Result<()> {
        debug_trace::reset_for_tests();
        let temp = tempdir()?;
        let office_store = temp.path().join("office-store.json");
        let codex_home = temp.path().join("codex-home");
        let docker_workspace = temp.path().join("docker-workspace");
        let trace_root = temp.path().join("debug-trace");
        let owner_message_id = "task14-owner-msg";
        let runtime_blackboard_marker = "task14-runtime-blackboard-marker";
        let stable_memory_marker = "Task14 owner prefers durable Rust architecture summaries.";

        let office_app = OfficeWebApp::open(OfficeWebConfig::new(office_store.clone()))?;
        let (office_base_url, office_handle) = spawn_office_web_app(office_app).await;
        let client = Client::new();

        let login = client
            .post(format!("{office_base_url}/api/login"))
            .json(&json!({ "username": "employee_b", "password": "password" }))
            .send()
            .await?;
        assert_eq!(login.status(), StatusCode::OK);
        let owner_cookie = response_session_cookie(&login);
        let login_body: Value = login.json().await?;
        assert_eq!(login_body["me"]["user_id"].as_str(), Some("user_b"));
        assert_eq!(login_body["me"]["agent_id"].as_str(), Some("agent_b"));
        assert_eq!(
            login_body["me"]["agent_profile"]["owner_user_id"].as_str(),
            Some("user_b")
        );

        let states = serde_json::to_value([
            OfficeAgentExternalState::Idle,
            OfficeAgentExternalState::Working,
            OfficeAgentExternalState::Waiting,
        ])?;
        assert_eq!(states, json!(["idle", "working", "waiting"]));
        let me = client
            .get(format!("{office_base_url}/api/me"))
            .header("accept", "application/json")
            .header("cookie", owner_cookie.as_str())
            .send()
            .await?;
        assert_eq!(me.status(), StatusCode::OK);
        let me_body: Value = me.json().await?;
        assert_eq!(me_body["agent_state"]["state"].as_str(), Some("idle"));

        let owner_message = client
            .post(format!("{office_base_url}/api/inbox"))
            .header("cookie", owner_cookie.as_str())
            .json(&json!({
                "message_id": owner_message_id,
                "content": "Task14 owner asks agent B for a production readiness pass",
                "need_reply": true,
                "target_agent_id": "agent_c"
            }))
            .send()
            .await?;
        assert_eq!(owner_message.status(), StatusCode::OK);
        let owner_message_body: Value = owner_message.json().await?;
        assert_eq!(
            owner_message_body["queued_to_agent_id"].as_str(),
            Some("agent_b")
        );
        assert_eq!(
            owner_message_body["message"]["from"].as_str(),
            Some("user_b")
        );
        assert_eq!(
            owner_message_body["message"]["to"].as_str(),
            Some("agent_b")
        );
        assert_eq!(
            owner_message_body["me"]["pending_owner_replies"][0]["message_id"].as_str(),
            Some(owner_message_id)
        );

        let other_login = client
            .post(format!("{office_base_url}/api/login"))
            .json(&json!({ "username": "employee_c", "password": "password" }))
            .send()
            .await?;
        assert_eq!(other_login.status(), StatusCode::OK);
        let other_cookie = response_session_cookie(&other_login);
        let other_me = client
            .get(format!("{office_base_url}/api/me"))
            .header("accept", "application/json")
            .header("cookie", other_cookie.as_str())
            .send()
            .await?;
        let other_me_body: Value = other_me.json().await?;
        assert_eq!(other_me_body["user_id"].as_str(), Some("user_c"));
        assert_eq!(other_me_body["agent_id"].as_str(), Some("agent_c"));
        assert_eq!(
            other_me_body["agent_inbox"]["queued_count"].as_u64(),
            Some(0)
        );

        office_handle.abort();
        let persisted_office = PersistentPilotDirectory::open(office_store)?;
        let persisted_session = persisted_office
            .web_session(
                login_body["session_token"]
                    .as_str()
                    .expect("session token should be present"),
            )
            .expect("web session persists");
        assert_eq!(persisted_session.user_id, "user_b");
        assert_eq!(persisted_session.agent_id, "agent_b");

        let agent_store = AgentContextStore::new(&codex_home, "agent_b", "Agent B");
        let agent_paths = agent_store.ensure_layout().await?;
        tokio::fs::write(
            &agent_paths.manual_system_prompt_file,
            "Task14 manual permanent prompt for agent B\n",
        )
        .await?;
        let transient_runtime_candidate = format!(
            "Currently waiting on inbox and blackboard item {runtime_blackboard_marker} for {owner_message_id}"
        );
        let journal = vec![
            task14_memory_candidate(
                AgentAutomaticPromptSection::OwnerPreference,
                stable_memory_marker,
            ),
            task14_memory_candidate(
                AgentAutomaticPromptSection::StableCollaborationRule,
                &transient_runtime_candidate,
            ),
        ];
        tokio::fs::write(
            agent_paths.task_journal_dir.join("task14.json"),
            serde_json::to_string_pretty(&journal)?,
        )
        .await?;
        let update_time = chrono::Utc
            .with_ymd_and_hms(2026, 5, 15, 12, 0, 0)
            .single()
            .expect("valid update time");
        let update_report = agent_store
            .refresh_automatic_prompt_once_per_day(update_time)
            .await?;
        assert_eq!(
            update_report.status,
            AgentAutomaticPromptUpdateStatus::Updated
        );
        assert_eq!(update_report.admitted_facts, 1);
        assert_eq!(update_report.rejected_facts, 1);
        assert!(update_report.reflection_file.exists());
        let second_update = agent_store
            .refresh_automatic_prompt_once_per_day(update_time)
            .await?;
        assert_eq!(
            second_update.status,
            AgentAutomaticPromptUpdateStatus::SkippedAlreadyUpdated
        );
        let automatic_prompt =
            tokio::fs::read_to_string(&agent_paths.automatic_prompt_file).await?;
        let manual_prompt =
            tokio::fs::read_to_string(&agent_paths.manual_system_prompt_file).await?;
        assert!(agent_paths.agent_root.starts_with(&codex_home));
        assert!(automatic_prompt.contains(stable_memory_marker));
        assert!(!automatic_prompt.contains(runtime_blackboard_marker));
        assert!(!automatic_prompt.contains(owner_message_id));
        assert_eq!(
            manual_prompt,
            "Task14 manual permanent prompt for agent B\n"
        );

        let durable_context = agent_store
            .load_prompt_bundle()
            .await?
            .render_developer_instructions()
            .expect("durable context should render");
        let blackboard_path = temp.path().join("task14-blackboard.md");
        std::fs::write(
            &blackboard_path,
            format!("[wmj-assistant]：{runtime_blackboard_marker}\n"),
        )?;
        let trace_context = DebugTraceContext {
            conversation_id: ThreadId::new(),
            turn_id: "task14-turn".to_string(),
            session_source: SessionSource::Cli,
            collaboration_mode_kind: ModeKind::Swarm,
            reasoning_effort: None,
            base_instructions: None,
            initial_context_items: None,
            developer_instructions: None,
            user_instructions: None,
            agent_name: Some("agent_b".to_string()),
            shared_blackboard_path: Some(blackboard_path.clone()),
        };
        let runtime_tail = format!(
            "Below are the agents collaborating with you. You can use the `call` tool to communicate and collaborate with them.\nCurrent time: 2026-05-15 22:46:04 UTC\n\nCollaborating agents and summary lists (first 40 summaries per agent):\n- agent_b: [processing {owner_message_id}]\n\nBelow is the shared blackboard content:\n[wmj-assistant]：{runtime_blackboard_marker}"
        );
        let request_input = vec![
            ResponseItem::Message {
                id: None,
                role: "developer".to_string(),
                content: vec![ContentItem::InputText {
                    text: durable_context,
                }],
                end_turn: None,
                phase: None,
            },
            ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText { text: runtime_tail }],
                end_turn: None,
                phase: None,
            },
        ];
        let trace_event =
            debug_trace::record_request_context(&trace_root, &trace_context, &request_input)
                .expect("request context trace should record sections");
        let manual_source = agent_paths.manual_system_prompt_file.display().to_string();
        let automatic_source = agent_paths.automatic_prompt_file.display().to_string();
        let blackboard_source = blackboard_path.display().to_string();
        let sections = &trace_event.snapshot.context_sections;
        let manual_section = sections
            .iter()
            .find(|section| {
                section.channel == DebugTraceContextChannel::ManualPermanentSystemPrompt
            })
            .expect("manual durable section");
        assert_eq!(manual_section.role, DebugTraceRole::Developer);
        assert_eq!(
            manual_section.source_path.as_deref(),
            Some(manual_source.as_str())
        );
        let automatic_section = sections
            .iter()
            .find(|section| section.channel == DebugTraceContextChannel::AutomaticUpdatedPrompt)
            .expect("automatic durable section");
        assert_eq!(automatic_section.role, DebugTraceRole::Developer);
        assert_eq!(
            automatic_section.source_path.as_deref(),
            Some(automatic_source.as_str())
        );
        assert!(automatic_section.content.contains(stable_memory_marker));
        for durable_section in sections.iter().filter(|section| {
            matches!(
                section.channel,
                DebugTraceContextChannel::ManualPermanentSystemPrompt
                    | DebugTraceContextChannel::AutomaticUpdatedPrompt
            )
        }) {
            assert!(!durable_section.content.contains(runtime_blackboard_marker));
            assert!(!durable_section.content.contains(owner_message_id));
        }
        let runtime_sections = sections
            .iter()
            .filter(|section| section.channel == DebugTraceContextChannel::RuntimeTailInjection)
            .collect::<Vec<_>>();
        assert_eq!(runtime_sections.len(), 2);
        assert!(
            runtime_sections
                .iter()
                .all(|section| section.role == DebugTraceRole::User)
        );
        let blackboard_section = runtime_sections
            .iter()
            .find(|section| section.source_kind == "shared_blackboard.snapshot")
            .expect("blackboard runtime section");
        assert_eq!(
            blackboard_section.source_path.as_deref(),
            Some(blackboard_source.as_str())
        );
        assert!(
            blackboard_section
                .content
                .contains(runtime_blackboard_marker)
        );
        assert!(
            runtime_sections
                .iter()
                .all(|section| !section.content.contains(stable_memory_marker))
        );
        assert!(
            trace_root
                .join(trace_event.conversation_id.to_string())
                .join("history.latest.json")
                .exists()
        );

        let fake_runner = Arc::new(FakeDockerRunner::with_responses(vec![
            inspect_missing(),
            create_ok(),
            exec_ok(b"first exec\n".to_vec()),
            inspect_running(),
            exec_ok(b"second exec\n".to_vec()),
        ]));
        let backend = DockerGatewayBackend::new("codex-test-image", &docker_workspace)
            .with_uid_gid(None)
            .with_runner(Arc::clone(&fake_runner));
        let first_request = gateway_request_with_scope(
            "shell",
            GatewayToolPayload::Function {
                arguments: json!({ "command": ["pwd"] }).to_string(),
            },
            "/workspace",
            "Acme Integrated",
            Some("Project X"),
            "Agent B",
        );
        let second_request = gateway_request_with_scope(
            "shell",
            GatewayToolPayload::Function {
                arguments: json!({ "command": ["pwd"] }).to_string(),
            },
            "/workspace",
            "Acme Integrated",
            Some("Project X"),
            "Agent B",
        );
        backend.dispatch(first_request).await?;
        backend.dispatch(second_request).await?;
        assert!(docker_workspace.join("public").is_dir());
        assert!(docker_workspace.join("agents/agent-b/private").is_dir());
        assert!(docker_workspace.join("projects/project-x").is_dir());
        let docker_requests = fake_runner.requests();
        assert_eq!(docker_requests.len(), 5);
        assert_eq!(docker_requests[0].args[0], "inspect");
        assert_eq!(docker_requests[1].args[0], "run");
        assert_eq!(docker_requests[2].args[0], "exec");
        assert_eq!(docker_requests[3].args[0], "inspect");
        assert_eq!(docker_requests[4].args[0], "exec");
        assert_eq!(
            docker_requests
                .iter()
                .filter(|request| request.args.first().is_some_and(|arg| arg == "run"))
                .count(),
            1
        );
        assert!(
            docker_requests
                .iter()
                .all(|request| request.container_name == "codex-company-acme-integrated")
        );
        for exec_request in [&docker_requests[2], &docker_requests[4]] {
            let workdir_pos = exec_request
                .args
                .iter()
                .position(|arg| arg == "-w")
                .expect("docker exec workdir flag");
            assert_eq!(
                exec_request.args[workdir_pos + 1],
                "/workspace/projects/project-x"
            );
        }

        Ok(())
    }

    #[tokio::test]
    async fn docker_backend_uses_company_container_and_exec_for_shell_command() -> anyhow::Result<()>
    {
        let workspace = tempdir()?;
        std::fs::write(workspace.path().join("hello.txt"), "hello\nworld\n")?;
        let fake_runner = Arc::new(FakeDockerRunner::with_responses(vec![
            inspect_missing(),
            create_ok(),
            docker_output(b"ok".to_vec(), b"warn".to_vec(), 0),
        ]));
        let backend = DockerGatewayBackend::new("codex-test-image", workspace.path())
            .with_uid_gid(None)
            .with_limits("256m", "0.5", 64)
            .with_default_timeout(Duration::from_secs(12))
            .with_runner(Arc::clone(&fake_runner));

        let response = backend
            .dispatch(gateway_request(
                "shell",
                GatewayToolPayload::Function {
                    arguments: json!({
                        "command": ["python3", "-c", "print(1)"],
                        "workdir": workspace.path().to_string_lossy(),
                        "timeout_ms": 2500
                    })
                    .to_string(),
                },
                workspace.path().to_string_lossy().to_string(),
            ))
            .await?;

        let requests = fake_runner.requests();
        assert_company_container_lifecycle(&requests);
        let exec = &requests[2];
        assert_eq!(exec.executable, "docker");
        assert_eq!(exec.timeout, Duration::from_millis(2500));
        assert_eq!(exec.args[0], "exec");
        assert_eq!(
            command_after_container(exec, "codex-company-acme"),
            &[
                "python3".to_string(),
                "-c".to_string(),
                "print(1)".to_string()
            ]
        );
        assert!(workspace.path().join("public").is_dir());
        assert!(workspace.path().join("agents/agent-a/private").is_dir());

        match response {
            GatewayToolResponse::Function { output, .. } => {
                assert_eq!(output.success, Some(true));
                assert_eq!(
                    output.text_content(),
                    Some(r#"{"stdout":"ok","stderr":"warn","exit_code":0}"#)
                );
            }
            other => panic!("expected function output, got {other:?}"),
        }

        Ok(())
    }

    #[tokio::test]
    async fn docker_backend_reuses_running_company_container_without_recreate() -> anyhow::Result<()>
    {
        let workspace = tempdir()?;
        let fake_runner = Arc::new(FakeDockerRunner::with_responses(vec![
            inspect_running(),
            exec_ok(b"/workspace\n".to_vec()),
        ]));
        let backend = DockerGatewayBackend::new("codex-test-image", workspace.path())
            .with_uid_gid(None)
            .with_runner(Arc::clone(&fake_runner));

        backend
            .dispatch(gateway_request(
                "shell",
                GatewayToolPayload::Function {
                    arguments: json!({
                        "command": ["pwd"],
                        "workdir": "/workspace"
                    })
                    .to_string(),
                },
                "/workspace",
            ))
            .await?;

        let requests = fake_runner.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0].args,
            ["inspect", "-f", "{{.State.Running}}", "codex-company-acme"]
        );
        assert_eq!(requests[1].args[0], "exec");
        assert!(
            !requests
                .iter()
                .any(|request| request.args.get(0).is_some_and(|arg| arg == "run"))
        );
        let workdir_pos = requests[1]
            .args
            .iter()
            .position(|arg| arg == "-w")
            .expect("workdir flag present");
        assert_eq!(requests[1].args[workdir_pos + 1], "/workspace");

        Ok(())
    }

    #[tokio::test]
    async fn docker_backend_maps_logical_workspace_paths_for_read_file() -> anyhow::Result<()> {
        let workspace = tempdir()?;
        std::fs::write(workspace.path().join("hello.txt"), "hello\nworld\n")?;
        let fake_runner = Arc::new(FakeDockerRunner::with_responses(vec![
            inspect_running(),
            exec_ok(b"L1: hello\nL2: world\n".to_vec()),
        ]));
        let backend = DockerGatewayBackend::new("codex-test-image", workspace.path())
            .with_uid_gid(None)
            .with_runner(Arc::clone(&fake_runner));

        backend
            .dispatch(gateway_request(
                "read_file",
                GatewayToolPayload::Function {
                    arguments: json!({
                        "file_path": "/workspace/hello.txt",
                        "offset": 1,
                        "limit": 2
                    })
                    .to_string(),
                },
                "/workspace",
            ))
            .await?;

        let recorded = fake_runner.request();
        let command = command_after_container(&recorded, "codex-company-acme");
        assert_eq!(command[0], "python3");
        assert_eq!(command[3], "/workspace/hello.txt");

        Ok(())
    }

    #[tokio::test]
    async fn docker_backend_uses_project_workdir_and_agent_private_partition() -> anyhow::Result<()>
    {
        let workspace = tempdir()?;
        let fake_runner = Arc::new(FakeDockerRunner::with_responses(vec![
            inspect_running(),
            exec_ok(b"/workspace/projects/project-x\n".to_vec()),
        ]));
        let backend = DockerGatewayBackend::new("codex-test-image", workspace.path())
            .with_uid_gid(None)
            .with_runner(Arc::clone(&fake_runner));

        backend
            .dispatch(gateway_request_with_scope(
                "shell",
                GatewayToolPayload::Function {
                    arguments: json!({ "command": ["pwd"] }).to_string(),
                },
                "/workspace",
                "Acme Inc",
                Some("Project X"),
                "Agent B",
            ))
            .await?;

        assert!(workspace.path().join("public").is_dir());
        assert!(workspace.path().join("agents/agent-b/private").is_dir());
        assert!(workspace.path().join("projects/project-x").is_dir());
        let requests = fake_runner.requests();
        assert_eq!(requests[0].args[3], "codex-company-acme-inc");
        let exec = &requests[1];
        let workdir_pos = exec
            .args
            .iter()
            .position(|arg| arg == "-w")
            .expect("workdir flag present");
        assert_eq!(exec.args[workdir_pos + 1], "/workspace/projects/project-x");

        Ok(())
    }

    #[tokio::test]
    async fn docker_backend_rejects_escalated_sandbox_permissions() -> anyhow::Result<()> {
        let workspace = tempdir()?;
        let backend = DockerGatewayBackend::new("codex-test-image", workspace.path())
            .with_uid_gid(None)
            .with_runner(Arc::new(FakeDockerRunner::default()));

        let err = backend
            .dispatch(gateway_request(
                "shell",
                GatewayToolPayload::Function {
                    arguments: json!({
                        "command": ["echo", "hi"],
                        "workdir": workspace.path().to_string_lossy(),
                        "timeout_ms": 1000,
                        "sandbox_permissions": "require_escalated"
                    })
                    .to_string(),
                },
                workspace.path().to_string_lossy().to_string(),
            ))
            .await
            .expect_err("escalated permissions should be rejected");

        assert!(
            err.to_string()
                .contains("does not allow escalated sandbox permissions")
        );
        Ok(())
    }

    #[tokio::test]
    async fn docker_backend_translates_apply_patch_function_into_docker_exec() -> anyhow::Result<()>
    {
        let workspace = tempdir()?;
        let target = workspace.path().join("absolute.txt");
        let patch = format!(
            "*** Begin Patch\n*** Add File: {}\n+hello\n*** End Patch\n",
            target.display()
        );
        let fake_runner = Arc::new(FakeDockerRunner::with_responses(vec![
            inspect_running(),
            exec_ok(b"Success. Updated the following files:\nA /workspace/absolute.txt\n".to_vec()),
        ]));
        let backend = DockerGatewayBackend::new("codex-test-image", workspace.path())
            .with_uid_gid(None)
            .with_default_timeout(Duration::from_secs(12))
            .with_runner(Arc::clone(&fake_runner));

        let response = backend
            .dispatch(gateway_request(
                "apply_patch",
                GatewayToolPayload::Function {
                    arguments: json!({ "input": patch }).to_string(),
                },
                workspace.path().to_string_lossy().to_string(),
            ))
            .await?;

        let recorded = fake_runner.request();
        assert_eq!(recorded.args[0], "exec");
        assert_eq!(recorded.timeout, Duration::from_secs(12));
        assert_eq!(
            command_after_container(&recorded, "codex-company-acme"),
            &[
                "apply_patch".to_string(),
                "*** Begin Patch\n*** Add File: /workspace/absolute.txt\n+hello\n*** End Patch"
                    .to_string()
            ]
        );

        match response {
            GatewayToolResponse::Function { output, .. } => {
                assert_eq!(output.success, Some(true));
                assert!(
                    output
                        .text_content()
                        .expect("text output")
                        .contains(r#""exit_code":0"#)
                );
            }
            other => panic!("expected function output, got {other:?}"),
        }

        Ok(())
    }

    #[tokio::test]
    async fn docker_backend_translates_apply_patch_custom_into_custom_response()
    -> anyhow::Result<()> {
        let workspace = tempdir()?;
        let patch =
            "*** Begin Patch\n*** Add File: custom.txt\n+custom\n*** End Patch\n".to_string();
        let fake_runner = Arc::new(FakeDockerRunner::with_responses(vec![
            inspect_running(),
            exec_ok(b"Success. Updated the following files:\nA custom.txt\n".to_vec()),
        ]));
        let backend = DockerGatewayBackend::new("codex-test-image", workspace.path())
            .with_uid_gid(None)
            .with_runner(Arc::clone(&fake_runner));

        let response = backend
            .dispatch(gateway_request(
                "apply_patch",
                GatewayToolPayload::Custom { input: patch },
                workspace.path().to_string_lossy().to_string(),
            ))
            .await?;

        let recorded = fake_runner.request();
        assert_eq!(recorded.args[0], "exec");
        assert_eq!(
            command_after_container(&recorded, "codex-company-acme"),
            &[
                "apply_patch".to_string(),
                "*** Begin Patch\n*** Add File: custom.txt\n+custom\n*** End Patch".to_string()
            ]
        );

        match response {
            GatewayToolResponse::Custom { output, .. } => {
                assert!(output.contains(r#""exit_code":0"#));
                assert!(output.contains("custom.txt"));
            }
            other => panic!("expected custom output, got {other:?}"),
        }

        Ok(())
    }

    #[tokio::test]
    async fn docker_backend_translates_logical_apply_patch_path_into_container_path()
    -> anyhow::Result<()> {
        let workspace = tempdir()?;
        let patch =
            "*** Begin Patch\n*** Add File: /workspace/custom.txt\n+custom\n*** End Patch\n";
        let fake_runner = Arc::new(FakeDockerRunner::with_responses(vec![
            inspect_running(),
            exec_ok(b"Success. Updated the following files:\nA /workspace/custom.txt\n".to_vec()),
        ]));
        let backend = DockerGatewayBackend::new("codex-test-image", workspace.path())
            .with_uid_gid(None)
            .with_runner(Arc::clone(&fake_runner));

        backend
            .dispatch(gateway_request(
                "apply_patch",
                GatewayToolPayload::Custom {
                    input: patch.to_string(),
                },
                "/workspace",
            ))
            .await?;

        let recorded = fake_runner.request();
        assert_eq!(recorded.args[0], "exec");
        assert_eq!(
            command_after_container(&recorded, "codex-company-acme"),
            &[
                "apply_patch".to_string(),
                "*** Begin Patch\n*** Add File: /workspace/custom.txt\n+custom\n*** End Patch"
                    .to_string()
            ]
        );

        Ok(())
    }

    #[tokio::test]
    async fn docker_backend_rejects_apply_patch_paths_outside_workspace() -> anyhow::Result<()> {
        let workspace = tempdir()?;
        let fake_runner = Arc::new(FakeDockerRunner::default());
        let backend = DockerGatewayBackend::new("codex-test-image", workspace.path())
            .with_uid_gid(None)
            .with_runner(Arc::clone(&fake_runner));
        let patch = "*** Begin Patch\n*** Add File: /tmp/codex-outside-workspace.txt\n+bad\n*** End Patch\n";

        let err = backend
            .dispatch(gateway_request(
                "apply_patch",
                GatewayToolPayload::Custom {
                    input: patch.to_string(),
                },
                workspace.path().to_string_lossy().to_string(),
            ))
            .await
            .expect_err("outside apply_patch target should be rejected");

        assert!(
            err.to_string()
                .contains("apply_patch target must stay inside docker workspace_root")
        );
        assert_eq!(fake_runner.request_count(), 0);

        Ok(())
    }
}
