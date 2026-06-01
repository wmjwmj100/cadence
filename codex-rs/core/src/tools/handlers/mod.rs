pub mod apply_patch;
mod call;
pub(crate) mod collab;
pub(crate) mod collab_inbox;
mod dynamic;
mod grep_files;
mod js_repl;
mod list_dir;
mod mcp;
mod mcp_resource;
mod plan;
mod read_agent_status;
mod read_file;
mod request_user_input;
mod search_tool_bm25;
mod shell;
mod test_sync;
mod unified_exec;
mod view_image;
mod wait;

pub use plan::PLAN_TOOL;
use serde::Deserialize;

use crate::function_tool::FunctionCallError;
pub use apply_patch::ApplyPatchHandler;
pub use call::CallHandler;
pub use collab::CollabHandler;
pub use dynamic::DynamicToolHandler;
pub use grep_files::GrepFilesHandler;
pub use js_repl::JsReplHandler;
pub use js_repl::JsReplResetHandler;
pub use list_dir::ListDirHandler;
pub use mcp::McpHandler;
pub use mcp_resource::McpResourceHandler;
pub use plan::PlanHandler;
pub use read_agent_status::ReadAgentStatusHandler;
pub use read_file::ReadFileHandler;
pub use request_user_input::RequestUserInputHandler;
pub(crate) use request_user_input::request_user_input_tool_description;
pub(crate) use search_tool_bm25::DEFAULT_LIMIT as SEARCH_TOOL_BM25_DEFAULT_LIMIT;
pub use search_tool_bm25::SearchToolBm25Handler;
pub use shell::ShellCommandHandler;
pub use shell::ShellHandler;
pub use test_sync::TestSyncHandler;
pub use unified_exec::UnifiedExecHandler;
pub use view_image::ViewImageHandler;
pub use wait::WaitHandler;

fn parse_arguments<T>(arguments: &str) -> Result<T, FunctionCallError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_str(arguments).map_err(|err| {
        FunctionCallError::RespondToModel(format!("failed to parse function arguments: {err}"))
    })
}
