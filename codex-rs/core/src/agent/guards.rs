use crate::agent::UNNAMED_AGENT_NAME;
use crate::error::CodexErr;
use crate::error::Result;
use chrono::Timelike;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tracing::debug;

/// This structure is used to add some limits on the multi-agent capabilities for Codex. In
/// the current implementation, it limits:
/// * Total number of sub-agents (i.e. threads) per user session
///
/// This structure is shared by all agents in the same user session (because the `AgentControl`
/// is).
#[derive(Default)]
pub(crate) struct Guards {
    threads_set: Mutex<HashSet<ThreadId>>,
    total_count: AtomicUsize,
    agent_names: Mutex<AgentNameState>,
}

#[derive(Default)]
struct AgentNameState {
    by_thread_id: HashMap<ThreadId, String>,
    by_canonical_name: HashMap<String, ThreadId>,
    used_canonical_names: HashSet<String>,
    work_entries_by_thread_id: HashMap<ThreadId, VecDeque<AgentWorkEntry>>,
    pending_primary_completion_notices_by_thread_id: HashMap<ThreadId, String>,
}

const MAX_WORK_SUMMARIES_PER_AGENT: usize = 40;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentWorkEntry {
    pub(crate) recorded_at: i64,
    pub(crate) summary: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentWorkStatus {
    pub(crate) thread_id: ThreadId,
    pub(crate) agent_name: String,
    pub(crate) entries: Vec<AgentWorkEntry>,
}

/// Initial agent is depth 0.
/// Allow only one spawn level (depth=1), so only the primary agent can spawn.
pub(crate) const MAX_THREAD_SPAWN_DEPTH: i32 = 1;

fn canonical_agent_name(name: &str) -> String {
    name.trim().to_ascii_lowercase()
}

fn session_depth(session_source: &SessionSource) -> i32 {
    match session_source {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn { depth, .. }) => *depth,
        SessionSource::SubAgent(_) => 0,
        _ => 0,
    }
}

pub(crate) fn next_thread_spawn_depth(session_source: &SessionSource) -> i32 {
    session_depth(session_source).saturating_add(1)
}

pub(crate) fn exceeds_thread_spawn_depth_limit(depth: i32) -> bool {
    depth > MAX_THREAD_SPAWN_DEPTH
}

impl Guards {
    pub(crate) fn reserve_agent_name(&self, name: &str) -> std::result::Result<(), String> {
        let display_name = name.trim();
        if display_name.is_empty() {
            return Err("agent name must be non-empty".to_string());
        }

        let canonical_name = canonical_agent_name(display_name);
        let mut state = self
            .agent_names
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.used_canonical_names.contains(&canonical_name) {
            return Err(format!("agent name `{display_name}` already exists"));
        }
        state.used_canonical_names.insert(canonical_name);
        Ok(())
    }

    pub(crate) fn register_agent_name(
        &self,
        thread_id: ThreadId,
        name: &str,
    ) -> std::result::Result<(), String> {
        let display_name = name.trim();
        if display_name.is_empty() {
            return Err("agent name must be non-empty".to_string());
        }

        let canonical_name = canonical_agent_name(display_name);
        let mut state = self
            .agent_names
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        if let Some(existing_thread_id) = state.by_canonical_name.get(&canonical_name)
            && *existing_thread_id != thread_id
        {
            return Err(format!("agent name `{display_name}` already exists"));
        }

        if let Some(existing_name) = state.by_thread_id.get(&thread_id) {
            let existing_canonical_name = canonical_agent_name(existing_name);
            if existing_canonical_name != canonical_name {
                state.by_canonical_name.remove(&existing_canonical_name);
            }
        }

        state
            .by_thread_id
            .insert(thread_id, display_name.to_string());
        state
            .by_canonical_name
            .insert(canonical_name.clone(), thread_id);
        state.used_canonical_names.insert(canonical_name);
        Ok(())
    }

    pub(crate) fn release_agent_name_reservation(&self, name: &str) {
        let canonical_name = canonical_agent_name(name);
        let mut state = self
            .agent_names
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.by_canonical_name.contains_key(&canonical_name) {
            return;
        }
        state.used_canonical_names.remove(&canonical_name);
    }

    pub(crate) fn find_thread_by_name(&self, name: &str) -> Option<ThreadId> {
        let canonical_name = canonical_agent_name(name);
        let state = self
            .agent_names
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.by_canonical_name.get(&canonical_name).copied()
    }

