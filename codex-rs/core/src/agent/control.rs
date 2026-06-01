use crate::agent::AgentStatus;
use crate::agent::guards::AgentWorkStatus;
use crate::agent::guards::Guards;
use crate::codex_thread::ThreadConfigSnapshot;
use crate::error::CodexErr;
use crate::error::Result as CodexResult;
use crate::thread_manager::ThreadManagerState;
use codex_protocol::ThreadId;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::UserInputOrigin;
use codex_protocol::user_input::UserInput;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Weak;
use tokio::sync::watch;

/// Control-plane handle for multi-agent operations.
/// `AgentControl` is held by each session (via `SessionServices`). It provides capability to
/// spawn new agents and the inter-agent communication layer.
/// An `AgentControl` instance is shared per "user session" which means the same `AgentControl`
/// is used for every sub-agent spawned by Codex. By doing so, we make sure the guards are
/// scoped to a user session.
#[derive(Clone, Default)]
pub(crate) struct AgentControl {
    /// Weak handle back to the global thread registry/state.
    /// This is `Weak` to avoid reference cycles and shadow persistence of the form
    /// `ThreadManagerState -> CodexThread -> Session -> SessionServices -> ThreadManagerState`.
    manager: Weak<ThreadManagerState>,
    state: Arc<Guards>,
}

pub(crate) const PRIMARY_AGENT_NAME: &str = "wmj-assistant";
pub(crate) const UNNAMED_AGENT_NAME: &str = "unnamed-agent";

impl AgentControl {
    /// Construct a new `AgentControl` that can spawn/message agents via the given manager state.
    pub(crate) fn new(manager: Weak<ThreadManagerState>) -> Self {
        Self {
            manager,
            ..Default::default()
        }
    }

    /// Spawn a new agent thread.
    pub(crate) async fn spawn_agent(
        &self,
        config: crate::config::Config,
        session_source: Option<SessionSource>,
    ) -> CodexResult<ThreadId> {
        let state = self.upgrade()?;
        let reservation = self.state.reserve_spawn_slot(config.agent_max_threads)?;

        // The same `AgentControl` is sent to spawn the thread.
        let new_thread = match session_source {
            Some(session_source) => {
                state
                    .spawn_new_thread_with_source(config, self.clone(), session_source, false)
                    .await?
            }
            None => state.spawn_new_thread(config, self.clone()).await?,
        };
        reservation.commit(new_thread.thread_id);

        // Notify a new thread has been created. This notification will be processed by clients
        // to subscribe or drain this newly created thread.
        // TODO(jif) add helper for drain
        state.notify_thread_created(new_thread.thread_id);

        Ok(new_thread.thread_id)
    }

    /// Resume an existing agent thread from a recorded rollout file.
    pub(crate) async fn resume_agent_from_rollout(
        &self,
        config: crate::config::Config,
        rollout_path: PathBuf,
        session_source: SessionSource,
    ) -> CodexResult<ThreadId> {
        let state = self.upgrade()?;
        let reservation = self.state.reserve_spawn_slot(config.agent_max_threads)?;

        let resumed_thread = state
            .resume_thread_from_rollout_with_source(
                config,
                rollout_path,
                self.clone(),
                session_source,
            )
            .await?;
        reservation.commit(resumed_thread.thread_id);
        // Resumed threads are re-registered in-memory and need the same listener
        // attachment path as freshly spawned threads.
        state.notify_thread_created(resumed_thread.thread_id);

        Ok(resumed_thread.thread_id)
    }

    /// Send rich user input items to an existing agent thread.
    pub(crate) async fn send_input(
        &self,
        agent_id: ThreadId,
        items: Vec<UserInput>,
    ) -> CodexResult<String> {
        let state = self.upgrade()?;
        let result = state
            .send_op(
                agent_id,
                Op::UserInput {
                    items,
                    origin: UserInputOrigin::AgentCall,
                    final_output_json_schema: None,
                },
            )
            .await;
        if matches!(result, Err(CodexErr::InternalAgentDied)) {
            let _ = state.remove_thread(&agent_id).await;
            self.state.release_spawned_thread(agent_id);
        }
        result
    }

