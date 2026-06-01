use std::net::IpAddr;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use codex_core::AuthManager;
use codex_core::ThreadManager;
use codex_core::config::ConfigBuilder;
use codex_core::config::ConfigOverrides;
use codex_core::office::OfficeWebApp;
use codex_core::office::OfficeWebConfig;
use codex_core::protocol::AskForApproval;
use codex_core::protocol::SessionSource;
use codex_core::protocol_config_types::ModeKind;
use codex_core::protocol_config_types::SandboxMode;

#[derive(Debug, Parser)]
#[command(name = "codex-office-server")]
struct Args {
    #[arg(long, env = "AI_OFFICE_DB", default_value = "ai-office-store.json")]
    db: PathBuf,
    #[arg(long, env = "AI_OFFICE_HOST", default_value = "0.0.0.0")]
    host: IpAddr,
    #[arg(long, env = "AI_OFFICE_PORT", default_value_t = 8080)]
    port: u16,
    #[arg(
        long,
        env = "AI_OFFICE_SESSION_COOKIE",
        default_value = "codex_office_session"
    )]
    session_cookie: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let addr = SocketAddr::new(args.host, args.port);
    let mut agent_config = ConfigBuilder::default()
        .harness_overrides(ConfigOverrides {
            approval_policy: Some(AskForApproval::Never),
            sandbox_mode: Some(SandboxMode::DangerFullAccess),
            ..Default::default()
        })
        .build()
        .await?;
    agent_config.experimental_mode = Some(ModeKind::Swarm);
    let auth_manager = AuthManager::shared(
        agent_config.codex_home.clone(),
        false,
        agent_config.cli_auth_credentials_store_mode,
    );
    auth_manager.set_forced_chatgpt_workspace_id(agent_config.forced_chatgpt_workspace_id.clone());
    let thread_manager = Arc::new(ThreadManager::new(
        agent_config.codex_home.clone(),
        auth_manager,
        SessionSource::Exec,
    ));
    let app = OfficeWebApp::open_with_thread_manager(
        OfficeWebConfig::new(args.db)
            .with_session_cookie(args.session_cookie)
            .with_codex_home(agent_config.codex_home.clone()),
        thread_manager,
        agent_config,
    )?;
    app.serve(addr).await?;
    Ok(())
}