    pub(crate) fn find_name_by_thread(&self, thread_id: ThreadId) -> Option<String> {
        let state = self
            .agent_names
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.by_thread_id.get(&thread_id).cloned()
    }

    pub(crate) fn is_agent_name_used(&self, name: &str) -> bool {
        let canonical_name = canonical_agent_name(name);
        let state = self
            .agent_names
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.used_canonical_names.contains(&canonical_name)
    }

    pub(crate) fn known_agent_names(&self) -> Vec<String> {
        let state = self
            .agent_names
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut names = state.by_thread_id.values().cloned().collect::<Vec<_>>();
        names.sort_unstable();
        names
    }

    pub(crate) fn record_agent_work_summary(&self, thread_id: ThreadId, summary: String) {
        let mut state = self
            .agent_names
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let recorded_at = chrono::Utc::now().timestamp();
        let agent_name = state
            .by_thread_id
            .get(&thread_id)
            .cloned()
            .unwrap_or_else(|| UNNAMED_AGENT_NAME.to_string());
        let debug_line = format_work_summary_debug_line(recorded_at, &agent_name, &summary);
        let entries = state
            .work_entries_by_thread_id
            .entry(thread_id)
            .or_default();
        entries.push_back(AgentWorkEntry {
            recorded_at,
            summary,
        });
        while entries.len() > MAX_WORK_SUMMARIES_PER_AGENT {
            entries.pop_front();
        }
        debug!("agent_tool_summary {debug_line}");
    }

    pub(crate) fn agent_work_status(&self, thread_id: ThreadId) -> Option<AgentWorkStatus> {
        let state = self
            .agent_names
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entries = state.work_entries_by_thread_id.get(&thread_id)?;
        if entries.is_empty() {
            return None;
        }

        Some(AgentWorkStatus {
            thread_id,
            agent_name: state
                .by_thread_id
                .get(&thread_id)
                .cloned()
                .unwrap_or_else(|| UNNAMED_AGENT_NAME.to_string()),
            entries: entries.iter().cloned().collect(),
        })
    }

    pub(crate) fn other_agents_work_status(
        &self,
        current_thread_id: ThreadId,
    ) -> Vec<AgentWorkStatus> {
        let state = self
            .agent_names
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let mut statuses = state
            .work_entries_by_thread_id
            .iter()
            .filter(|(thread_id, entries)| **thread_id != current_thread_id && !entries.is_empty())
            .map(|(thread_id, entries)| AgentWorkStatus {
                thread_id: *thread_id,
                agent_name: state
                    .by_thread_id
                    .get(thread_id)
                    .cloned()
                    .unwrap_or_else(|| UNNAMED_AGENT_NAME.to_string()),
                entries: entries.iter().cloned().collect(),
            })
            .collect::<Vec<_>>();

        statuses.sort_unstable_by(|left, right| {
            let left_name = left.agent_name.to_ascii_lowercase();
            let right_name = right.agent_name.to_ascii_lowercase();
            left_name
                .cmp(&right_name)
                .then(left.agent_name.cmp(&right.agent_name))
        });

        statuses
    }

    pub(crate) fn queue_primary_completion_notice(&self, thread_id: ThreadId, notice: String) {
        if notice.trim().is_empty() {
            return;
        }
        let mut state = self
            .agent_names
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .pending_primary_completion_notices_by_thread_id
            .insert(thread_id, notice);
    }

    pub(crate) fn take_primary_completion_notice(&self, thread_id: ThreadId) -> Option<String> {
        let mut state = self
            .agent_names
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .pending_primary_completion_notices_by_thread_id
            .remove(&thread_id)
    }

    pub(crate) fn clear_primary_completion_notice(&self, thread_id: ThreadId) {
        let mut state = self
            .agent_names
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .pending_primary_completion_notices_by_thread_id
            .remove(&thread_id);
    }

    pub(crate) fn reserve_spawn_slot(
        self: &Arc<Self>,
        max_threads: Option<usize>,
    ) -> Result<SpawnReservation> {
        if let Some(max_threads) = max_threads {
            if !self.try_increment_spawned(max_threads) {
                return Err(CodexErr::AgentLimitReached { max_threads });
            }
        } else {
            self.total_count.fetch_add(1, Ordering::AcqRel);
        }
        Ok(SpawnReservation {
            state: Arc::clone(self),
            active: true,
        })
    }

