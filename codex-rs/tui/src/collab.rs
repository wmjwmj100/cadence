use crate::history_cell::PlainHistoryCell;
use crate::render::line_utils::prefix_lines;
use crate::text_formatting::truncate_text;
use codex_core::protocol::AgentStatus;
use codex_core::protocol::CollabAgentInteractionEndEvent;
use codex_core::protocol::CollabAgentSpawnEndEvent;
use codex_core::protocol::CollabCloseEndEvent;
use codex_core::protocol::CollabResumeBeginEvent;
use codex_core::protocol::CollabResumeEndEvent;
use codex_core::protocol::CollabWaitLifecycleState;
use codex_core::protocol::CollabWaitTargetEvent;
use codex_core::protocol::CollabWaitingBeginEvent;
use codex_core::protocol::CollabWaitingEndEvent;
use codex_protocol::ThreadId;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;

const COLLAB_PROMPT_PREVIEW_GRAPHEMES: usize = 160;
const COLLAB_CALLBACK_PREVIEW_GRAPHEMES: usize = 240;

pub(crate) fn spawn_end(ev: CollabAgentSpawnEndEvent) -> PlainHistoryCell {
    let CollabAgentSpawnEndEvent {
        call_id,
        sender_thread_id: _,
        new_thread_id,
        prompt,
        status,
    } = ev;
    let new_agent = new_thread_id
        .map(|id| Span::from(id.to_string()))
        .unwrap_or_else(|| Span::from("not created").dim());
    let mut details = vec![
        detail_line("call", call_id),
        detail_line("agent", new_agent),
        status_line(&status),
    ];
    if let Some(line) = prompt_line(&prompt) {
        details.push(line);
    }
    collab_event("Agent spawned", details)
}

pub(crate) fn interaction_end(ev: CollabAgentInteractionEndEvent) -> PlainHistoryCell {
    let CollabAgentInteractionEndEvent {
        call_id,
        sender_thread_id: _,
        receiver_thread_id,
        prompt,
        status,
    } = ev;
    let mut details = vec![
        detail_line("call", call_id),
        detail_line("receiver", receiver_thread_id.to_string()),
        status_line(&status),
    ];
    if let Some(line) = prompt_line(&prompt) {
        details.push(line);
    }
    collab_event("Input sent", details)
}

pub(crate) fn waiting_begin(ev: CollabWaitingBeginEvent) -> PlainHistoryCell {
    let CollabWaitingBeginEvent {
        call_id,
        sender_thread_id,
        sender_agent_name,
        targets,
    } = ev;
    let details = vec![
        detail_line(
            "sender",
            wait_actor_label(&sender_agent_name, sender_thread_id),
        ),
        detail_line("call", call_id),
        detail_line("receivers", format_wait_receivers(&targets)),
    ];
    collab_event("Waiting for agents", details)
}

pub(crate) fn waiting_end(ev: CollabWaitingEndEvent) -> PlainHistoryCell {
    let CollabWaitingEndEvent {
        call_id,
        sender_thread_id,
        sender_agent_name,
        timed_out,
        targets,
    } = ev;
    let mut details = vec![
        detail_line(
            "sender",
            wait_actor_label(&sender_agent_name, sender_thread_id),
        ),
        detail_line("call", call_id),
        detail_line(
            "result",
            if timed_out {
                Span::from("timed out").yellow()
            } else {
                Span::from("completed").green()
            },
        ),
    ];
    details.extend(wait_complete_lines(&targets));
    collab_event("Wait complete", details)
}

pub(crate) fn close_end(ev: CollabCloseEndEvent) -> PlainHistoryCell {
    let CollabCloseEndEvent {
        call_id,
        sender_thread_id: _,
        receiver_thread_id,
        status,
    } = ev;
    let details = vec![
        detail_line("call", call_id),
        detail_line("receiver", receiver_thread_id.to_string()),
        status_line(&status),
    ];
    collab_event("Agent closed", details)
}

pub(crate) fn resume_begin(ev: CollabResumeBeginEvent) -> PlainHistoryCell {
    let CollabResumeBeginEvent {
        call_id,
        sender_thread_id: _,
        receiver_thread_id,
    } = ev;
    let details = vec![
        detail_line("call", call_id),
        detail_line("receiver", receiver_thread_id.to_string()),
    ];
    collab_event("Resuming agent", details)
}

pub(crate) fn resume_end(ev: CollabResumeEndEvent) -> PlainHistoryCell {
    let CollabResumeEndEvent {
        call_id,
        sender_thread_id: _,
        receiver_thread_id,
        status,
    } = ev;
    let details = vec![
        detail_line("call", call_id),
        detail_line("receiver", receiver_thread_id.to_string()),
        status_line(&status),
    ];
    collab_event("Agent resumed", details)
}

fn collab_event(title: impl Into<String>, details: Vec<Line<'static>>) -> PlainHistoryCell {
    let title = title.into();
    let mut lines: Vec<Line<'static>> =
        vec![vec![Span::from("\u{2022} ").dim(), Span::from(title).bold()].into()];
    if !details.is_empty() {
        lines.extend(prefix_lines(details, "  \u{2514} ".dim(), "    ".into()));
    }
    PlainHistoryCell::new(lines)
}

