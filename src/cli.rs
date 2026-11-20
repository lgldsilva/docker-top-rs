//! CLI (clap derive). Highest-precedence overrides over TOML/env.

use clap::Parser;

#[derive(Debug, Parser, Clone)]
#[command(
    name = "docker-top-rs",
    version,
    about = "Interactive multi-context Docker container resource monitor (Rust)"
)]
pub struct Cli {
    /// Refresh interval in seconds (0.5..=10).
    #[arg(short, long, env = "DOCKER_TOP_DELAY")]
    pub delay: Option<f64>,
    /// Initial sort column.
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
    /// Ascending sort (default: descending).
    #[arg(short, long, env = "DOCKER_TOP_ASC")]
    pub asc: bool,
    /// Initial name filter.
    #[arg(short, long, env = "DOCKER_TOP_FILTER")]
    pub filter: Option<String>,
    /// Context (`all` or name).
    #[arg(short, long, env = "DOCKER_TOP_CONTEXT")]
    pub context: Option<String>,
    /// Run once and exit (non-interactive mode).
    #[arg(long)]
    pub once: bool,
    /// Alternative TOML config path.
    #[arg(long, env = "DOCKER_TOP_CONFIG")]
    pub config: Option<std::path::PathBuf>,
    /// Log file (append). E.g. `--log ~/.local/state/docker-top/log.txt`.
    #[arg(long, env = "DOCKER_TOP_LOG")]
    pub log: Option<std::path::PathBuf>,
    /// Reach `ssh://` contexts via plain `ssh <host> docker ...` (BatchMode/ConnectTimeout).
    #[arg(long, env = "DOCKER_TOP_SSH_DIRECT")]
    pub ssh_direct: bool,
    /// Probe `~/.ssh/config` hosts with `docker version` and add them as contexts.
    #[arg(long, env = "DOCKER_TOP_SSH_HOSTS")]
    pub ssh_hosts: bool,
    /// Generate the versioned `config.example.toml` and exit.
    #[arg(long)]
    pub generate_config_example: bool,
}