    /// Update an agent thread's collaboration mode.
    pub(crate) async fn set_collaboration_mode(
        &self,
        agent_id: ThreadId,
        collaboration_mode: CollaborationMode,
    ) -> CodexResult<String> {
        let state = self.upgrade()?;
        let result = state
            .send_op(agent_id, collaboration_mode_override_op(collaboration_mode))
            .await;
        if matches!(result, Err(CodexErr::InternalAgentDied)) {
            let _ = state.remove_thread(&agent_id).await;
            self.state.release_spawned_thread(agent_id);
        }
        result
    }

    /// Update collaboration mode for all active sub-agents directly spawned by `parent_thread_id`.
    ///
    /// Returns per-agent failures while still attempting all updates.
    pub(crate) async fn set_spawned_agents_collaboration_mode(
        &self,
        parent_thread_id: ThreadId,
        collaboration_mode: CollaborationMode,
    ) -> CodexResult<Vec<(ThreadId, CodexErr)>> {
        let state = self.upgrade()?;
        let child_ids = state.list_thread_ids_spawned_by(parent_thread_id).await;
        let mut failures = Vec::new();

        for child_id in child_ids {
            let result = state
                .send_op(
                    child_id,
                    collaboration_mode_override_op(collaboration_mode.clone()),
                )
                .await;
            if let Err(err) = result {
                if matches!(
                    err,
                    CodexErr::InternalAgentDied | CodexErr::ThreadNotFound(_)
                ) {
                    let _ = state.remove_thread(&child_id).await;
                    self.state.release_spawned_thread(child_id);
                }
                failures.push((child_id, err));
            }
        }

        Ok(failures)
    }

    /// Interrupt the current task for an existing agent thread.
    pub(crate) async fn interrupt_agent(&self, agent_id: ThreadId) -> CodexResult<String> {
        let state = self.upgrade()?;
        state.send_op(agent_id, Op::Interrupt).await
    }

    /// Submit a shutdown request to an existing agent thread.
    pub(crate) async fn shutdown_agent(&self, agent_id: ThreadId) -> CodexResult<String> {
        let state = self.upgrade()?;
        let result = state.send_op(agent_id, Op::Shutdown {}).await;
        let _ = state.remove_thread(&agent_id).await;
        self.state.release_spawned_thread(agent_id);
        result
    }

    /// Fetch the last known status for `agent_id`, returning `NotFound` when unavailable.
    pub(crate) async fn get_status(&self, agent_id: ThreadId) -> AgentStatus {
        let Ok(state) = self.upgrade() else {
            // No agent available if upgrade fails.
            return AgentStatus::NotFound;
        };
        let Ok(thread) = state.get_thread(agent_id).await else {
            return AgentStatus::NotFound;
        };
        thread.agent_status().await
    }

    /// Subscribe to status updates for `agent_id`, yielding the latest value and changes.
    pub(crate) async fn subscribe_status(
        &self,
        agent_id: ThreadId,
    ) -> CodexResult<watch::Receiver<AgentStatus>> {
        let state = self.upgrade()?;
        let thread = state.get_thread(agent_id).await?;
        Ok(thread.subscribe_status())
    }

    pub(crate) async fn thread_config_snapshot(
        &self,
        agent_id: ThreadId,
    ) -> CodexResult<ThreadConfigSnapshot> {
        let state = self.upgrade()?;
        let thread = state.get_thread(agent_id).await?;
        Ok(thread.config_snapshot().await)
    }

    pub(crate) fn register_agent_name(
        &self,
        agent_id: ThreadId,
        name: &str,
    ) -> std::result::Result<(), String> {
        self.state.register_agent_name(agent_id, name)
    }

