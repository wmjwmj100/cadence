use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BlackboardNote {
    pub id: String,
    pub author_id: String,
    pub content: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProjectState {
    pub id: String,
    pub title: String,
    pub summary: String,
    #[serde(default)]
    pub owner_agent_ids: Vec<String>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct OfficeBlackboard {
    #[serde(default)]
    pub company_goals: Vec<String>,
    #[serde(default)]
    pub active_projects: Vec<ProjectState>,
    #[serde(default)]
    pub shared_notes: Vec<BlackboardNote>,
}

#[derive(Debug, thiserror::Error)]
pub enum OfficeBlackboardError {
    #[error("failed to create office blackboard directory `{path}`: {source}")]
    CreateDir { path: PathBuf, source: io::Error },
    #[error("failed to read office blackboard `{path}`: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("failed to write office blackboard `{path}`: {source}")]
    Write { path: PathBuf, source: io::Error },
    #[error("failed to serialize office blackboard `{path}`: {source}")]
    Serialize {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("failed to parse office blackboard `{path}`: {source}")]
    Deserialize {
        path: PathBuf,
        source: serde_json::Error,
    },
}

#[derive(Clone, Debug)]
pub struct OfficeBlackboardStore {
    path: PathBuf,
    blackboard: OfficeBlackboard,
}

impl OfficeBlackboardStore {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, OfficeBlackboardError> {
        let path = path.into();
        if path.exists() {
            let raw = fs::read_to_string(&path).map_err(|source| OfficeBlackboardError::Read {
                path: path.clone(),
                source,
            })?;
            let blackboard = serde_json::from_str(&raw).map_err(|source| {
                OfficeBlackboardError::Deserialize {
                    path: path.clone(),
                    source,
                }
            })?;
            Ok(Self { path, blackboard })
        } else {
            let store = Self {
                path,
                blackboard: OfficeBlackboard::default(),
            };
            store.persist()?;
            Ok(store)
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn blackboard(&self) -> &OfficeBlackboard {
        &self.blackboard
    }

    pub fn add_note(
        &mut self,
        id: impl Into<String>,
        author_id: impl Into<String>,
        content: impl Into<String>,
    ) -> Result<BlackboardNote, OfficeBlackboardError> {
        let note = BlackboardNote {
            id: id.into(),
            author_id: author_id.into(),
            content: content.into(),
            created_at: Utc::now(),
        };
        self.blackboard.shared_notes.push(note.clone());
        self.persist()?;
        Ok(note)
    }

    pub fn reload(&mut self) -> Result<(), OfficeBlackboardError> {
        if self.path.exists() {
            let raw =
                fs::read_to_string(&self.path).map_err(|source| OfficeBlackboardError::Read {
                    path: self.path.clone(),
                    source,
                })?;
            self.blackboard = serde_json::from_str(&raw).map_err(|source| {
                OfficeBlackboardError::Deserialize {
                    path: self.path.clone(),
                    source,
                }
            })?;
        }
        Ok(())
    }

    pub fn persist(&self) -> Result<(), OfficeBlackboardError> {
        if let Some(parent) = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).map_err(|source| OfficeBlackboardError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let body = serde_json::to_string_pretty(&self.blackboard).map_err(|source| {
            OfficeBlackboardError::Serialize {
                path: self.path.clone(),
                source,
            }
        })?;
        let tmp_path = self.path.with_extension("tmp");
        fs::write(&tmp_path, body).map_err(|source| OfficeBlackboardError::Write {
            path: tmp_path.clone(),
            source,
        })?;
        fs::rename(&tmp_path, &self.path).map_err(|source| OfficeBlackboardError::Write {
            path: self.path.clone(),
            source,
        })?;
        Ok(())
    }
}
