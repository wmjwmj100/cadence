use std::net::IpAddr;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use codex_core::DockerGatewayBackend;
use codex_core::DockerGatewayConfig;
use codex_core::GatewayHttpServer;

#[derive(Debug, Parser)]
#[command(name = "codex-docker-gateway")]
struct Args {
    #[arg(long, env = "CODEX_DOCKER_GATEWAY_IMAGE")]
    image: String,
    #[arg(long, env = "CODEX_DOCKER_GATEWAY_WORKSPACE_ROOT", default_value = ".")]
    workspace_root: PathBuf,
    #[arg(long, env = "CODEX_DOCKER_GATEWAY_HOST", default_value = "127.0.0.1")]
    host: IpAddr,
    #[arg(long, env = "CODEX_DOCKER_GATEWAY_PORT", default_value_t = 8090)]
    port: u16,
    #[arg(long, env = "CODEX_DOCKER_GATEWAY_BEARER_TOKEN")]
    bearer_token: Option<String>,
    #[arg(
        long,
        env = "CODEX_DOCKER_GATEWAY_DOCKER_BINARY",
        default_value = "docker"
    )]
    docker_binary: String,
    #[arg(
        long,
        env = "CODEX_DOCKER_GATEWAY_COMPANY_ID",
        default_value = "default"
    )]
    company_id: String,
    #[arg(
        long,
        env = "CODEX_DOCKER_GATEWAY_CONTAINER_PREFIX",
        default_value = "codex-company"
    )]
    container_name_prefix: String,
    #[arg(long, env = "CODEX_DOCKER_GATEWAY_UID_GID")]
    uid_gid: Option<String>,
    #[arg(long, env = "CODEX_DOCKER_GATEWAY_MEMORY", default_value = "512m")]
    memory_limit: String,
    #[arg(long, env = "CODEX_DOCKER_GATEWAY_CPUS", default_value = "1.0")]
    cpu_limit: String,
    #[arg(long, env = "CODEX_DOCKER_GATEWAY_PIDS_LIMIT", default_value_t = 256)]
    pids_limit: u32,
    #[arg(
        long,
        env = "CODEX_DOCKER_GATEWAY_TIMEOUT_MS",
        default_value_t = 30_000
    )]
    timeout_ms: u64,
    #[arg(
        long,
        env = "CODEX_DOCKER_GATEWAY_MAX_TIMEOUT_MS",
        default_value_t = 600_000
    )]
    max_timeout_ms: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let addr = SocketAddr::new(args.host, args.port);
    let mut backend = DockerGatewayBackend::with_config(DockerGatewayConfig::new(
        args.image,
        args.workspace_root,
    ))
    .with_docker_binary(args.docker_binary)
    .with_default_company_id(args.company_id)
    .with_container_name_prefix(args.container_name_prefix)
    .with_limits(args.memory_limit, args.cpu_limit, args.pids_limit)
    .with_default_timeout(Duration::from_millis(args.timeout_ms))
    .with_max_timeout(Duration::from_millis(args.max_timeout_ms));
    backend = backend.with_uid_gid(args.uid_gid);

    let mut server = GatewayHttpServer::new(backend);
    if let Some(token) = args.bearer_token {
        server = server.with_bearer_token(token);
    }
    server.serve(addr).await?;
    Ok(())
}