fn detail_line(label: &str, value: impl Into<Span<'static>>) -> Line<'static> {
    vec![Span::from(format!("{label}: ")).dim(), value.into()].into()
}

fn status_line(status: &AgentStatus) -> Line<'static> {
    detail_line("status", status_span(status))
}

fn status_span(status: &AgentStatus) -> Span<'static> {
    match status {
        AgentStatus::PendingInit => Span::from("pending init").dim(),
        AgentStatus::Running => Span::from("running").cyan().bold(),
        AgentStatus::Interrupted => Span::from("interrupted").yellow(),
        AgentStatus::Completed(_) => Span::from("completed").green(),
        AgentStatus::Errored(_) => Span::from("errored").red(),
        AgentStatus::Shutdown => Span::from("shutdown").dim(),
        AgentStatus::NotFound => Span::from("not found").red(),
    }
}

fn prompt_line(prompt: &str) -> Option<Line<'static>> {
    let trimmed = prompt.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(detail_line(
            "prompt",
            Span::from(truncate_text(trimmed, COLLAB_PROMPT_PREVIEW_GRAPHEMES)).dim(),
        ))
    }
}

fn wait_actor_label(agent_name: &str, thread_id: ThreadId) -> Span<'static> {
    let trimmed = agent_name.trim();
    if trimmed.is_empty() {
        Span::from(thread_id.to_string()).dim()
    } else {
        Span::from(trimmed.to_string())
    }
}

fn format_wait_receivers(targets: &[CollabWaitTargetEvent]) -> Span<'static> {
    if targets.is_empty() {
        return Span::from("none").dim();
    }

    let joined = targets
        .iter()
        .map(|target| {
            if target.receiver_agent_name.trim().is_empty() {
                target.receiver_thread_id.to_string()
            } else {
                target.receiver_agent_name.trim().to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    Span::from(joined)
}

fn wait_complete_lines(targets: &[CollabWaitTargetEvent]) -> Vec<Line<'static>> {
    if targets.is_empty() {
        return vec![detail_line("targets", Span::from("none").dim())];
    }

    let mut pending = 0usize;
    let mut running = 0usize;
    let mut completed = 0usize;
    let mut timed_out = 0usize;
    let mut failed = 0usize;

    for target in targets {
        match &target.state {
            CollabWaitLifecycleState::Pending => pending += 1,
            CollabWaitLifecycleState::Running => running += 1,
            CollabWaitLifecycleState::Completed => completed += 1,
            CollabWaitLifecycleState::TimedOut => timed_out += 1,
            CollabWaitLifecycleState::Failed => failed += 1,
        }
    }

    let mut summary = vec![Span::from(format!("{} total", targets.len())).dim()];
    push_wait_state_count(
        &mut summary,
        pending,
        "pending",
        ratatui::prelude::Stylize::dim,
    );
    push_wait_state_count(&mut summary, running, "running", |span| span.cyan().bold());
    push_wait_state_count(
        &mut summary,
        completed,
        "completed",
        ratatui::prelude::Stylize::green,
    );
    push_wait_state_count(
        &mut summary,
        timed_out,
        "timed out",
        ratatui::prelude::Stylize::yellow,
    );
    push_wait_state_count(
        &mut summary,
        failed,
        "failed",
        ratatui::prelude::Stylize::red,
    );

    let mut entries: Vec<&CollabWaitTargetEvent> = targets.iter().collect();
    entries.sort_by(|left, right| {
        (
            left.receiver_agent_name.trim().to_ascii_lowercase(),
            left.receiver_thread_id.to_string(),
            left.message_id.as_str(),
        )
            .cmp(&(
                right.receiver_agent_name.trim().to_ascii_lowercase(),
                right.receiver_thread_id.to_string(),
                right.message_id.as_str(),
            ))
    });

    let mut lines = Vec::with_capacity(entries.len() + 1);
    lines.push(detail_line_spans("targets", summary));
    lines.extend(entries.into_iter().map(|target| {
        let mut spans = vec![
            wait_actor_label(&target.receiver_agent_name, target.receiver_thread_id),
            Span::from(" [").dim(),
            Span::from(target.message_id.clone()).dim(),
            Span::from("] ").dim(),
            wait_state_span(&target.state),
        ];

        if let Some(callback_content) = target
            .callback_content
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            && matches!(&target.state, CollabWaitLifecycleState::Completed)
        {
            let preview = truncate_text(
                &callback_content
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" "),
                COLLAB_CALLBACK_PREVIEW_GRAPHEMES,
            );
            spans.push(Span::from(": ").dim());
            spans.push(Span::from(preview));
        }

        spans.into()
    }));
    lines
}

fn wait_state_span(state: &CollabWaitLifecycleState) -> Span<'static> {
    match state {
        CollabWaitLifecycleState::Pending => Span::from("pending").dim(),
        CollabWaitLifecycleState::Running => Span::from("running").cyan().bold(),
        CollabWaitLifecycleState::Completed => Span::from("completed").green(),
        CollabWaitLifecycleState::TimedOut => Span::from("timed out").yellow(),
        CollabWaitLifecycleState::Failed => Span::from("failed").red(),
    }
}