    pub(crate) fn reserve_agent_name(&self, name: &str) -> std::result::Result<(), String> {
        self.state.reserve_agent_name(name)
    }

    pub(crate) fn release_agent_name_reservation(&self, name: &str) {
        self.state.release_agent_name_reservation(name);
    }

    pub(crate) fn thread_id_for_agent_name(&self, name: &str) -> Option<ThreadId> {
        self.state.find_thread_by_name(name)
    }

    pub(crate) fn agent_name_for_thread(&self, thread_id: ThreadId) -> Option<String> {
        self.state.find_name_by_thread(thread_id)
    }

    pub(crate) fn is_agent_name_used(&self, name: &str) -> bool {
        self.state.is_agent_name_used(name)
    }

    pub(crate) fn known_agent_names(&self) -> Vec<String> {
        self.state.known_agent_names()
    }

    pub(crate) fn record_agent_work_summary(&self, thread_id: ThreadId, summary: String) {
        self.state.record_agent_work_summary(thread_id, summary);
    }

    pub(crate) fn other_agents_work_status(
        &self,
        current_thread_id: ThreadId,
    ) -> Vec<AgentWorkStatus> {
        self.state.other_agents_work_status(current_thread_id)
    }

    pub(crate) fn agent_work_status(&self, thread_id: ThreadId) -> Option<AgentWorkStatus> {
        self.state.agent_work_status(thread_id)
    }

    pub(crate) async fn list_agent_ids(&self) -> CodexResult<Vec<ThreadId>> {
        let state = self.upgrade()?;
        Ok(state.list_thread_ids().await)
    }

    pub(crate) fn queue_primary_completion_notice(&self, thread_id: ThreadId, notice: String) {
        self.state
            .queue_primary_completion_notice(thread_id, notice);
    }

    pub(crate) fn take_primary_completion_notice(&self, thread_id: ThreadId) -> Option<String> {
        self.state.take_primary_completion_notice(thread_id)
    }

    pub(crate) fn clear_primary_completion_notice(&self, thread_id: ThreadId) {
        self.state.clear_primary_completion_notice(thread_id);
    }

    fn upgrade(&self) -> CodexResult<Arc<ThreadManagerState>> {
        self.manager
            .upgrade()
            .ok_or_else(|| CodexErr::UnsupportedOperation("thread manager dropped".to_string()))
    }
}

