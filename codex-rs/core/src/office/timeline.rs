use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OfficeTimelineEvent {
    AgentStep {
        agent_id: String,
        summary: String,
    },
    Message {
        message_id: String,
        from: String,
        to: String,
    },
    ToolIntent {
        agent_id: String,
        tool_name: String,
    },
    ToolResult {
        agent_id: String,
        tool_name: String,
        exit_code: i32,
    },
    HumanInput {
        human_id: String,
        message_id: String,
    },
    Summary {
        step_index: usize,
        text: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OfficeSummary {
    pub step_index: usize,
    pub text: String,
    pub source_event_count: usize,
    pub source_event_start: usize,
    pub source_event_end: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StepSummaryClock {
    threshold: usize,
    steps_since_summary: usize,
    total_steps: usize,
}

impl StepSummaryClock {
    pub fn new(threshold: usize) -> Self {
        Self {
            threshold,
            steps_since_summary: 0,
            total_steps: 0,
        }
    }

    pub fn record_step(&mut self) -> bool {
        self.total_steps += 1;
        self.steps_since_summary += 1;
        if self.steps_since_summary >= self.threshold {
            self.steps_since_summary = 0;
            true
        } else {
            false
        }
    }

    pub fn total_steps(&self) -> usize {
        self.total_steps
    }
}

impl Default for StepSummaryClock {
    fn default() -> Self {
        Self::new(15)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OfficeTimeline {
    clock: StepSummaryClock,
    events: Vec<OfficeTimelineEvent>,
    summaries: Vec<OfficeSummary>,
    last_summary_event_end: usize,
}

impl OfficeTimeline {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_agent_step(
        &mut self,
        agent_id: impl Into<String>,
        summary: impl Into<String>,
    ) -> Option<OfficeSummary> {
        self.events.push(OfficeTimelineEvent::AgentStep {
            agent_id: agent_id.into(),
            summary: summary.into(),
        });
        if self.clock.record_step() {
            let source_event_start = self.last_summary_event_end;
            let source_event_end = self.events.len();
            let summary = OfficeSummary {
                step_index: self.clock.total_steps(),
                text: format!(
                    "Summary after {} global Agent steps",
                    self.clock.total_steps()
                ),
                source_event_count: source_event_end.saturating_sub(source_event_start),
                source_event_start,
                source_event_end,
            };
            self.last_summary_event_end = source_event_end;
            self.events.push(OfficeTimelineEvent::Summary {
                step_index: summary.step_index,
                text: summary.text.clone(),
            });
            self.summaries.push(summary.clone());
            Some(summary)
        } else {
            None
        }
    }

    pub fn append_event(&mut self, event: OfficeTimelineEvent) {
        self.events.push(event);
    }

    pub fn events(&self) -> &[OfficeTimelineEvent] {
        &self.events
    }

    pub fn summaries(&self) -> &[OfficeSummary] {
        &self.summaries
    }

    pub fn total_steps(&self) -> usize {
        self.clock.total_steps()
    }
}