fn push_wait_state_count(
    spans: &mut Vec<Span<'static>>,
    count: usize,
    label: &'static str,
    style: impl FnOnce(Span<'static>) -> Span<'static>,
) {
    if count == 0 {
        return;
    }

    spans.push(Span::from(" \u{00B7} ").dim());
    spans.push(style(Span::from(format!("{count} {label}"))));
}

fn detail_line_spans(label: &str, mut value: Vec<Span<'static>>) -> Line<'static> {
    let mut spans = Vec::with_capacity(value.len() + 1);
    spans.push(Span::from(format!("{label}: ")).dim());
    spans.append(&mut value);
    spans.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::history_cell::HistoryCell;
    use insta::assert_snapshot;

    fn render(cell: &dyn HistoryCell) -> String {
        let lines = cell.display_lines(u16::MAX);
        lines
            .into_iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn thread_id(raw: &str) -> ThreadId {
        ThreadId::from_string(raw).expect("valid thread id")
    }

    #[test]
    fn collab_interaction_end_includes_sender_and_receiver() {
        let sender = thread_id("00000000-0000-0000-0000-000000000001");
        let receiver = thread_id("00000000-0000-0000-0000-000000000002");
        let cell = interaction_end(CollabAgentInteractionEndEvent {
            call_id: "call-1".to_string(),
            sender_thread_id: sender,
            receiver_thread_id: receiver,
            prompt: "do the thing".to_string(),
            status: AgentStatus::Running,
        });
        assert_snapshot!(render(&cell));
    }

    #[test]
    fn collab_waiting_begin_includes_sender_and_receivers() {
        let sender = thread_id("00000000-0000-0000-0000-000000000001");
        let receiver_a = thread_id("00000000-0000-0000-0000-000000000002");
        let receiver_b = thread_id("00000000-0000-0000-0000-000000000003");
        let cell = waiting_begin(CollabWaitingBeginEvent {
            call_id: "call-2".to_string(),
            sender_thread_id: sender,
            sender_agent_name: "wmj-assistant".to_string(),
            targets: vec![
                CollabWaitTargetEvent {
                    receiver_thread_id: receiver_a,
                    receiver_agent_name: "Alice-worker".to_string(),
                    message_id: "m-1".to_string(),
                    state: CollabWaitLifecycleState::Running,
                    callback_content: None,
                },
                CollabWaitTargetEvent {
                    receiver_thread_id: receiver_b,
                    receiver_agent_name: String::new(),
                    message_id: "m-2".to_string(),
                    state: CollabWaitLifecycleState::Running,
                    callback_content: None,
                },
            ],
        });
        assert_snapshot!(render(&cell));
    }

    #[test]
    fn collab_waiting_end_renders_callback_payload_and_lifecycle_states() {
        let sender = thread_id("00000000-0000-0000-0000-000000000001");
        let receiver_a = thread_id("00000000-0000-0000-0000-000000000002");
        let receiver_b = thread_id("00000000-0000-0000-0000-000000000003");
        let receiver_c = thread_id("00000000-0000-0000-0000-000000000004");
        let receiver_d = thread_id("00000000-0000-0000-0000-000000000005");
        let receiver_e = thread_id("00000000-0000-0000-0000-000000000006");
        let cell = waiting_end(CollabWaitingEndEvent {
            call_id: "call-3".to_string(),
            sender_thread_id: sender,
            sender_agent_name: "wmj-assistant".to_string(),
            timed_out: true,
            targets: vec![
                CollabWaitTargetEvent {
                    receiver_thread_id: receiver_a,
                    receiver_agent_name: "Alice-worker".to_string(),
                    message_id: "m-1".to_string(),
                    state: CollabWaitLifecycleState::Completed,
                    callback_content: Some("callback says task is done".to_string()),
                },
                CollabWaitTargetEvent {
                    receiver_thread_id: receiver_b,
                    receiver_agent_name: "Bob-worker".to_string(),
                    message_id: "m-2".to_string(),
                    state: CollabWaitLifecycleState::Running,
                    callback_content: None,
                },
                CollabWaitTargetEvent {
                    receiver_thread_id: receiver_c,
                    receiver_agent_name: "Cara-worker".to_string(),
                    message_id: "m-3".to_string(),
                    state: CollabWaitLifecycleState::TimedOut,
                    callback_content: None,
                },
                CollabWaitTargetEvent {
                    receiver_thread_id: receiver_d,
                    receiver_agent_name: "Dana-worker".to_string(),
                    message_id: "m-4".to_string(),
                    state: CollabWaitLifecycleState::Failed,
                    callback_content: None,
                },
                CollabWaitTargetEvent {
                    receiver_thread_id: receiver_e,
                    receiver_agent_name: "Eve-worker".to_string(),
                    message_id: "m-5".to_string(),
                    state: CollabWaitLifecycleState::Pending,
                    callback_content: None,
                },
            ],
        });
        assert_snapshot!(render(&cell));
    }
}