fn collaboration_mode_override_op(collaboration_mode: CollaborationMode) -> Op {
    Op::OverrideTurnContext {
        cwd: None,
        approval_policy: None,
        sandbox_policy: None,
        windows_sandbox_level: None,
        model: None,
        effort: None,
        summary: None,
        collaboration_mode: Some(collaboration_mode),
        personality: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CodexAuth;
    use crate::CodexThread;
    use crate::ThreadManager;
    use crate::agent::agent_status_from_event;
    use crate::config::Config;
    use crate::config::ConfigBuilder;
    use assert_matches::assert_matches;
    use codex_protocol::config_types::CollaborationMode;
    use codex_protocol::config_types::ModeKind;
    use codex_protocol::config_types::Settings;
    use codex_protocol::protocol::ErrorEvent;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::SessionSource;
    use codex_protocol::protocol::SubAgentSource;
    use codex_protocol::protocol::TurnAbortReason;
    use codex_protocol::protocol::TurnAbortedEvent;
    use codex_protocol::protocol::TurnCompleteEvent;
    use codex_protocol::protocol::TurnStartedEvent;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;
    use toml::Value as TomlValue;

    async fn test_config_with_cli_overrides(
        cli_overrides: Vec<(String, TomlValue)>,
    ) -> (TempDir, Config) {
        let home = TempDir::new().expect("create temp dir");
        let config = ConfigBuilder::default()
            .codex_home(home.path().to_path_buf())
            .cli_overrides(cli_overrides)
            .build()
            .await
            .expect("load default test config");
        (home, config)
    }

    async fn test_config() -> (TempDir, Config) {
        test_config_with_cli_overrides(Vec::new()).await
    }

    struct AgentControlHarness {
        _home: TempDir,
        config: Config,
        manager: ThreadManager,
        control: AgentControl,
    }

    impl AgentControlHarness {
        async fn new() -> Self {
            let (home, config) = test_config().await;
            let manager = ThreadManager::with_models_provider_and_home_for_tests(
                CodexAuth::from_api_key("dummy"),
                config.model_provider.clone(),
                config.codex_home.clone(),
            );
            let control = manager.agent_control();
            Self {
                _home: home,
                config,
                manager,
                control,
            }
        }

        async fn start_thread(&self) -> (ThreadId, Arc<CodexThread>) {
            let new_thread = self
                .manager
                .start_thread(self.config.clone())
                .await
                .expect("start thread");
            (new_thread.thread_id, new_thread.thread)
        }
    }

    #[tokio::test]
    async fn send_input_errors_when_manager_dropped() {
        let control = AgentControl::default();
        let err = control
            .send_input(
                ThreadId::new(),
                vec![UserInput::Text {
                    text: "hello".to_string(),
                    text_elements: Vec::new(),
                }],
            )
            .await
            .expect_err("send_input should fail without a manager");
        assert_eq!(
            err.to_string(),
            "unsupported operation: thread manager dropped"
        );
    }

    #[tokio::test]
    async fn get_status_returns_not_found_without_manager() {
        let control = AgentControl::default();
        let got = control.get_status(ThreadId::new()).await;
        assert_eq!(got, AgentStatus::NotFound);
    }

    #[tokio::test]
    async fn on_event_updates_status_from_task_started() {
        let status = agent_status_from_event(&EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "turn-1".to_string(),
            model_context_window: None,
            collaboration_mode_kind: ModeKind::Default,
        }));
        assert_eq!(status, Some(AgentStatus::Running));
    }

    #[tokio::test]
    async fn on_event_updates_status_from_task_complete() {
        let status = agent_status_from_event(&EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "turn-1".to_string(),
            last_agent_message: Some("done".to_string()),
        }));
        let expected = AgentStatus::Completed(Some("done".to_string()));
        assert_eq!(status, Some(expected));
    }

    #[tokio::test]
    async fn on_event_updates_status_from_error() {
        let status = agent_status_from_event(&EventMsg::Error(ErrorEvent {
            message: "boom".to_string(),
            codex_error_info: None,
        }));

        let expected = AgentStatus::Errored("boom".to_string());
        assert_eq!(status, Some(expected));
    }

    #[tokio::test]
    async fn on_event_updates_status_from_turn_aborted() {
        let status = agent_status_from_event(&EventMsg::TurnAborted(TurnAbortedEvent {
            turn_id: Some("turn-1".to_string()),
            reason: TurnAbortReason::Interrupted,
        }));

        let expected = AgentStatus::Interrupted;
        assert_eq!(status, Some(expected));
    }

    #[tokio::test]
    async fn on_event_updates_status_from_shutdown_complete() {
        let status = agent_status_from_event(&EventMsg::ShutdownComplete);
        assert_eq!(status, Some(AgentStatus::Shutdown));
    }

    #[tokio::test]
    async fn spawn_agent_errors_when_manager_dropped() {
        let control = AgentControl::default();
        let (_home, config) = test_config().await;
        let err = control
            .spawn_agent(config, None)
            .await
            .expect_err("spawn_agent should fail without a manager");
        assert_eq!(
            err.to_string(),
            "unsupported operation: thread manager dropped"
        );
    }

    #[tokio::test]
    async fn resume_agent_errors_when_manager_dropped() {
        let control = AgentControl::default();
        let (_home, config) = test_config().await;
        let err = control
            .resume_agent_from_rollout(
                config,
                PathBuf::from("/tmp/missing-rollout.jsonl"),
                SessionSource::Exec,
            )
            .await
            .expect_err("resume_agent should fail without a manager");
        assert_eq!(
            err.to_string(),
            "unsupported operation: thread manager dropped"
        );
    }

    #[tokio::test]
    async fn send_input_errors_when_thread_missing() {
        let harness = AgentControlHarness::new().await;
        let thread_id = ThreadId::new();
        let err = harness
            .control
            .send_input(
                thread_id,
                vec![UserInput::Text {
                    text: "hello".to_string(),
                    text_elements: Vec::new(),
                }],
            )
            .await
            .expect_err("send_input should fail for missing thread");
        assert_matches!(err, CodexErr::ThreadNotFound(id) if id == thread_id);
    }

    #[tokio::test]
    async fn get_status_returns_not_found_for_missing_thread() {
        let harness = AgentControlHarness::new().await;
        let status = harness.control.get_status(ThreadId::new()).await;
        assert_eq!(status, AgentStatus::NotFound);
    }

    #[tokio::test]
    async fn get_status_returns_pending_init_for_new_thread() {
        let harness = AgentControlHarness::new().await;
        let (thread_id, _) = harness.start_thread().await;
        let status = harness.control.get_status(thread_id).await;
        assert_eq!(status, AgentStatus::PendingInit);
    }

    #[tokio::test]
    async fn subscribe_status_errors_for_missing_thread() {
        let harness = AgentControlHarness::new().await;
        let thread_id = ThreadId::new();
        let err = harness
            .control
            .subscribe_status(thread_id)
            .await
            .expect_err("subscribe_status should fail for missing thread");
        assert_matches!(err, CodexErr::ThreadNotFound(id) if id == thread_id);
    }

    #[tokio::test]
    async fn subscribe_status_updates_on_shutdown() {
        let harness = AgentControlHarness::new().await;
        let (thread_id, thread) = harness.start_thread().await;
        let mut status_rx = harness
            .control
            .subscribe_status(thread_id)
            .await
            .expect("subscribe_status should succeed");
        assert_eq!(status_rx.borrow().clone(), AgentStatus::PendingInit);

        let _ = thread
            .submit(Op::Shutdown {})
            .await
            .expect("shutdown should submit");

        let _ = status_rx.changed().await;
        assert_eq!(status_rx.borrow().clone(), AgentStatus::Shutdown);
    }

    #[tokio::test]
    async fn send_input_submits_user_message() {
        let harness = AgentControlHarness::new().await;
        let (thread_id, _thread) = harness.start_thread().await;

        let submission_id = harness
            .control
            .send_input(
                thread_id,
                vec![UserInput::Text {
                    text: "hello from tests".to_string(),
                    text_elements: Vec::new(),
                }],
            )
            .await
            .expect("send_input should succeed");
        assert!(!submission_id.is_empty());
        let expected = (
            thread_id,
            Op::UserInput {
                items: vec![UserInput::Text {
                    text: "hello from tests".to_string(),
                    text_elements: Vec::new(),
                }],
                origin: UserInputOrigin::AgentCall,
                final_output_json_schema: None,
            },
        );
        let captured = harness
            .manager
            .captured_ops()
            .into_iter()
            .find(|entry| *entry == expected);
        assert_eq!(captured, Some(expected));
    }

    #[tokio::test]
    async fn spawn_agent_creates_thread_without_submitting_input() {
        let harness = AgentControlHarness::new().await;
        let thread_id = harness
            .control
            .spawn_agent(harness.config.clone(), None)
            .await
            .expect("spawn_agent should succeed");
        let _thread = harness
            .manager
            .get_thread(thread_id)
            .await
            .expect("thread should be registered");
        let has_user_input = harness
            .manager
            .captured_ops()
            .into_iter()
            .any(|(id, op)| id == thread_id && matches!(op, Op::UserInput { .. }));
        assert!(!has_user_input);
    }

    #[tokio::test]
    async fn spawn_agent_respects_max_threads_limit() {
        let max_threads = 1usize;
        let (_home, config) = test_config_with_cli_overrides(vec![(
            "agents.max_threads".to_string(),
            TomlValue::Integer(max_threads as i64),
        )])
        .await;
        let manager = ThreadManager::with_models_provider_and_home_for_tests(
            CodexAuth::from_api_key("dummy"),
            config.model_provider.clone(),
            config.codex_home.clone(),
        );
        let control = manager.agent_control();

        let _ = manager
            .start_thread(config.clone())
            .await
            .expect("start thread");

        let first_agent_id = control
            .spawn_agent(config.clone(), None)
            .await
            .expect("spawn_agent should succeed");

        let err = control
            .spawn_agent(config, None)
            .await
            .expect_err("spawn_agent should respect max threads");
        let CodexErr::AgentLimitReached {
            max_threads: seen_max_threads,
        } = err
        else {
            panic!("expected CodexErr::AgentLimitReached");
        };
        assert_eq!(seen_max_threads, max_threads);

        let _ = control
            .shutdown_agent(first_agent_id)
            .await
            .expect("shutdown agent");
    }

    #[tokio::test]
    async fn spawn_agent_releases_slot_after_shutdown() {
        let max_threads = 1usize;
        let (_home, config) = test_config_with_cli_overrides(vec![(
            "agents.max_threads".to_string(),
            TomlValue::Integer(max_threads as i64),
        )])
        .await;
        let manager = ThreadManager::with_models_provider_and_home_for_tests(
            CodexAuth::from_api_key("dummy"),
            config.model_provider.clone(),
            config.codex_home.clone(),
        );
        let control = manager.agent_control();

        let first_agent_id = control
            .spawn_agent(config.clone(), None)
            .await
            .expect("spawn_agent should succeed");
        let _ = control
            .shutdown_agent(first_agent_id)
            .await
            .expect("shutdown agent");

        let second_agent_id = control
            .spawn_agent(config.clone(), None)
            .await
            .expect("spawn_agent should succeed after shutdown");
        let _ = control
            .shutdown_agent(second_agent_id)
            .await
            .expect("shutdown agent");
    }

    #[tokio::test]
    async fn spawn_agent_limit_shared_across_clones() {
        let max_threads = 1usize;
        let (_home, config) = test_config_with_cli_overrides(vec![(
            "agents.max_threads".to_string(),
            TomlValue::Integer(max_threads as i64),
        )])
        .await;
        let manager = ThreadManager::with_models_provider_and_home_for_tests(
            CodexAuth::from_api_key("dummy"),
            config.model_provider.clone(),
            config.codex_home.clone(),
        );
        let control = manager.agent_control();
        let cloned = control.clone();

        let first_agent_id = cloned
            .spawn_agent(config.clone(), None)
            .await
            .expect("spawn_agent should succeed");

        let err = control
            .spawn_agent(config, None)
            .await
            .expect_err("spawn_agent should respect shared guard");
        let CodexErr::AgentLimitReached { max_threads } = err else {
            panic!("expected CodexErr::AgentLimitReached");
        };
        assert_eq!(max_threads, 1);

        let _ = control
            .shutdown_agent(first_agent_id)
            .await
            .expect("shutdown agent");
    }

    #[tokio::test]
    async fn resume_agent_respects_max_threads_limit() {
        let max_threads = 1usize;
        let (_home, config) = test_config_with_cli_overrides(vec![(
            "agents.max_threads".to_string(),
            TomlValue::Integer(max_threads as i64),
        )])
        .await;
        let manager = ThreadManager::with_models_provider_and_home_for_tests(
            CodexAuth::from_api_key("dummy"),
            config.model_provider.clone(),
            config.codex_home.clone(),
        );
        let control = manager.agent_control();

        let resumable_id = control
            .spawn_agent(config.clone(), None)
            .await
            .expect("spawn_agent should succeed");
        let rollout_path = manager
            .get_thread(resumable_id)
            .await
            .expect("thread should exist")
            .rollout_path()
            .expect("rollout path should exist");
        let _ = control
            .shutdown_agent(resumable_id)
            .await
            .expect("shutdown resumable thread");

        let active_id = control
            .spawn_agent(config.clone(), None)
            .await
            .expect("spawn_agent should succeed for active slot");

        let err = control
            .resume_agent_from_rollout(config, rollout_path, SessionSource::Exec)
            .await
            .expect_err("resume should respect max threads");
        let CodexErr::AgentLimitReached {
            max_threads: seen_max_threads,
        } = err
        else {
            panic!("expected CodexErr::AgentLimitReached");
        };
        assert_eq!(seen_max_threads, max_threads);

        let _ = control
            .shutdown_agent(active_id)
            .await
            .expect("shutdown active thread");
    }

    #[tokio::test]
    async fn resume_agent_releases_slot_after_resume_failure() {
        let max_threads = 1usize;
        let (_home, config) = test_config_with_cli_overrides(vec![(
            "agents.max_threads".to_string(),
            TomlValue::Integer(max_threads as i64),
        )])
        .await;
        let manager = ThreadManager::with_models_provider_and_home_for_tests(
            CodexAuth::from_api_key("dummy"),
            config.model_provider.clone(),
            config.codex_home.clone(),
        );
        let control = manager.agent_control();

        let missing_rollout = config.codex_home.join("sessions/missing-rollout.jsonl");
        let _ = control
            .resume_agent_from_rollout(config.clone(), missing_rollout, SessionSource::Exec)
            .await
            .expect_err("resume should fail for missing rollout path");

        let resumed_id = control
            .spawn_agent(config, None)
            .await
            .expect("spawn should succeed after failed resume");
        let _ = control
            .shutdown_agent(resumed_id)
            .await
            .expect("shutdown resumed thread");
    }

    #[tokio::test]
    async fn set_spawned_agents_collaboration_mode_only_targets_direct_children() {
        let harness = AgentControlHarness::new().await;
        let (parent_id, _parent_thread) = harness.start_thread().await;

        let child_id = harness
            .control
            .spawn_agent(
                harness.config.clone(),
                Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id: parent_id,
                    depth: 1,
                    agent_type: None,
                    agent_name_hint: None,
                })),
            )
            .await
            .expect("spawn direct child");
        let unrelated_id = harness
            .control
            .spawn_agent(harness.config.clone(), Some(SessionSource::Exec))
            .await
            .expect("spawn unrelated thread");

        let collaboration_mode = CollaborationMode {
            mode: ModeKind::Swarm,
            settings: Settings {
                model: "gpt-5.1".to_string(),
                reasoning_effort: None,
                developer_instructions: Some("swarm-mode".to_string()),
            },
        };

        let failures = harness
            .control
            .set_spawned_agents_collaboration_mode(parent_id, collaboration_mode.clone())
            .await
            .expect("mode sync should run");
        assert!(failures.is_empty());

        let expected = (
            child_id,
            collaboration_mode_override_op(collaboration_mode.clone()),
        );
        let unrelated = (
            unrelated_id,
            collaboration_mode_override_op(collaboration_mode),
        );
        let captured = harness.manager.captured_ops();
        assert!(captured.contains(&expected));
        assert!(!captured.contains(&unrelated));

        let _ = harness
            .control
            .shutdown_agent(child_id)
            .await
            .expect("shutdown child");
        let _ = harness
            .control
            .shutdown_agent(unrelated_id)
            .await
            .expect("shutdown unrelated");
    }
}