    pub(crate) fn release_spawned_thread(&self, thread_id: ThreadId) {
        let removed = {
            let mut threads = self
                .threads_set
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            threads.remove(&thread_id)
        };
        if removed {
            self.total_count.fetch_sub(1, Ordering::AcqRel);
            self.remove_thread_state(thread_id);
        }
    }

    fn register_spawned_thread(&self, thread_id: ThreadId) {
        let mut threads = self
            .threads_set
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        threads.insert(thread_id);
    }

    fn try_increment_spawned(&self, max_threads: usize) -> bool {
        let mut current = self.total_count.load(Ordering::Acquire);
        loop {
            if current >= max_threads {
                return false;
            }
            match self.total_count.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(updated) => current = updated,
            }
        }
    }

    fn remove_thread_state(&self, thread_id: ThreadId) {
        let mut state = self
            .agent_names
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(existing_name) = state.by_thread_id.remove(&thread_id) {
            let existing_canonical_name = canonical_agent_name(&existing_name);
            if state.by_canonical_name.get(&existing_canonical_name) == Some(&thread_id) {
                state.by_canonical_name.remove(&existing_canonical_name);
            }
        }
        state.work_entries_by_thread_id.remove(&thread_id);
        state
            .pending_primary_completion_notices_by_thread_id
            .remove(&thread_id);
    }
}

fn format_work_entry_time(timestamp: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(timestamp, 0)
        .map(|dt| format!("{}:{:02}", dt.hour(), dt.minute()))
        .unwrap_or_else(|| timestamp.to_string())
}

fn format_work_summary_debug_line(timestamp: i64, agent_name: &str, summary: &str) -> String {
    let collapsed_summary = summary.split_whitespace().collect::<Vec<_>>().join(" ");
    format!(
        "{} {agent_name} {collapsed_summary}",
        format_work_entry_time(timestamp)
    )
}

pub(crate) struct SpawnReservation {
    state: Arc<Guards>,
    active: bool,
}

impl SpawnReservation {
    pub(crate) fn commit(mut self, thread_id: ThreadId) {
        self.state.register_spawned_thread(thread_id);
        self.active = false;
    }
}

impl Drop for SpawnReservation {
    fn drop(&mut self) {
        if self.active {
            self.state.total_count.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn session_depth_defaults_to_zero_for_root_sources() {
        assert_eq!(session_depth(&SessionSource::Cli), 0);
    }

    #[test]
    fn thread_spawn_depth_increments_and_enforces_limit() {
        let session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: ThreadId::new(),
            depth: 1,
            agent_type: None,
            agent_name_hint: None,
        });
        let child_depth = next_thread_spawn_depth(&session_source);
        assert_eq!(child_depth, 2);
        assert!(exceeds_thread_spawn_depth_limit(child_depth));
    }

    #[test]
    fn non_thread_spawn_subagents_default_to_depth_zero() {
        let session_source = SessionSource::SubAgent(SubAgentSource::Review);
        assert_eq!(session_depth(&session_source), 0);
        assert_eq!(next_thread_spawn_depth(&session_source), 1);
        assert!(!exceeds_thread_spawn_depth_limit(1));
    }

    #[test]
    fn reservation_drop_releases_slot() {
        let guards = Arc::new(Guards::default());
        let reservation = guards.reserve_spawn_slot(Some(1)).expect("reserve slot");
        drop(reservation);

        let reservation = guards.reserve_spawn_slot(Some(1)).expect("slot released");
        drop(reservation);
    }

    #[test]
    fn commit_holds_slot_until_release() {
        let guards = Arc::new(Guards::default());
        let reservation = guards.reserve_spawn_slot(Some(1)).expect("reserve slot");
        let thread_id = ThreadId::new();
        reservation.commit(thread_id);

        let err = match guards.reserve_spawn_slot(Some(1)) {
            Ok(_) => panic!("limit should be enforced"),
            Err(err) => err,
        };
        let CodexErr::AgentLimitReached { max_threads } = err else {
            panic!("expected CodexErr::AgentLimitReached");
        };
        assert_eq!(max_threads, 1);

        guards.release_spawned_thread(thread_id);
        let reservation = guards
            .reserve_spawn_slot(Some(1))
            .expect("slot released after thread removal");
        drop(reservation);
    }

