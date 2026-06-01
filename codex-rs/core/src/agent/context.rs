use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;

use chrono::DateTime;
use chrono::SecondsFormat;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

const AGENTS_DIR: &str = "agents";
const SCHEMA_VERSION: u32 = 1;
const MAX_AUTOMATIC_PROMPT_BYTES: usize = 12_000;
const MAX_AUTOMATIC_FACTS_PER_SECTION: usize = 12;
const MIN_STABILITY: f32 = 0.75;
const MIN_REUSE_VALUE: f32 = 0.50;
const MIN_OWNER_RELEVANCE: f32 = 0.40;
const MIN_CONFIDENCE: f32 = 0.60;

pub(crate) const MANUAL_SYSTEM_PROMPT_FILENAME: &str = "manual_system_prompt.md";
pub(crate) const AUTOMATIC_PROMPT_FILENAME: &str = "automatic_prompt.md";
pub(crate) const OWNER_PROFILE_PROMPT_FILENAME: &str = "owner_profile.md";
pub(crate) const IDENTITY_FILENAME: &str = "identity.json";
pub(crate) const METADATA_FILENAME: &str = "metadata.json";
pub(crate) const MEMORY_DIR: &str = "memory";
pub(crate) const RELATIONSHIP_MEMORY_FILENAME: &str = "relationship_memory.json";
pub(crate) const DAILY_REFLECTIONS_DIR: &str = "daily_reflections";
pub(crate) const TASK_JOURNAL_DIR: &str = "task_journal";

/// The durable/non-durable context channels that may contribute to a model turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AgentContextChannel {
    /// Human-authored, durable per-agent prompt. The system may create the file,
    /// but must never rewrite its content as part of automated memory updates.
    ManualPermanentSystemPrompt,
    /// System-authored, durable per-agent prompt material updated from stable memory.
    AutomaticUpdatedPrompt,
    /// System-authored, durable profile of the human owner this agent serves.
    OwnerProfile,
    /// Non-durable live context such as blackboard, waiting state, and peer snapshots.
    RuntimeTailInjection,
}

