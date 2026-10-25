//! CLI (`clap derive`). Overrides de maior precedência sobre TOML/env.

use clap::Parser;

#[derive(Debug, Parser, Clone)]
#[command(
    name = "docker-top-rs",
    version,
    about = "Monitor interativo de recursos de containers Docker (Rust)"
)]
pub struct Cli {
    /// Intervalo de atualização em segundos (0.5..=10).
    #[arg(short, long, env = "DOCKER_TOP_DELAY")]
    pub delay: Option<f64>,
    /// Coluna inicial de ordenação.
    #[arg(
        short,
        long,
        env = "DOCKER_TOP_SORT",
        value_parser = [
            "id","context","name","cpu_rel","mem_rel","cpu","mem_used",
            "cpu_lim","mem_lim","net_rx","net_tx","net",
            "block_read","block_write","block","pids",
        ]
    )]
    pub sort: Option<String>,
    /// Ordenação ascendente (padrão: descendente).
    #[arg(short, long, env = "DOCKER_TOP_ASC")]
    pub asc: bool,
    /// Filtro inicial de nome.
    #[arg(short, long, env = "DOCKER_TOP_FILTER")]
    pub filter: Option<String>,
    /// Contexto (`all` ou nome).
    #[arg(short, long, env = "DOCKER_TOP_CONTEXT")]
    pub context: Option<String>,
    /// Executa uma vez e sai (modo não-interativo).
    #[arg(long)]
    pub once: bool,
    /// Caminho alternativo do config TOML.
    #[arg(long, env = "DOCKER_TOP_CONFIG")]
    pub config: Option<std::path::PathBuf>,
    /// Arquivo de log (append). Ex.: `--log ~/.local/state/docker-top/log.txt`.
    #[arg(long, env = "DOCKER_TOP_LOG")]
    pub log: Option<std::path::PathBuf>,
    /// Contextos `ssh://` via `ssh <host> docker ...` direto (BatchMode/ConnectTimeout).
    #[arg(long, env = "DOCKER_TOP_SSH_DIRECT")]
    pub ssh_direct: bool,
    /// Sonda hosts de `~/.ssh/config` com `docker version` e adiciona como contextos.
    #[arg(long, env = "DOCKER_TOP_SSH_HOSTS")]
    pub ssh_hosts: bool,
    /// Gera `config.example.toml` versionado e sai.
    #[arg(long)]
    pub generate_config_example: bool,
}
