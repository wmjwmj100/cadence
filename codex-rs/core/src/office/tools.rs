use std::collections::BTreeSet;

use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GatewayDecision {
    Allowed,
    Denied { reason: String },
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolCapabilityPolicy {
    #[serde(default)]
    allow_all: bool,
    allowed_tools: BTreeSet<String>,
}

impl ToolCapabilityPolicy {
    pub fn allow_all() -> Self {
        Self {
            allow_all: true,
            allowed_tools: BTreeSet::new(),
        }
    }

    pub fn allow(mut self, tool_name: impl Into<String>) -> Self {
        self.allowed_tools.insert(tool_name.into());
        self
    }

    pub fn check(&self, tool_name: &str) -> GatewayDecision {
        if self.allow_all || self.allowed_tools.contains(tool_name) {
            GatewayDecision::Allowed
        } else {
            GatewayDecision::Denied {
                reason: format!("tool `{tool_name}` is not allowed for this Agent profile"),
            }
        }
    }

    pub fn is_unrestricted(&self) -> bool {
        self.allow_all
    }

    pub fn allowed_tools(&self) -> impl Iterator<Item = &str> {
        self.allowed_tools.iter().map(String::as_str)
    }
}
