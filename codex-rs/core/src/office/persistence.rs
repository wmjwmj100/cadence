use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;

use super::AgentOwnerBinding;
use super::AgentProfile;
use super::DailyReflection;
use super::HumanProfile;
use super::PilotAccount;
use super::PilotDirectory;
use super::PilotSession;
use super::pilot::derive_owner_bindings_from_profiles;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PilotWebSession {
    pub token: String,
    pub user_id: String,
    pub agent_id: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PilotStoreSnapshot {
    pub accounts: HashMap<String, PilotAccount>,
    pub human_profiles: HashMap<String, HumanProfile>,
    pub agent_profiles: HashMap<String, AgentProfile>,
    #[serde(default)]
    pub agent_owner_bindings: HashMap<String, AgentOwnerBinding>,
    pub web_sessions: HashMap<String, PilotWebSession>,
}

impl PilotStoreSnapshot {
    pub fn seeded() -> Self {
        let directory = PilotDirectory::six_person_seed();
        Self {
            accounts: directory.accounts().clone(),
            human_profiles: directory.human_profiles().clone(),
            agent_profiles: directory.agent_profiles().clone(),
            agent_owner_bindings: directory.agent_owner_bindings().clone(),
            web_sessions: HashMap::new(),
        }
    }

    pub fn into_directory(self) -> PilotDirectory {
        let agent_owner_bindings = if self.agent_owner_bindings.is_empty() {
            derive_owner_bindings_from_profiles(&self.agent_profiles)
        } else {
            self.agent_owner_bindings
        };
        PilotDirectory::from_parts(
            self.accounts,
            self.human_profiles,
            self.agent_profiles,
            agent_owner_bindings,
        )
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PilotStoreError {
    #[error("failed to create pilot store directory `{path}`: {source}")]
    CreateDir { path: PathBuf, source: io::Error },
    #[error("failed to read pilot store `{path}`: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("failed to write pilot store `{path}`: {source}")]
    Write { path: PathBuf, source: io::Error },
    #[error("failed to serialize pilot store `{path}`: {source}")]
    Serialize {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("failed to parse pilot store `{path}`: {source}")]
    Deserialize {
        path: PathBuf,
        source: serde_json::Error,
    },
}

#[derive(Clone, Debug)]
pub struct PersistentPilotDirectory {
    path: PathBuf,
    directory: PilotDirectory,
    web_sessions: HashMap<String, PilotWebSession>,
}

impl PersistentPilotDirectory {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, PilotStoreError> {
        let path = path.into();
        if path.exists() {
            let raw = fs::read_to_string(&path).map_err(|source| PilotStoreError::Read {
                path: path.clone(),
                source,
            })?;
            let snapshot: PilotStoreSnapshot =
                serde_json::from_str(&raw).map_err(|source| PilotStoreError::Deserialize {
                    path: path.clone(),
                    source,
                })?;
            let agent_owner_bindings = if snapshot.agent_owner_bindings.is_empty() {
                derive_owner_bindings_from_profiles(&snapshot.agent_profiles)
            } else {
                snapshot.agent_owner_bindings
            };
            Ok(Self {
                path,
                directory: PilotDirectory::from_parts(
                    snapshot.accounts,
                    snapshot.human_profiles,
                    snapshot.agent_profiles,
                    agent_owner_bindings,
                ),
                web_sessions: snapshot.web_sessions,
            })
        } else {
            let store = Self {
                path,
                directory: PilotDirectory::six_person_seed(),
                web_sessions: HashMap::new(),
            };
            store.persist()?;
            Ok(store)
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn directory(&self) -> &PilotDirectory {
        &self.directory
    }

    pub fn account_count(&self) -> usize {
        self.directory.account_count()
    }

    pub fn login(&self, username: &str, password: &str) -> Option<PilotSession> {
        self.directory.login(username, password)
    }

    pub fn human_profile(&self, user_id: &str) -> Option<&HumanProfile> {
        self.directory.human_profile(user_id)
    }

    pub fn agent_profile(&self, agent_id: &str) -> Option<&AgentProfile> {
        self.directory.agent_profile(agent_id)
    }

    pub fn active_owner_binding(&self, agent_id: &str) -> Option<&AgentOwnerBinding> {
        self.directory.active_owner_binding(agent_id)
    }

    pub fn account_for_user(&self, user_id: &str) -> Option<&PilotAccount> {
        self.directory.account_for_user(user_id)
    }

    pub fn run_mini_interview(
        &mut self,
        user_id: &str,
        role: impl Into<String>,
        capabilities: impl IntoIterator<Item = impl Into<String>>,
        avoid: impl IntoIterator<Item = impl Into<String>>,
        report_preference: impl Into<String>,
    ) -> Result<Option<HumanProfile>, PilotStoreError> {
        let profile = self
            .directory
            .run_mini_interview(user_id, role, capabilities, avoid, report_preference)
            .cloned();
        self.persist()?;
        Ok(profile)
    }

    pub fn daily_reflection(
        &mut self,
        user_id: &str,
        profile_updates: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<Option<DailyReflection>, PilotStoreError> {
        let reflection = self.directory.daily_reflection(user_id, profile_updates);
        self.persist()?;
        Ok(reflection)
    }

    pub fn record_owner_message_profile_evidence(
        &mut self,
        user_id: &str,
        agent_id: &str,
        message_id: &str,
        content: &str,
    ) -> Result<Option<Vec<super::pilot::UserProfileFact>>, PilotStoreError> {
        let admitted = self
            .directory
            .record_owner_message_profile_evidence(user_id, agent_id, message_id, content);
        self.persist()?;
        Ok(admitted)
    }

    pub fn create_web_session(
        &mut self,
        token: String,
        session: PilotSession,
    ) -> Result<PilotWebSession, PilotStoreError> {
        let web_session = PilotWebSession {
            token: token.clone(),
            user_id: session.user_id,
            agent_id: session.agent_id,
            created_at: Utc::now(),
        };
        self.web_sessions.insert(token, web_session.clone());
        self.persist()?;
        Ok(web_session)
    }

    pub fn web_session(&self, token: &str) -> Option<&PilotWebSession> {
        self.web_sessions.get(token)
    }

    pub fn delete_web_session(&mut self, token: &str) -> Result<bool, PilotStoreError> {
        let deleted = self.web_sessions.remove(token).is_some();
        self.persist()?;
        Ok(deleted)
    }

    pub fn snapshot(&self) -> PilotStoreSnapshot {
        PilotStoreSnapshot {
            accounts: self.directory.accounts().clone(),
            human_profiles: self.directory.human_profiles().clone(),
            agent_profiles: self.directory.agent_profiles().clone(),
            agent_owner_bindings: self.directory.agent_owner_bindings().clone(),
            web_sessions: self.web_sessions.clone(),
        }
    }

    pub fn persist(&self) -> Result<(), PilotStoreError> {
        if let Some(parent) = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).map_err(|source| PilotStoreError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let body = serde_json::to_string_pretty(&self.snapshot()).map_err(|source| {
            PilotStoreError::Serialize {
                path: self.path.clone(),
                source,
            }
        })?;
        let tmp_path = self.path.with_extension("tmp");
        fs::write(&tmp_path, body).map_err(|source| PilotStoreError::Write {
            path: tmp_path.clone(),
            source,
        })?;
        fs::rename(&tmp_path, &self.path).map_err(|source| PilotStoreError::Write {
            path: self.path.clone(),
            source,
        })?;
        Ok(())
    }
}