    #[test]
    fn release_ignores_unknown_thread_id() {
        let guards = Arc::new(Guards::default());
        let reservation = guards.reserve_spawn_slot(Some(1)).expect("reserve slot");
        let thread_id = ThreadId::new();
        reservation.commit(thread_id);

        guards.release_spawned_thread(ThreadId::new());

        let err = match guards.reserve_spawn_slot(Some(1)) {
            Ok(_) => panic!("limit should still be enforced"),
            Err(err) => err,
        };
        let CodexErr::AgentLimitReached { max_threads } = err else {
            panic!("expected CodexErr::AgentLimitReached");
        };
        assert_eq!(max_threads, 1);

        guards.release_spawned_thread(thread_id);
        let reservation = guards
            .reserve_spawn_slot(Some(1))
            .expect("slot released after real thread removal");
        drop(reservation);
    }

    #[test]
    fn release_is_idempotent_for_registered_threads() {
        let guards = Arc::new(Guards::default());
        let reservation = guards.reserve_spawn_slot(Some(1)).expect("reserve slot");
        let first_id = ThreadId::new();
        reservation.commit(first_id);

        guards.release_spawned_thread(first_id);

        let reservation = guards.reserve_spawn_slot(Some(1)).expect("slot reused");
        let second_id = ThreadId::new();
        reservation.commit(second_id);

        guards.release_spawned_thread(first_id);

        let err = match guards.reserve_spawn_slot(Some(1)) {
            Ok(_) => panic!("limit should still be enforced"),
            Err(err) => err,
        };
        let CodexErr::AgentLimitReached { max_threads } = err else {
            panic!("expected CodexErr::AgentLimitReached");
        };
        assert_eq!(max_threads, 1);

        guards.release_spawned_thread(second_id);
        let reservation = guards
            .reserve_spawn_slot(Some(1))
            .expect("slot released after second thread removal");
        drop(reservation);
    }

    #[test]
    fn register_agent_name_tracks_name_and_thread_lookup() {
        let guards = Arc::new(Guards::default());
        let thread_id = ThreadId::new();

        guards
            .register_agent_name(thread_id, "Alice-worker")
            .expect("name should register");

        assert_eq!(guards.find_thread_by_name("alice-worker"), Some(thread_id));
        assert_eq!(
            guards.find_name_by_thread(thread_id),
            Some("Alice-worker".to_string())
        );
        assert!(guards.is_agent_name_used("ALICE-worker"));
    }

    #[test]
    fn register_agent_name_rejects_duplicate_for_different_thread() {
        let guards = Arc::new(Guards::default());
        let first = ThreadId::new();
        let second = ThreadId::new();

        guards
            .register_agent_name(first, "Alice-worker")
            .expect("first name should register");

        let err = guards
            .register_agent_name(second, "alice-worker")
            .expect_err("duplicate name should fail");
        assert_eq!(err, "agent name `alice-worker` already exists");
    }

    #[test]
    fn reserve_agent_name_blocks_reuse_but_allows_later_thread_binding() {
        let guards = Arc::new(Guards::default());
        let thread_id = ThreadId::new();

        guards
            .reserve_agent_name("Alice-worker")
            .expect("reservation should succeed");
        assert!(guards.is_agent_name_used("alice-worker"));

        let err = guards
            .reserve_agent_name("alice-worker")
            .expect_err("second reservation should fail");
        assert_eq!(err, "agent name `alice-worker` already exists");

        guards
            .register_agent_name(thread_id, "Alice-worker")
            .expect("reserved name should bind to thread later");
        assert_eq!(guards.find_thread_by_name("alice-worker"), Some(thread_id));
    }

    #[test]
    fn release_agent_name_reservation_frees_unbound_name_only() {
        let guards = Arc::new(Guards::default());
        let thread_id = ThreadId::new();

        guards
            .reserve_agent_name("Alice-worker")
            .expect("reservation should succeed");
        guards.release_agent_name_reservation("alice-worker");
        assert!(!guards.is_agent_name_used("Alice-worker"));

        guards
            .reserve_agent_name("Bob-worker")
            .expect("reservation should succeed");
        guards
            .register_agent_name(thread_id, "Bob-worker")
            .expect("reserved name should bind");
        guards.release_agent_name_reservation("bob-worker");
        assert_eq!(guards.find_thread_by_name("bob-worker"), Some(thread_id));
    }

