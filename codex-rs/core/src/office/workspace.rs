use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

use super::OFFICE_AGENT_SYSTEM_PROMPT_RULES;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceArea {
    Private,
    Public,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceItemKind {
    PrivateMaterial,
    SharedMaterial,
    WorkProduct,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceItem {
    pub owner_agent_id: String,
    pub area: WorkspaceArea,
    pub kind: WorkspaceItemKind,
    pub path: PathBuf,
    pub description: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspacePolicy;

impl WorkspacePolicy {
    pub fn area_for_owner_upload(&self) -> WorkspaceArea {
        WorkspaceArea::Private
    }

    pub fn area_for_work(&self, collaborative: bool, reusable_by_team: bool) -> WorkspaceArea {
        if collaborative || reusable_by_team {
            WorkspaceArea::Public
        } else {
            WorkspaceArea::Private
        }
    }

    pub fn system_prompt_rules(&self) -> &'static str {
        OFFICE_AGENT_SYSTEM_PROMPT_RULES
    }
}

impl Default for WorkspacePolicy {
    fn default() -> Self {
        Self
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentWorkspace {
    pub agent_id: String,
    pub private_root: PathBuf,
    pub public_root: PathBuf,
    pub items: Vec<WorkspaceItem>,
}

impl AgentWorkspace {
    pub fn new(
        agent_id: impl Into<String>,
        private_root: impl Into<PathBuf>,
        public_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            agent_id: agent_id.into(),
            private_root: private_root.into(),
            public_root: public_root.into(),
            items: Vec::new(),
        }
    }

    pub fn receive_owner_upload(
        &mut self,
        relative_path: impl Into<PathBuf>,
        description: impl Into<String>,
    ) -> WorkspaceItem {
        let item = WorkspaceItem {
            owner_agent_id: self.agent_id.clone(),
            area: WorkspaceArea::Private,
            kind: WorkspaceItemKind::PrivateMaterial,
            path: self.private_root.join(relative_path.into()),
            description: description.into(),
        };
        self.items.push(item.clone());
        item
    }

    pub fn write_work_product(
        &mut self,
        relative_path: impl Into<PathBuf>,
        description: impl Into<String>,
        collaborative: bool,
        reusable_by_team: bool,
    ) -> WorkspaceItem {
        let policy = WorkspacePolicy;
        let area = policy.area_for_work(collaborative, reusable_by_team);
        let root = match area {
            WorkspaceArea::Private => &self.private_root,
            WorkspaceArea::Public => &self.public_root,
        };
        let item = WorkspaceItem {
            owner_agent_id: self.agent_id.clone(),
            area,
            kind: WorkspaceItemKind::WorkProduct,
            path: root.join(relative_path.into()),
            description: description.into(),
        };
        self.items.push(item.clone());
        item
    }

    pub fn migrate_summary_to_public(
        &mut self,
        relative_path: impl Into<PathBuf>,
        description: impl Into<String>,
    ) -> WorkspaceItem {
        let item = WorkspaceItem {
            owner_agent_id: self.agent_id.clone(),
            area: WorkspaceArea::Public,
            kind: WorkspaceItemKind::SharedMaterial,
            path: self.public_root.join(relative_path.into()),
            description: description.into(),
        };
        self.items.push(item.clone());
        item
    }
}
