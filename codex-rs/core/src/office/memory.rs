use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use chrono::NaiveDate;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentMemoryPaths {
    pub root: PathBuf,
    pub identity: PathBuf,
    pub memory: PathBuf,
    pub owner_profile: PathBuf,
    pub peer_notes: PathBuf,
    pub daily_reflections: PathBuf,
}

impl AgentMemoryPaths {
    pub fn new(root: impl Into<PathBuf>, agent_id: &str) -> Self {
        let root = root.into().join("agents").join(agent_id);
        Self {
            identity: root.join("identity.md"),
            memory: root.join("memory.md"),
            owner_profile: root.join("owner_profile.md"),
            peer_notes: root.join("peer_notes.md"),
            daily_reflections: root.join("daily_reflections"),
            root,
        }
    }

    pub fn daily_reflection(&self, date: NaiveDate) -> PathBuf {
        self.daily_reflections.join(format!("{date}.md"))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AgentMemoryError {
    #[error("failed to create agent memory directory `{path}`: {source}")]
    CreateDir { path: PathBuf, source: io::Error },
    #[error("failed to read agent memory file `{path}`: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("failed to write agent memory file `{path}`: {source}")]
    Write { path: PathBuf, source: io::Error },
}

#[derive(Clone, Debug)]
pub struct AgentMemoryStore {
    root: PathBuf,
}

impl AgentMemoryStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn paths_for(&self, agent_id: &str) -> AgentMemoryPaths {
        AgentMemoryPaths::new(self.root.clone(), agent_id)
    }

    pub fn ensure_agent_files(
        &self,
        agent_id: &str,
        owner_user_id: &str,
    ) -> Result<AgentMemoryPaths, AgentMemoryError> {
        let paths = self.paths_for(agent_id);
        fs::create_dir_all(&paths.daily_reflections).map_err(|source| {
            AgentMemoryError::CreateDir {
                path: paths.daily_reflections.clone(),
                source,
            }
        })?;
        write_file(
            &paths.identity,
            format!("# Identity\n\nagent_id: {agent_id}\nowner_user_id: {owner_user_id}\n"),
        )?;
        write_if_missing(&paths.memory, "# Long-Term Memory\n\n".to_string())?;
        write_if_missing(&paths.owner_profile, "# Owner Profile\n\n".to_string())?;
        write_if_missing(&paths.peer_notes, "# Peer Notes\n\n".to_string())?;
        Ok(paths)
    }

    pub fn write_owner_profile_text(
        &self,
        agent_id: &str,
        owner_user_id: &str,
        content: &str,
    ) -> Result<AgentMemoryPaths, AgentMemoryError> {
        let paths = self.ensure_agent_files(agent_id, owner_user_id)?;
        write_file(&paths.owner_profile, ensure_trailing_newline(content))?;
        Ok(paths)
    }

    pub fn read_prompt_memory(&self, agent_id: &str) -> Result<String, AgentMemoryError> {
        let paths = self.paths_for(agent_id);
        let mut sections = Vec::new();
        for path in [
            paths.identity,
            paths.owner_profile,
            paths.memory,
            paths.peer_notes,
        ] {
            if path.exists() {
                sections.push(read_file(&path)?);
            }
        }
        Ok(sections.join("\n\n"))
    }
}

fn write_if_missing(path: &Path, content: String) -> Result<(), AgentMemoryError> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| AgentMemoryError::CreateDir {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    fs::write(path, content).map_err(|source| AgentMemoryError::Write {
        path: path.to_path_buf(),
        source,
    })
}

fn write_file(path: &Path, content: String) -> Result<(), AgentMemoryError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| AgentMemoryError::CreateDir {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    fs::write(path, content).map_err(|source| AgentMemoryError::Write {
        path: path.to_path_buf(),
        source,
    })
}

fn read_file(path: &Path) -> Result<String, AgentMemoryError> {
    fs::read_to_string(path).map_err(|source| AgentMemoryError::Read {
        path: path.to_path_buf(),
        source,
    })
}

fn ensure_trailing_newline(content: &str) -> String {
    let mut content = content.to_string();
    if !content.ends_with('\n') {
        content.push('\n');
    }
    content
}