    #[test]
    fn workboard_keeps_only_forty_latest_entries_per_agent() {
        let guards = Arc::new(Guards::default());
        let thread_id = ThreadId::new();
        guards
            .register_agent_name(thread_id, "Alice-worker")
            .expect("name should register");

        for idx in 1..=41 {
            guards.record_agent_work_summary(thread_id, format!("entry-{idx}"));
        }

        let statuses = guards.other_agents_work_status(ThreadId::new());
        assert_eq!(statuses.len(), 1);
        let entries = &statuses[0].entries;
        assert_eq!(entries.len(), 40);
        assert_eq!(entries[0].summary, "entry-2");
        assert_eq!(entries[39].summary, "entry-41");
    }

    #[test]
    fn agent_work_status_returns_entries_for_requested_thread() {
        let guards = Arc::new(Guards::default());
        let thread_id = ThreadId::new();
        guards
            .register_agent_name(thread_id, "Alice-worker")
            .expect("name should register");
        guards.record_agent_work_summary(thread_id, "entry-1".to_string());
        guards.record_agent_work_summary(thread_id, "entry-2".to_string());

        let status = guards
            .agent_work_status(thread_id)
            .expect("status should exist");
        let summaries = status
            .entries
            .iter()
            .map(|entry| entry.summary.as_str())
            .collect::<Vec<_>>();

        assert_eq!(status.agent_name, "Alice-worker");
        assert_eq!(summaries, vec!["entry-1", "entry-2"]);
    }

    #[test]
    fn workboard_excludes_current_thread_and_sorts_by_agent_name() {
        let guards = Arc::new(Guards::default());
        let current = ThreadId::new();
        let alpha = ThreadId::new();
        let beta = ThreadId::new();
        guards
            .register_agent_name(current, "wmj-assistant")
            .expect("name should register");
        guards
            .register_agent_name(alpha, "Alpha-worker")
            .expect("name should register");
        guards
            .register_agent_name(beta, "beta-worker")
            .expect("name should register");

        guards.record_agent_work_summary(current, "current".to_string());
        guards.record_agent_work_summary(beta, "beta".to_string());
        guards.record_agent_work_summary(alpha, "alpha".to_string());

        let statuses = guards.other_agents_work_status(current);
        let names = statuses
            .iter()
            .map(|status| status.agent_name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["Alpha-worker", "beta-worker"]);
    }

    #[test]
    fn release_spawned_thread_cleans_name_and_workboard_state() {
        let guards = Arc::new(Guards::default());
        let thread_id = ThreadId::new();
        let reservation = guards.reserve_spawn_slot(Some(1)).expect("reserve slot");
        reservation.commit(thread_id);
        guards
            .register_agent_name(thread_id, "Alice-worker")
            .expect("name should register");
        guards.record_agent_work_summary(thread_id, "search".to_string());

        guards.release_spawned_thread(thread_id);

        assert_eq!(guards.find_name_by_thread(thread_id), None);
        assert!(guards.other_agents_work_status(ThreadId::new()).is_empty());
    }

    #[test]
    fn format_work_summary_debug_line_matches_expected_debug_format() {
        let timestamp = chrono::DateTime::parse_from_rfc3339("2026-02-22T08:41:00Z")
            .expect("parse timestamp")
            .timestamp();

        assert_eq!(
            format_work_summary_debug_line(timestamp, "wmj_assistant", "task_summary"),
            "8:41 wmj_assistant task_summary"
        );
    }

    #[test]
    fn primary_completion_notice_is_one_time_per_thread() {
        let guards = Arc::new(Guards::default());
        let thread_id = ThreadId::new();

        guards.queue_primary_completion_notice(thread_id, "stop-now".to_string());

        let first = guards.take_primary_completion_notice(thread_id);
        assert_eq!(first.as_deref(), Some("stop-now"));
        assert!(guards.take_primary_completion_notice(thread_id).is_none());
    }

    #[test]
    fn clear_primary_completion_notice_removes_pending_message() {
        let guards = Arc::new(Guards::default());
        let thread_id = ThreadId::new();

        guards.queue_primary_completion_notice(thread_id, "stop".to_string());
        guards.clear_primary_completion_notice(thread_id);

        assert!(guards.take_primary_completion_notice(thread_id).is_none());
    }
}
