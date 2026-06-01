use std::collections::HashMap;

use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PilotAccount {
    pub user_id: String,
    pub username: String,
    pub password: String,
    pub agent_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HumanProfile {
    pub user_id: String,
    pub role: String,
    pub capability_labels: Vec<String>,
    pub do_not_disturb: Vec<String>,
    pub report_preference: String,
    pub last_reflection_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub likes: Vec<String>,
    #[serde(default)]
    pub dislikes: Vec<String>,
    #[serde(default)]
    pub preferences: Vec<String>,
    #[serde(default)]
    pub profile_facts: Vec<UserProfileFact>,
    #[serde(default)]
    pub conversation_evidence: Vec<UserProfileEvidence>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserProfileFactCategory {
    Like,
    Dislike,
    Capability,
    Preference,
    DoNotDisturb,
    CommunicationStyle,
}

impl UserProfileFactCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Like => "like",
            Self::Dislike => "dislike",
            Self::Capability => "capability",
            Self::Preference => "preference",
            Self::DoNotDisturb => "do_not_disturb",
            Self::CommunicationStyle => "communication_style",
        }
    }
}

impl std::fmt::Display for UserProfileFactCategory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserProfileFactStatus {
    Candidate,
    Active,
    Superseded,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UserProfileEvidence {
    pub event_id: String,
    pub message_id: String,
    pub agent_id: String,
    pub content: String,
    pub observed_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UserProfileFact {
    pub fact_id: String,
    pub user_id: String,
    pub category: UserProfileFactCategory,
    pub value: String,
    pub confidence: u8,
    pub salience: u8,
    pub status: UserProfileFactStatus,
    pub evidence_message_ids: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentProfile {
    pub agent_id: String,
    pub owner_user_id: String,
    pub allowed_tools: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentOwnerBindingType {
    PrimaryOwner,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentOwnerBindingStatus {
    Active,
    Inactive,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentOwnerBinding {
    pub agent_id: String,
    pub owner_user_id: String,
    pub binding_type: AgentOwnerBindingType,
    pub status: AgentOwnerBindingStatus,
    pub version: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PilotSession {
    pub user_id: String,
    pub agent_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DailyReflection {
    pub user_id: String,
    pub profile_updates: Vec<String>,
    #[serde(default)]
    pub admitted_facts: Vec<UserProfileFact>,
    pub reflected_at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub struct PilotDirectory {
    accounts: HashMap<String, PilotAccount>,
    human_profiles: HashMap<String, HumanProfile>,
    agent_profiles: HashMap<String, AgentProfile>,
    agent_owner_bindings: HashMap<String, AgentOwnerBinding>,
}

impl PilotDirectory {
    pub fn six_person_seed() -> Self {
        let seeds = [
            (
                "ceo",
                "user_ceo",
                "agent_ceo",
                "CEO",
                vec!["方向判断", "资源决策", "最终优先级"],
                vec!["实现细节频繁打扰"],
            ),
            (
                "employee_a",
                "user_a",
                "agent_a",
                "算法",
                vec!["模型方案", "实验设计"],
                vec!["部署琐事"],
            ),
            (
                "employee_b",
                "user_b",
                "agent_b",
                "Infra",
                vec!["部署", "算力", "成本"],
                vec!["审美细节"],
            ),
            (
                "employee_c",
                "user_c",
                "agent_c",
                "产品",
                vec!["用户场景", "需求优先级"],
                vec!["低风险格式调整"],
            ),
            (
                "employee_d",
                "user_d",
                "agent_d",
                "工程",
                vec!["代码改动", "技术风险"],
                vec!["普通资料搜索"],
            ),
            (
                "employee_e",
                "user_e",
                "agent_e",
                "运营设计",
                vec!["表达", "审美", "外部沟通"],
                vec!["底层实现细节"],
            ),
        ];
        let mut accounts = HashMap::new();
        let mut human_profiles = HashMap::new();
        let mut agent_profiles = HashMap::new();
        let mut agent_owner_bindings = HashMap::new();
        for (username, user_id, agent_id, role, capabilities, avoid) in seeds {
            accounts.insert(
                username.to_string(),
                PilotAccount {
                    user_id: user_id.to_string(),
                    username: username.to_string(),
                    password: "password".to_string(),
                    agent_id: agent_id.to_string(),
                },
            );
            human_profiles.insert(
                user_id.to_string(),
                HumanProfile {
                    user_id: user_id.to_string(),
                    role: role.to_string(),
                    capability_labels: capabilities.into_iter().map(str::to_string).collect(),
                    do_not_disturb: avoid.into_iter().map(str::to_string).collect(),
                    report_preference: "先结论后细节".to_string(),
                    last_reflection_at: None,
                    likes: Vec::new(),
                    dislikes: Vec::new(),
                    preferences: Vec::new(),
                    profile_facts: Vec::new(),
                    conversation_evidence: Vec::new(),
                },
            );
            agent_profiles.insert(
                agent_id.to_string(),
                AgentProfile {
                    agent_id: agent_id.to_string(),
                    owner_user_id: user_id.to_string(),
                    allowed_tools: vec![
                        "call".to_string(),
                        "wait".to_string(),
                        "read_agent_status".to_string(),
                    ],
                },
            );
            agent_owner_bindings.insert(
                agent_id.to_string(),
                AgentOwnerBinding {
                    agent_id: agent_id.to_string(),
                    owner_user_id: user_id.to_string(),
                    binding_type: AgentOwnerBindingType::PrimaryOwner,
                    status: AgentOwnerBindingStatus::Active,
                    version: 1,
                },
            );
        }
        Self {
            accounts,
            human_profiles,
            agent_profiles,
            agent_owner_bindings,
        }
    }

    pub fn from_parts(
        accounts: HashMap<String, PilotAccount>,
        human_profiles: HashMap<String, HumanProfile>,
        agent_profiles: HashMap<String, AgentProfile>,
        agent_owner_bindings: HashMap<String, AgentOwnerBinding>,
    ) -> Self {
        let agent_owner_bindings = if agent_owner_bindings.is_empty() {
            derive_owner_bindings_from_profiles(&agent_profiles)
        } else {
            agent_owner_bindings
        };
        Self {
            accounts,
            human_profiles,
            agent_profiles,
            agent_owner_bindings,
        }
    }

    pub fn accounts(&self) -> &HashMap<String, PilotAccount> {
        &self.accounts
    }

    pub fn human_profiles(&self) -> &HashMap<String, HumanProfile> {
        &self.human_profiles
    }

    pub fn agent_profiles(&self) -> &HashMap<String, AgentProfile> {
        &self.agent_profiles
    }

    pub fn agent_owner_bindings(&self) -> &HashMap<String, AgentOwnerBinding> {
        &self.agent_owner_bindings
    }

    pub fn account_for_user(&self, user_id: &str) -> Option<&PilotAccount> {
        self.accounts
            .values()
            .find(|account| account.user_id == user_id)
    }

    pub fn account_count(&self) -> usize {
        self.accounts.len()
    }

    pub fn login(&self, username: &str, password: &str) -> Option<PilotSession> {
        let account = self.accounts.get(username)?;
        if account.password != password {
            return None;
        }
        Some(PilotSession {
            user_id: account.user_id.clone(),
            agent_id: account.agent_id.clone(),
        })
    }

    pub fn human_profile(&self, user_id: &str) -> Option<&HumanProfile> {
        self.human_profiles.get(user_id)
    }

    pub fn agent_profile(&self, agent_id: &str) -> Option<&AgentProfile> {
        self.agent_profiles.get(agent_id)
    }

    pub fn active_owner_binding(&self, agent_id: &str) -> Option<&AgentOwnerBinding> {
        self.agent_owner_bindings.get(agent_id).filter(|binding| {
            binding.binding_type == AgentOwnerBindingType::PrimaryOwner
                && binding.status == AgentOwnerBindingStatus::Active
        })
    }

    pub fn run_mini_interview(
        &mut self,
        user_id: &str,
        role: impl Into<String>,
        capabilities: impl IntoIterator<Item = impl Into<String>>,
        avoid: impl IntoIterator<Item = impl Into<String>>,
        report_preference: impl Into<String>,
    ) -> Option<&HumanProfile> {
        let profile = self.human_profiles.get_mut(user_id)?;
        profile.role = role.into();
        profile.capability_labels = capabilities.into_iter().map(Into::into).collect();
        profile.do_not_disturb = avoid.into_iter().map(Into::into).collect();
        profile.report_preference = report_preference.into();
        self.human_profiles.get(user_id)
    }

    pub fn daily_reflection(
        &mut self,
        user_id: &str,
        profile_updates: impl IntoIterator<Item = impl Into<String>>,
    ) -> Option<DailyReflection> {
        let reflected_at = Utc::now();
        let profile = self.human_profiles.get_mut(user_id)?;
        let profile_updates = profile_updates
            .into_iter()
            .map(Into::into)
            .collect::<Vec<_>>();
        let mut admitted_facts = Vec::new();
        for update in &profile_updates {
            let update = update.trim();
            if update.is_empty() {
                continue;
            }
            let category = if profile.do_not_disturb.iter().any(|value| value == update) {
                UserProfileFactCategory::DoNotDisturb
            } else {
                UserProfileFactCategory::Capability
            };
            let fact = admit_profile_fact(
                profile,
                category,
                update,
                Some(format!("daily-reflection-{}", reflected_at.to_rfc3339())),
                reflected_at,
                78,
                72,
            );
            admitted_facts.push(fact);
        }
        profile.last_reflection_at = Some(reflected_at);
        Some(DailyReflection {
            user_id: user_id.to_string(),
            profile_updates,
            admitted_facts,
            reflected_at,
        })
    }

    pub fn record_owner_message_profile_evidence(
        &mut self,
        user_id: &str,
        agent_id: &str,
        message_id: &str,
        content: &str,
    ) -> Option<Vec<UserProfileFact>> {
        let observed_at = Utc::now();
        let profile = self.human_profiles.get_mut(user_id)?;
        if !profile
            .conversation_evidence
            .iter()
            .any(|evidence| evidence.message_id == message_id)
        {
            profile.conversation_evidence.push(UserProfileEvidence {
                event_id: format!("profile-event-{message_id}"),
                message_id: message_id.to_string(),
                agent_id: agent_id.to_string(),
                content: content.to_string(),
                observed_at,
            });
        }
        let candidates = extract_profile_fact_candidates(content);
        let mut admitted = Vec::new();
        for (category, value, confidence, salience) in candidates {
            let fact = admit_profile_fact(
                profile,
                category,
                &value,
                Some(message_id.to_string()),
                observed_at,
                confidence,
                salience,
            );
            admitted.push(fact);
        }
        Some(admitted)
    }
}

pub fn derive_owner_bindings_from_profiles(
    agent_profiles: &HashMap<String, AgentProfile>,
) -> HashMap<String, AgentOwnerBinding> {
    agent_profiles
        .iter()
        .map(|(agent_id, profile)| {
            (
                agent_id.clone(),
                AgentOwnerBinding {
                    agent_id: profile.agent_id.clone(),
                    owner_user_id: profile.owner_user_id.clone(),
                    binding_type: AgentOwnerBindingType::PrimaryOwner,
                    status: AgentOwnerBindingStatus::Active,
                    version: 1,
                },
            )
        })
        .collect()
}

fn extract_profile_fact_candidates(
    content: &str,
) -> Vec<(UserProfileFactCategory, String, u8, u8)> {
    let mut facts = Vec::new();
    for clause in split_profile_clauses(content) {
        let clause = clause.trim();
        if clause.is_empty() {
            continue;
        }
        admit_marker_candidate(
            &mut facts,
            clause,
            UserProfileFactCategory::Dislike,
            &["我不喜欢", "不喜欢", "我讨厌", "讨厌", "反感", "不爱"],
            88,
            84,
        );
        admit_marker_candidate(
            &mut facts,
            clause,
            UserProfileFactCategory::Like,
            &["我喜欢", "喜欢", "偏爱", "爱用"],
            86,
            80,
        );
        admit_marker_candidate(
            &mut facts,
            clause,
            UserProfileFactCategory::Capability,
            &[
                "我擅长",
                "擅长",
                "我会",
                "我负责",
                "负责",
                "我的能力是",
                "能力是",
            ],
            84,
            78,
        );
        admit_marker_candidate(
            &mut facts,
            clause,
            UserProfileFactCategory::DoNotDisturb,
            &[
                "不要打扰",
                "不要频繁打扰",
                "别打扰",
                "不要提醒",
                "别提醒",
                "勿扰",
            ],
            90,
            86,
        );
        admit_marker_candidate(
            &mut facts,
            clause,
            UserProfileFactCategory::CommunicationStyle,
            &["请以后", "以后请", "希望你", "我希望", "汇报时", "回复时"],
            82,
            78,
        );
        admit_marker_candidate(
            &mut facts,
            clause,
            UserProfileFactCategory::Preference,
            &["我偏好", "偏好", "更喜欢", "优先", "最好"],
            82,
            76,
        );
    }
    dedupe_candidates(facts)
}

fn split_profile_clauses(content: &str) -> Vec<String> {
    content
        .split(['。', '！', '!', '？', '?', '\n', ';', '；'])
        .flat_map(|part| part.split('，'))
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

fn admit_marker_candidate(
    facts: &mut Vec<(UserProfileFactCategory, String, u8, u8)>,
    clause: &str,
    category: UserProfileFactCategory,
    markers: &[&str],
    confidence: u8,
    salience: u8,
) {
    if facts
        .iter()
        .any(|(existing_category, _, _, _)| *existing_category == category)
    {
        return;
    }
    let Some(value) = markers
        .iter()
        .find_map(|marker| extract_after_marker(clause, marker, category))
    else {
        return;
    };
    facts.push((category, value, confidence, salience));
}

fn extract_after_marker(
    clause: &str,
    marker: &str,
    category: UserProfileFactCategory,
) -> Option<String> {
    let (before, after) = clause.split_once(marker)?;
    if category == UserProfileFactCategory::Like && before.ends_with(['不', '没', '無', '无']) {
        return None;
    }
    let value = clean_profile_value_for_category(after, marker, category);
    if value.is_empty() { None } else { Some(value) }
}

fn clean_profile_value_for_category(
    value: &str,
    marker: &str,
    category: UserProfileFactCategory,
) -> String {
    let value = clean_profile_value(value);
    if category == UserProfileFactCategory::DoNotDisturb {
        clean_do_not_disturb_value(&value, marker)
    } else {
        value
    }
}

fn clean_do_not_disturb_value(value: &str, marker: &str) -> String {
    let trimmed = value.trim().trim_end_matches('我').trim();
    if trimmed.is_empty() {
        marker
            .trim_start_matches("不要")
            .trim_start_matches('别')
            .trim_start_matches('勿')
            .trim()
            .to_string()
    } else {
        trimmed.to_string()
    }
}

fn clean_profile_value(value: &str) -> String {
    value
        .trim()
        .trim_start_matches(['：', ':', '，', ',', '、', ' '])
        .trim_end_matches(['。', '.', '！', '!', '？', '?', '，', ',', '；', ';'])
        .trim()
        .to_string()
}

fn dedupe_candidates(
    facts: Vec<(UserProfileFactCategory, String, u8, u8)>,
) -> Vec<(UserProfileFactCategory, String, u8, u8)> {
    let mut deduped = Vec::new();
    for fact in facts {
        if !deduped.iter().any(
            |(category, value, _, _): &(UserProfileFactCategory, String, u8, u8)| {
                *category == fact.0 && value == &fact.1
            },
        ) {
            deduped.push(fact);
        }
    }
    deduped
}

fn admit_profile_fact(
    profile: &mut HumanProfile,
    category: UserProfileFactCategory,
    value: &str,
    evidence_message_id: Option<String>,
    observed_at: DateTime<Utc>,
    confidence: u8,
    salience: u8,
) -> UserProfileFact {
    let value = clean_profile_value(value);
    supersede_opposite_preference(profile, category, &value, observed_at);
    if let Some(existing) = profile.profile_facts.iter_mut().find(|fact| {
        fact.category == category
            && fact.value == value
            && fact.status == UserProfileFactStatus::Active
    }) {
        existing.confidence = existing
            .confidence
            .saturating_add(6)
            .max(confidence)
            .min(100);
        existing.salience = existing.salience.saturating_add(4).max(salience).min(100);
        existing.updated_at = observed_at;
        existing.last_seen_at = observed_at;
        if let Some(message_id) = evidence_message_id
            && !existing.evidence_message_ids.contains(&message_id)
        {
            existing.evidence_message_ids.push(message_id);
        }
        let updated = existing.clone();
        materialize_profile_fact(profile, &updated);
        return updated;
    }
    let mut evidence_message_ids = Vec::new();
    if let Some(message_id) = evidence_message_id {
        evidence_message_ids.push(message_id);
    }
    let fact = UserProfileFact {
        fact_id: profile_fact_id(&profile.user_id, category, &value),
        user_id: profile.user_id.clone(),
        category,
        value,
        confidence,
        salience,
        status: UserProfileFactStatus::Active,
        evidence_message_ids,
        created_at: observed_at,
        updated_at: observed_at,
        last_seen_at: observed_at,
    };
    profile.profile_facts.push(fact.clone());
    materialize_profile_fact(profile, &fact);
    fact
}

fn supersede_opposite_preference(
    profile: &mut HumanProfile,
    category: UserProfileFactCategory,
    value: &str,
    observed_at: DateTime<Utc>,
) {
    let opposite = match category {
        UserProfileFactCategory::Like => Some(UserProfileFactCategory::Dislike),
        UserProfileFactCategory::Dislike => Some(UserProfileFactCategory::Like),
        _ => None,
    };
    let Some(opposite) = opposite else { return };
    for fact in &mut profile.profile_facts {
        if fact.category == opposite
            && fact.value == value
            && fact.status == UserProfileFactStatus::Active
        {
            fact.status = UserProfileFactStatus::Superseded;
            fact.updated_at = observed_at;
        }
    }
}

fn materialize_profile_fact(profile: &mut HumanProfile, fact: &UserProfileFact) {
    match fact.category {
        UserProfileFactCategory::Like => push_unique(&mut profile.likes, fact.value.clone()),
        UserProfileFactCategory::Dislike => push_unique(&mut profile.dislikes, fact.value.clone()),
        UserProfileFactCategory::Capability => {
            push_unique(&mut profile.capability_labels, fact.value.clone())
        }
        UserProfileFactCategory::Preference => {
            push_unique(&mut profile.preferences, fact.value.clone())
        }
        UserProfileFactCategory::DoNotDisturb => {
            push_unique(&mut profile.do_not_disturb, fact.value.clone())
        }
        UserProfileFactCategory::CommunicationStyle => {
            push_unique(&mut profile.preferences, fact.value.clone());
            if fact.value.contains("结论")
                || fact.value.contains("风险")
                || fact.value.contains("细节")
            {
                profile.report_preference = fact.value.clone();
            }
        }
    }
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !value.is_empty() && !values.iter().any(|existing| existing == &value) {
        values.push(value);
    }
}

fn profile_fact_id(user_id: &str, category: UserProfileFactCategory, value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(user_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(category.as_str().as_bytes());
    hasher.update(b"\0");
    hasher.update(value.as_bytes());
    let digest = hasher.finalize();
    format!("profile_fact_{}", hex_prefix(&digest, 12))
}

fn hex_prefix(bytes: &[u8], len: usize) -> String {
    bytes
        .iter()
        .flat_map(|byte| [byte >> 4, byte & 0x0f])
        .take(len)
        .map(|nibble| char::from_digit(nibble as u32, 16).expect("hex nibble"))
        .collect()
}