impl AgentContextChannel {
    fn tag(self) -> &'static str {
        match self {
            Self::ManualPermanentSystemPrompt => "manual_permanent_system_prompt",
            Self::AutomaticUpdatedPrompt => "automatic_updated_prompt",
            Self::OwnerProfile => "owner_profile",
            Self::RuntimeTailInjection => "runtime_tail_injection",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct AgentContextContract {
    pub schema_version: u32,
    pub channels: Vec<AgentContextChannelContract>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct AgentContextChannelContract {
    pub channel: AgentContextChannel,
    pub owner: String,
    pub durable: bool,
    pub auto_writable: bool,
    pub purpose: String,
}

impl AgentContextContract {
    pub(crate) fn current() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            channels: vec![
                AgentContextChannelContract {
                    channel: AgentContextChannel::ManualPermanentSystemPrompt,
                    owner: "human".to_string(),
                    durable: true,
                    auto_writable: false,
                    purpose: "Human-maintained permanent system prompt material for one agent."
                        .to_string(),
                },
                AgentContextChannelContract {
                    channel: AgentContextChannel::AutomaticUpdatedPrompt,
                    owner: "system".to_string(),
                    durable: true,
                    auto_writable: true,
                    purpose: "System-maintained stable memory prompt for one agent, updated from durable evidence."
                        .to_string(),
                },
                AgentContextChannelContract {
                    channel: AgentContextChannel::OwnerProfile,
                    owner: "system".to_string(),
                    durable: true,
                    auto_writable: true,
                    purpose: "System-maintained durable summary of the human owner profile for one agent."
                        .to_string(),
                },
                AgentContextChannelContract {
                    channel: AgentContextChannel::RuntimeTailInjection,
                    owner: "runtime".to_string(),
                    durable: false,
                    auto_writable: false,
                    purpose: "Ephemeral turn context such as blackboard snapshots, wait state, and peer summaries."
                        .to_string(),
                },
            ],
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct AgentIdentityRecord {
    pub schema_version: u32,
    pub agent_id: String,
    pub display_name: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct AgentContextMetadata {
    pub schema_version: u32,
    pub agent_id: String,
    pub display_name: String,
    pub created_at: String,
    pub context_contract: AgentContextContract,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AgentAutomaticPromptSection {
    SelfCapabilityBoundary,
    OwnerCapabilityBoundary,
    OwnerPreference,
    RelationshipMemory,
    StableCollaborationRule,
}

impl AgentAutomaticPromptSection {
    fn title(self) -> &'static str {
        match self {
            Self::SelfCapabilityBoundary => "Self Capability Boundary",
            Self::OwnerCapabilityBoundary => "Owner Capability Boundary",
            Self::OwnerPreference => "Owner Preferences",
            Self::RelationshipMemory => "Relationship Memory",
            Self::StableCollaborationRule => "Stable Collaboration Rules",
        }
    }

    fn ordered() -> [Self; 5] {
        [
            Self::SelfCapabilityBoundary,
            Self::OwnerCapabilityBoundary,
            Self::OwnerPreference,
            Self::RelationshipMemory,
            Self::StableCollaborationRule,
        ]
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct AgentMemoryProvenance {
    pub source_type: String,
    pub source_path: Option<String>,
    pub note: Option<String>,
}

impl AgentMemoryProvenance {
    fn with_source_path(mut self, source_path: &Path) -> Self {
        if self.source_path.is_none() {
            self.source_path = Some(source_path.display().to_string());
        }
        self
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct AgentMemoryCandidate {
    pub section: Option<AgentAutomaticPromptSection>,
    pub text: String,
    pub stability: f32,
    pub reuse_value: f32,
    pub owner_relevance: f32,
    pub confidence: f32,
    pub provenance: AgentMemoryProvenance,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AgentMemoryAdmissionDestination {
    AutomaticPrompt,
    RelationshipMemory,
    Reject,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct AgentAutomaticPromptFact {
    pub section: AgentAutomaticPromptSection,
    pub text: String,
    pub confidence: f32,
    pub provenance: AgentMemoryProvenance,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct AgentMemoryAdmissionRecord {
    pub destination: AgentMemoryAdmissionDestination,
    pub section: Option<AgentAutomaticPromptSection>,
    pub text: String,
    pub reason: String,
    pub confidence: f32,
    pub provenance: AgentMemoryProvenance,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct AgentTaskJournalFile {
    #[serde(default = "default_schema_version")]
    schema_version: u32,
    #[serde(default)]
    candidates: Vec<AgentMemoryCandidate>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
enum AgentTaskJournalInput {
    Journal(AgentTaskJournalFile),
    Candidates(Vec<AgentMemoryCandidate>),
    Candidate(AgentMemoryCandidate),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct RelationshipMemoryFile {
    schema_version: u32,
    relationships: Vec<AgentAutomaticPromptFact>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct DailyReflectionFile {
    schema_version: u32,
    agent_id: String,
    display_name: String,
    reflection_date: String,
    updated_at: String,
    admitted: Vec<AgentMemoryAdmissionRecord>,
    rejected: Vec<AgentMemoryAdmissionRecord>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AgentAutomaticPromptUpdateStatus {
    Updated,
    SkippedAlreadyUpdated,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AgentAutomaticPromptUpdateReport {
    pub status: AgentAutomaticPromptUpdateStatus,
    pub reflection_file: PathBuf,
    pub automatic_prompt_file: PathBuf,
    pub admitted_facts: usize,
    pub rejected_facts: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AgentContextPaths {
    pub agent_root: PathBuf,
    pub identity_file: PathBuf,
    pub metadata_file: PathBuf,
    pub manual_system_prompt_file: PathBuf,
    pub automatic_prompt_file: PathBuf,
    pub owner_profile_file: PathBuf,
    pub memory_root: PathBuf,
    pub relationship_memory_file: PathBuf,
    pub daily_reflections_dir: PathBuf,
    pub task_journal_dir: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AgentPromptBlock {
    pub channel: AgentContextChannel,
    pub source_path: PathBuf,
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AgentPromptBundle {
    pub agent_id: String,
    pub display_name: String,
    pub paths: AgentContextPaths,
    pub blocks: Vec<AgentPromptBlock>,
}

impl AgentPromptBundle {
    pub(crate) fn render_developer_instructions(&self) -> Option<String> {
        if self.blocks.is_empty() {
            return None;
        }

        let mut output = String::new();
        output.push_str("<agent_durable_context schema_version=\"1\" agent_id=\"");
        output.push_str(&escape_xml_attr(&self.agent_id));
        output.push_str("\" display_name=\"");
        output.push_str(&escape_xml_attr(&self.display_name));
        output.push_str("\">\n");
        output.push_str("  <context_contract>Manual prompt is human-owned and never auto-written; automatic prompt is system-owned durable memory; runtime tail injection remains non-durable and is injected separately.</context_contract>\n");

        for block in &self.blocks {
            let tag = block.channel.tag();
            output.push_str("  <");
            output.push_str(tag);
            output.push_str(" source=\"");
            output.push_str(&escape_xml_attr(&block.source_path.display().to_string()));
            output.push_str("\">\n");
            output.push_str(block.text.trim());
            output.push('\n');
            output.push_str("  </");
            output.push_str(tag);
            output.push_str(">\n");
        }
        output.push_str("</agent_durable_context>");
        Some(output)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct AgentContextStore {
    codex_home: PathBuf,
    agent_id: String,
    display_name: String,
}

impl AgentContextStore {
    pub(crate) fn new(
        codex_home: impl Into<PathBuf>,
        agent_id: impl Into<String>,
        display_name: impl Into<String>,
    ) -> Self {
        Self {
            codex_home: codex_home.into(),
            agent_id: sanitize_agent_id(&agent_id.into()),
            display_name: display_name.into(),
        }
    }

    #[cfg(test)]
    pub(crate) fn agent_id(&self) -> &str {
        &self.agent_id
    }

    pub(crate) fn paths(&self) -> AgentContextPaths {
        let agent_root = self.codex_home.join(AGENTS_DIR).join(&self.agent_id);
        let memory_root = agent_root.join(MEMORY_DIR);
        AgentContextPaths {
            identity_file: agent_root.join(IDENTITY_FILENAME),
            metadata_file: agent_root.join(METADATA_FILENAME),
            manual_system_prompt_file: agent_root.join(MANUAL_SYSTEM_PROMPT_FILENAME),
            automatic_prompt_file: agent_root.join(AUTOMATIC_PROMPT_FILENAME),
            owner_profile_file: agent_root.join(OWNER_PROFILE_PROMPT_FILENAME),
            relationship_memory_file: memory_root.join(RELATIONSHIP_MEMORY_FILENAME),
            daily_reflections_dir: memory_root.join(DAILY_REFLECTIONS_DIR),
            task_journal_dir: memory_root.join(TASK_JOURNAL_DIR),
            memory_root,
            agent_root,
        }
    }

    pub(crate) async fn ensure_layout(&self) -> std::io::Result<AgentContextPaths> {
        let paths = self.paths();
        fs::create_dir_all(&paths.daily_reflections_dir).await?;
        fs::create_dir_all(&paths.task_journal_dir).await?;
        create_file_if_missing(&paths.manual_system_prompt_file, "").await?;
        create_file_if_missing(&paths.automatic_prompt_file, "").await?;
        create_file_if_missing(&paths.owner_profile_file, "").await?;

        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
        let identity = AgentIdentityRecord {
            schema_version: SCHEMA_VERSION,
            agent_id: self.agent_id.clone(),
            display_name: self.display_name.clone(),
        };
        create_json_if_missing(&paths.identity_file, &identity).await?;

        let metadata = AgentContextMetadata {
            schema_version: SCHEMA_VERSION,
            agent_id: self.agent_id.clone(),
            display_name: self.display_name.clone(),
            created_at: now,
            context_contract: AgentContextContract::current(),
        };
        create_json_if_missing(&paths.metadata_file, &metadata).await?;

        let relationship_seed = RelationshipMemoryFile {
            schema_version: SCHEMA_VERSION,
            relationships: Vec::new(),
        };
        create_json_if_missing(&paths.relationship_memory_file, &relationship_seed).await?;
        Ok(paths)
    }

    pub(crate) async fn load_prompt_bundle(&self) -> std::io::Result<AgentPromptBundle> {
        let paths = self.ensure_layout().await?;
        let mut blocks = Vec::new();
        if let Some(text) = read_non_empty_trimmed(&paths.manual_system_prompt_file).await? {
            blocks.push(AgentPromptBlock {
                channel: AgentContextChannel::ManualPermanentSystemPrompt,
                source_path: paths.manual_system_prompt_file.clone(),
                text,
            });
        }
        if let Some(text) = read_non_empty_trimmed(&paths.automatic_prompt_file).await? {
            blocks.push(AgentPromptBlock {
                channel: AgentContextChannel::AutomaticUpdatedPrompt,
                source_path: paths.automatic_prompt_file.clone(),
                text,
            });
        }
        if let Some(text) = read_non_empty_trimmed(&paths.owner_profile_file).await? {
            blocks.push(AgentPromptBlock {
                channel: AgentContextChannel::OwnerProfile,
                source_path: paths.owner_profile_file.clone(),
                text,
            });
        }
        Ok(AgentPromptBundle {
            agent_id: self.agent_id.clone(),
            display_name: self.display_name.clone(),
            paths,
            blocks,
        })
    }

    pub(crate) async fn refresh_automatic_prompt_once_per_day(
        &self,
        now: DateTime<Utc>,
    ) -> std::io::Result<AgentAutomaticPromptUpdateReport> {
        let paths = self.ensure_layout().await?;
        let reflection_date = now.format("%Y-%m-%d").to_string();
        let reflection_file = paths
            .daily_reflections_dir
            .join(format!("{reflection_date}.json"));
        if reflection_file.try_exists()? {
            return Ok(AgentAutomaticPromptUpdateReport {
                status: AgentAutomaticPromptUpdateStatus::SkippedAlreadyUpdated,
                reflection_file,
                automatic_prompt_file: paths.automatic_prompt_file,
                admitted_facts: 0,
                rejected_facts: 0,
            });
        }

        let candidates = read_task_journal_candidates(&paths.task_journal_dir).await?;
        let (admitted, rejected) = admit_memory_candidates(candidates);
        let prompt_facts = admitted
            .iter()
            .filter_map(admission_record_to_prompt_fact)
            .collect::<Vec<_>>();
        let prompt_facts = bounded_prompt_facts(prompt_facts);

        let reflection = DailyReflectionFile {
            schema_version: SCHEMA_VERSION,
            agent_id: self.agent_id.clone(),
            display_name: self.display_name.clone(),
            reflection_date,
            updated_at: now.to_rfc3339_opts(SecondsFormat::Secs, true),
            admitted,
            rejected,
        };
        let reflection_contents =
            serde_json::to_string_pretty(&reflection).map_err(std::io::Error::other)?;
        create_file_if_missing(&reflection_file, &format!("{reflection_contents}\n")).await?;

        if !prompt_facts.is_empty() {
            let relationship_memory = RelationshipMemoryFile {
                schema_version: SCHEMA_VERSION,
                relationships: prompt_facts
                    .iter()
                    .filter(|fact| fact.section == AgentAutomaticPromptSection::RelationshipMemory)
                    .cloned()
                    .collect(),
            };
            let relationship_memory_contents = serde_json::to_string_pretty(&relationship_memory)
                .map_err(std::io::Error::other)?;
            write_file_atomically(
                &paths.relationship_memory_file,
                format!("{relationship_memory_contents}\n"),
            )
            .await?;
            let automatic_prompt =
                render_automatic_prompt(&self.agent_id, &self.display_name, now, &prompt_facts);
            self.write_automatic_prompt(&automatic_prompt).await?;
        }

        Ok(AgentAutomaticPromptUpdateReport {
            status: AgentAutomaticPromptUpdateStatus::Updated,
            reflection_file,
            automatic_prompt_file: paths.automatic_prompt_file,
            admitted_facts: prompt_facts.len(),
            rejected_facts: reflection.rejected.len(),
        })
    }

    async fn write_automatic_prompt(&self, contents: &str) -> std::io::Result<()> {
        let paths = self.ensure_layout().await?;
        write_file_atomically(
            &paths.automatic_prompt_file,
            normalize_prompt_contents(contents),
        )
        .await
    }

    pub(crate) async fn write_owner_profile_prompt(&self, contents: &str) -> std::io::Result<()> {
        let paths = self.ensure_layout().await?;
        write_file_atomically(
            &paths.owner_profile_file,
            normalize_prompt_contents(contents),
        )
        .await
    }
}

pub(crate) async fn write_agent_owner_profile_prompt_for_agent(
    codex_home: &Path,
    agent_id: &str,
    agent_display_name: &str,
    contents: &str,
) -> std::io::Result<PathBuf> {
    let store = AgentContextStore::new(
        codex_home.to_path_buf(),
        agent_id.to_string(),
        agent_display_name.to_string(),
    );
    store.write_owner_profile_prompt(contents).await?;
    Ok(store.paths().owner_profile_file)
}

pub(crate) async fn build_agent_context_developer_instructions_for_agent(
    codex_home: &Path,
    agent_id: &str,
    agent_display_name: &str,
) -> Option<String> {
    let store = AgentContextStore::new(
        codex_home.to_path_buf(),
        agent_id.to_string(),
        agent_display_name.to_string(),
    );
    match store.load_prompt_bundle().await {
        Ok(bundle) => bundle.render_developer_instructions(),
        Err(err) => {
            tracing::warn!(
                agent_id,
                agent_display_name,
                error = %err,
                "failed to load agent durable context"
            );
            None
        }
    }
}

pub(crate) async fn refresh_agent_automatic_prompt_once_per_day_for_agent(
    codex_home: &Path,
    agent_id: &str,
    agent_display_name: &str,
) -> std::io::Result<AgentAutomaticPromptUpdateReport> {
    let store = AgentContextStore::new(
        codex_home.to_path_buf(),
        agent_id.to_string(),
        agent_display_name.to_string(),
    );
    store
        .refresh_automatic_prompt_once_per_day(Utc::now())
        .await
}

async fn read_task_journal_candidates(
    task_journal_dir: &Path,
) -> std::io::Result<Vec<AgentMemoryCandidate>> {
    let mut entries = fs::read_dir(task_journal_dir).await?;
    let mut files = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("json") {
            files.push(path);
        }
    }
    files.sort();

    let mut candidates = Vec::new();
    for path in files {
        let contents = fs::read_to_string(&path).await?;
        let parsed: AgentTaskJournalInput = serde_json::from_str(&contents).map_err(|err| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid task journal {}: {err}", path.display()),
            )
        })?;
        let parsed_candidates = match parsed {
            AgentTaskJournalInput::Journal(journal) => journal.candidates,
            AgentTaskJournalInput::Candidates(candidates) => candidates,
            AgentTaskJournalInput::Candidate(candidate) => vec![candidate],
        };
        candidates.extend(parsed_candidates.into_iter().map(|mut candidate| {
            candidate.provenance = candidate.provenance.with_source_path(&path);
            candidate
        }));
    }
    Ok(candidates)
}

fn admit_memory_candidates(
    candidates: Vec<AgentMemoryCandidate>,
) -> (
    Vec<AgentMemoryAdmissionRecord>,
    Vec<AgentMemoryAdmissionRecord>,
) {
    let mut admitted = Vec::new();
    let mut rejected = Vec::new();
    let mut seen = BTreeSet::new();

    for candidate in candidates {
        let normalized_text = normalize_fact_text(&candidate.text);
        let mut rejection_reason = None;
        if normalized_text.is_empty() {
            rejection_reason = Some("empty memory candidate".to_string());
        } else if candidate.section.is_none() {
            rejection_reason =
                Some("candidate is not assigned to a stable automatic prompt section".to_string());
        } else if looks_like_transient_runtime_state(&normalized_text) {
            rejection_reason = Some("candidate describes transient runtime/task state".to_string());
        } else if candidate.stability < MIN_STABILITY {
            rejection_reason = Some(format!("stability below threshold {MIN_STABILITY}"));
        } else if candidate.reuse_value < MIN_REUSE_VALUE {
            rejection_reason = Some(format!("reuse_value below threshold {MIN_REUSE_VALUE}"));
        } else if candidate.owner_relevance < MIN_OWNER_RELEVANCE {
            rejection_reason = Some(format!(
                "owner_relevance below threshold {MIN_OWNER_RELEVANCE}"
            ));
        } else if candidate.confidence < MIN_CONFIDENCE {
            rejection_reason = Some(format!("confidence below threshold {MIN_CONFIDENCE}"));
        }

        if let Some(reason) = rejection_reason {
            rejected.push(AgentMemoryAdmissionRecord {
                destination: AgentMemoryAdmissionDestination::Reject,
                section: candidate.section,
                text: normalized_text,
                reason,
                confidence: candidate.confidence,
                provenance: candidate.provenance,
            });
            continue;
        }

        let section = candidate.section.expect("section checked above");
        if !seen.insert((section, normalized_text.clone())) {
            rejected.push(AgentMemoryAdmissionRecord {
                destination: AgentMemoryAdmissionDestination::Reject,
                section: Some(section),
                text: normalized_text,
                reason: "duplicate stable memory candidate".to_string(),
                confidence: candidate.confidence,
                provenance: candidate.provenance,
            });
            continue;
        }

        let destination = if section == AgentAutomaticPromptSection::RelationshipMemory {
            AgentMemoryAdmissionDestination::RelationshipMemory
        } else {
            AgentMemoryAdmissionDestination::AutomaticPrompt
        };
        admitted.push(AgentMemoryAdmissionRecord {
            destination,
            section: Some(section),
            text: normalized_text,
            reason: "stable reusable fact admitted to automatic prompt memory".to_string(),
            confidence: candidate.confidence,
            provenance: candidate.provenance,
        });
    }

    admitted.sort_by(|left, right| {
        left.section
            .cmp(&right.section)
            .then_with(|| right.confidence.total_cmp(&left.confidence))
            .then_with(|| left.text.cmp(&right.text))
    });
    rejected.sort_by(|left, right| {
        left.section
            .cmp(&right.section)
            .then_with(|| left.text.cmp(&right.text))
    });
    (admitted, rejected)
}

fn admission_record_to_prompt_fact(
    record: &AgentMemoryAdmissionRecord,
) -> Option<AgentAutomaticPromptFact> {
    let section = record.section?;
    Some(AgentAutomaticPromptFact {
        section,
        text: record.text.clone(),
        confidence: record.confidence,
        provenance: record.provenance.clone(),
    })
}

fn bounded_prompt_facts(facts: Vec<AgentAutomaticPromptFact>) -> Vec<AgentAutomaticPromptFact> {
    let mut by_section: BTreeMap<AgentAutomaticPromptSection, Vec<AgentAutomaticPromptFact>> =
        BTreeMap::new();
    for fact in facts {
        by_section.entry(fact.section).or_default().push(fact);
    }

    let mut bounded = Vec::new();
    for section in AgentAutomaticPromptSection::ordered() {
        let Some(mut section_facts) = by_section.remove(&section) else {
            continue;
        };
        section_facts.sort_by(|left, right| {
            right
                .confidence
                .total_cmp(&left.confidence)
                .then_with(|| left.text.cmp(&right.text))
        });
        bounded.extend(
            section_facts
                .into_iter()
                .take(MAX_AUTOMATIC_FACTS_PER_SECTION),
        );
    }
    bounded
}

fn render_automatic_prompt(
    agent_id: &str,
    display_name: &str,
    updated_at: DateTime<Utc>,
    facts: &[AgentAutomaticPromptFact],
) -> String {
    let mut output = String::new();
    output.push_str("# Automatically Updated Agent Prompt\n\n");
    output.push_str("schema_version: 1\n");
    output.push_str("agent_id: ");
    output.push_str(agent_id);
    output.push('\n');
    output.push_str("display_name: ");
    output.push_str(display_name);
    output.push('\n');
    output.push_str("updated_at: ");
    output.push_str(&updated_at.to_rfc3339_opts(SecondsFormat::Secs, true));
    output.push_str("\nowner: system\n");
    output.push_str("scope: stable reusable memory only; runtime state, blackboard snapshots, waiting status, and current tasks are excluded.\n\n");

    for section in AgentAutomaticPromptSection::ordered() {
        output.push_str("## ");
        output.push_str(section.title());
        output.push('\n');
        let mut wrote_fact = false;
        for fact in facts.iter().filter(|fact| fact.section == section) {
            wrote_fact = true;
            output.push_str("- ");
            output.push_str(&fact.text);
            output.push_str("\n  confidence: ");
            output.push_str(&format!("{:.2}", fact.confidence));
            output.push_str("; provenance: ");
            output.push_str(&fact.provenance.source_type);
            if let Some(source_path) = &fact.provenance.source_path {
                output.push_str(" at ");
                output.push_str(source_path);
            }
            if let Some(note) = &fact.provenance.note {
                output.push_str("; note: ");
                output.push_str(note);
            }
            output.push('\n');
        }
        if !wrote_fact {
            output.push_str("- No stable facts admitted yet.\n");
        }
        output.push('\n');
    }

    if output.len() <= MAX_AUTOMATIC_PROMPT_BYTES {
        return output;
    }

    let mut truncated = output;
    truncated.truncate(MAX_AUTOMATIC_PROMPT_BYTES);
    truncated.push_str("\n[automatic prompt truncated at durable context limit]\n");
    truncated
}

async fn read_non_empty_trimmed(path: &Path) -> std::io::Result<Option<String>> {
    let contents = fs::read_to_string(path).await?;
    let trimmed = contents.trim();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed.to_string()))
    }
}

async fn create_file_if_missing(path: &Path, contents: &str) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("path has no parent: {}", path.display()),
        )
    })?;
    fs::create_dir_all(parent).await?;
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
    {
        Ok(mut file) => {
            file.write_all(contents.as_bytes()).await?;
            file.sync_all().await
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(err) => Err(err),
    }
}

async fn create_json_if_missing<T>(path: &Path, value: &T) -> std::io::Result<()>
where
    T: Serialize,
{
    let contents = serde_json::to_string_pretty(value).map_err(std::io::Error::other)?;
    create_file_if_missing(path, &format!("{contents}\n")).await
}

async fn write_file_atomically(path: &Path, contents: impl AsRef<[u8]>) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("path has no parent: {}", path.display()),
        )
    })?;
    fs::create_dir_all(parent).await?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("path has invalid file name: {}", path.display()),
            )
        })?;
    let tmp_path = parent.join(format!(".{file_name}.{}.tmp", Uuid::new_v4()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp_path)
        .await?;
    file.write_all(contents.as_ref()).await?;
    file.sync_all().await?;
    drop(file);
    fs::rename(&tmp_path, path).await
}

fn normalize_prompt_contents(contents: &str) -> String {
    let trimmed = contents.trim();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}\n")
    }
}

fn normalize_fact_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn looks_like_transient_runtime_state(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "currently waiting",
        "waiting for",
        "current task",
        "today's task",
        "this task",
        "right now",
        "blackboard",
        "inbox",
        "blocked",
        "recent message",
        "latest reply",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

fn sanitize_agent_id(display_name: &str) -> String {
    let mut sanitized = String::with_capacity(display_name.len().max(1));
    let mut previous_dash = false;
    for ch in display_name.chars() {
        let next = if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.') {
            previous_dash = false;
            Some(ch)
        } else if previous_dash {
            None
        } else {
            previous_dash = true;
            Some('-')
        };
        if let Some(ch) = next {
            sanitized.push(ch);
        }
    }
    let sanitized = sanitized
        .trim_matches(|ch| matches!(ch, '-' | '.' | '/' | '\\'))
        .to_string();
    if sanitized.is_empty() || sanitized == "." || sanitized == ".." {
        super::UNNAMED_AGENT_NAME.to_string()
    } else {
        sanitized
    }
}

fn escape_xml_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn default_schema_version() -> u32 {
    SCHEMA_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use pretty_assertions::assert_eq;

    fn stable_candidate(section: AgentAutomaticPromptSection, text: &str) -> AgentMemoryCandidate {
        AgentMemoryCandidate {
            section: Some(section),
            text: text.to_string(),
            stability: 0.95,
            reuse_value: 0.90,
            owner_relevance: 0.85,
            confidence: 0.88,
            provenance: AgentMemoryProvenance {
                source_type: "task_journal".to_string(),
                source_path: None,
                note: Some("test evidence".to_string()),
            },
        }
    }

    #[tokio::test]
    async fn ensure_layout_creates_agent_storage_under_codex_home() {
        let temp = tempfile::tempdir().expect("temp dir");
        let store = AgentContextStore::new(temp.path(), "agent_product", "Product Agent");

        let paths = store.ensure_layout().await.expect("layout created");

        assert_eq!(store.agent_id(), "agent_product");
        assert!(paths.agent_root.starts_with(temp.path()));
        assert!(paths.manual_system_prompt_file.exists());
        assert!(paths.automatic_prompt_file.exists());
        assert!(paths.owner_profile_file.exists());
        assert!(paths.identity_file.exists());
        assert!(paths.metadata_file.exists());
        assert!(paths.relationship_memory_file.exists());
        assert!(paths.daily_reflections_dir.exists());
        assert!(paths.task_journal_dir.exists());
    }

    #[tokio::test]
    async fn agent_storage_paths_do_not_collide_on_duplicate_display_names() {
        let temp = tempfile::tempdir().expect("temp dir");
        let first = AgentContextStore::new(temp.path(), "agent-1", "Shared Name");
        let second = AgentContextStore::new(temp.path(), "agent-2", "Shared Name");

        assert_ne!(first.paths().agent_root, second.paths().agent_root);
        assert_ne!(
            first.paths().manual_system_prompt_file,
            second.paths().manual_system_prompt_file
        );
        assert_ne!(
            first.paths().automatic_prompt_file,
            second.paths().automatic_prompt_file
        );
        assert_ne!(
            first.paths().owner_profile_file,
            second.paths().owner_profile_file
        );
    }

    #[tokio::test]
    async fn automatic_prompt_writer_never_modifies_manual_prompt() {
        let temp = tempfile::tempdir().expect("temp dir");
        let store = AgentContextStore::new(temp.path(), "agent_a", "agent_a");
        let paths = store.ensure_layout().await.expect("layout created");
        fs::write(&paths.manual_system_prompt_file, "manual human prompt\n")
            .await
            .expect("manual write");

        store
            .write_automatic_prompt("stable owner preference")
            .await
            .expect("auto write");

        assert_eq!(
            fs::read_to_string(&paths.manual_system_prompt_file)
                .await
                .expect("read manual"),
            "manual human prompt\n"
        );
        assert_eq!(
            fs::read_to_string(&paths.automatic_prompt_file)
                .await
                .expect("read automatic"),
            "stable owner preference\n"
        );
        assert_eq!(
            fs::read_to_string(&paths.owner_profile_file)
                .await
                .expect("read owner profile"),
            ""
        );
    }

    #[tokio::test]
    async fn render_developer_instructions_separates_durable_channels() {
        let temp = tempfile::tempdir().expect("temp dir");
        let store = AgentContextStore::new(temp.path(), "agent_a", "agent_a");
        let paths = store.ensure_layout().await.expect("layout created");
        fs::write(&paths.manual_system_prompt_file, "manual identity")
            .await
            .expect("manual write");
        store
            .write_automatic_prompt("owner prefers concise answers")
            .await
            .expect("auto write");
        store
            .write_owner_profile_prompt("role: CEO\nreport_preference: 先结论后细节")
            .await
            .expect("owner profile write");

        let rendered = store
            .load_prompt_bundle()
            .await
            .expect("bundle")
            .render_developer_instructions()
            .expect("rendered");

        assert!(rendered.contains("<agent_durable_context"));
        assert!(rendered.contains("<manual_permanent_system_prompt"));
        assert!(rendered.contains("manual identity"));
        assert!(rendered.contains("<automatic_updated_prompt"));
        assert!(rendered.contains("owner prefers concise answers"));
        assert!(rendered.contains("<owner_profile"));
        assert!(rendered.contains("role: CEO"));
        assert!(rendered.contains("report_preference: 先结论后细节"));
        assert!(!rendered.contains("<runtime_tail_injection"));
    }

    #[tokio::test]
    async fn daily_update_admits_only_stable_prompt_memory_and_is_idempotent() {
        let temp = tempfile::tempdir().expect("temp dir");
        let store = AgentContextStore::new(temp.path(), "agent_a", "Agent A");
        let paths = store.ensure_layout().await.expect("layout created");
        fs::write(&paths.manual_system_prompt_file, "manual identity\n")
            .await
            .expect("manual write");
        let journal = AgentTaskJournalFile {
            schema_version: SCHEMA_VERSION,
            candidates: vec![
                stable_candidate(
                    AgentAutomaticPromptSection::OwnerCapabilityBoundary,
                    "Owner is comfortable reviewing Rust architecture but prefers concise diffs.",
                ),
                stable_candidate(
                    AgentAutomaticPromptSection::RelationshipMemory,
                    "Morgan-explorer is strong at codebase mapping and evidence tables.",
                ),
                AgentMemoryCandidate {
                    text: "Currently waiting for Jordan on this task".to_string(),
                    ..stable_candidate(
                        AgentAutomaticPromptSection::StableCollaborationRule,
                        "placeholder",
                    )
                },
            ],
        };
        fs::write(
            paths.task_journal_dir.join("2026-05-15.json"),
            serde_json::to_string_pretty(&journal).expect("journal json"),
        )
        .await
        .expect("write journal");

        let now = Utc
            .with_ymd_and_hms(2026, 5, 15, 12, 0, 0)
            .single()
            .expect("timestamp");
        let report = store
            .refresh_automatic_prompt_once_per_day(now)
            .await
            .expect("refresh");
        assert_eq!(report.status, AgentAutomaticPromptUpdateStatus::Updated);
        assert_eq!(report.admitted_facts, 2);
        assert_eq!(report.rejected_facts, 1);

        let manual_prompt = fs::read_to_string(&paths.manual_system_prompt_file)
            .await
            .expect("manual prompt");
        let automatic_prompt = fs::read_to_string(&paths.automatic_prompt_file)
            .await
            .expect("automatic prompt");
        let relationship_memory = fs::read_to_string(&paths.relationship_memory_file)
            .await
            .expect("relationship memory");
        assert_eq!(manual_prompt, "manual identity\n");
        assert!(automatic_prompt.contains("## Owner Capability Boundary"));
        assert!(automatic_prompt.contains("Owner is comfortable reviewing Rust architecture"));
        assert!(automatic_prompt.contains("## Relationship Memory"));
        assert!(automatic_prompt.contains("Morgan-explorer is strong"));
        assert!(!automatic_prompt.contains("Currently waiting"));
        assert!(relationship_memory.contains("Morgan-explorer is strong"));

        let second_report = store
            .refresh_automatic_prompt_once_per_day(now)
            .await
            .expect("second refresh");
        assert_eq!(
            second_report.status,
            AgentAutomaticPromptUpdateStatus::SkippedAlreadyUpdated
        );
        assert_eq!(
            fs::read_to_string(&paths.automatic_prompt_file)
                .await
                .expect("automatic prompt unchanged"),
            automatic_prompt
        );
    }

    #[tokio::test]
    async fn next_day_update_creates_new_reflection() {
        let temp = tempfile::tempdir().expect("temp dir");
        let store = AgentContextStore::new(temp.path(), "agent_a", "Agent A");
        let paths = store.ensure_layout().await.expect("layout created");
        let journal = AgentTaskJournalFile {
            schema_version: SCHEMA_VERSION,
            candidates: vec![stable_candidate(
                AgentAutomaticPromptSection::SelfCapabilityBoundary,
                "Agent A is reliable at Rust test-first refactors.",
            )],
        };
        fs::write(
            paths.task_journal_dir.join("facts.json"),
            serde_json::to_string_pretty(&journal).expect("journal json"),
        )
        .await
        .expect("write journal");

        let first_day = Utc
            .with_ymd_and_hms(2026, 5, 15, 1, 0, 0)
            .single()
            .expect("first timestamp");
        let second_day = Utc
            .with_ymd_and_hms(2026, 5, 16, 1, 0, 0)
            .single()
            .expect("second timestamp");
        store
            .refresh_automatic_prompt_once_per_day(first_day)
            .await
            .expect("first refresh");
        let first_prompt = fs::read_to_string(&paths.automatic_prompt_file)
            .await
            .expect("first prompt");
        store
            .refresh_automatic_prompt_once_per_day(second_day)
            .await
            .expect("second refresh");
        let second_prompt = fs::read_to_string(&paths.automatic_prompt_file)
            .await
            .expect("second prompt");

        assert!(paths.daily_reflections_dir.join("2026-05-15.json").exists());
        assert!(paths.daily_reflections_dir.join("2026-05-16.json").exists());
        assert!(first_prompt.contains("updated_at: 2026-05-15T01:00:00Z"));
        assert!(second_prompt.contains("updated_at: 2026-05-16T01:00:00Z"));
    }
}
