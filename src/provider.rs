//! `DockerProvider` trait (Bollard / CLI / Mock) + caches.
//!
//! - `BollardProvider`: `unix://` via Bollard (rustls, sem openssl).
//! - `CliProvider`: `ssh://` via `tokio::process docker --context X ...`.
//! - `MockProvider`: testes / snapshot (`insta::TestBackend`).
//!
//! Cache `HashMap<(ctx, short_id), ContainerLimits>` com evict quando o id some,
//! mais cache `SystemInfo` por contexto.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context as _;

use crate::model::{short_id, ContainerLimits, ContainerSnapshot};

/// Linha crua de stats (já com valores numéricos quando via Bollard).
#[derive(Debug, Clone)]
pub struct RawStats {
    pub id: String,
    pub name: String,
    pub raw_cpu: String,
    pub raw_mem_usage: String,
    pub raw_mem_perc: String,
    pub raw_net: String,
    pub raw_block: String,
    pub raw_pids: String,
    pub cpu: Option<f64>,
    pub mem_used: Option<f64>,
    pub mem_limit_stats: Option<f64>,
    pub net_rx: Option<f64>,
    pub net_tx: Option<f64>,
    pub block_read: Option<f64>,
    pub block_write: Option<f64>,
    pub pids: Option<u32>,
}

impl RawStats {
    pub fn from_strings(parts: [String; 8]) -> Self {
        Self {
            id: parts[0].clone(),
            name: parts[1].clone(),
            raw_cpu: parts[2].clone(),
            raw_mem_usage: parts[3].clone(),
            raw_mem_perc: parts[4].clone(),
            raw_net: parts[5].clone(),
            raw_block: parts[6].clone(),
            raw_pids: parts[7].clone(),
            cpu: None,
            mem_used: None,
            mem_limit_stats: None,
            net_rx: None,
            net_tx: None,
            block_read: None,
            block_write: None,
            pids: None,
        }
    }
}

#[async_trait::async_trait]
pub trait DockerProvider: Send + Sync {
    fn context_name(&self) -> &str;
    /// Badge `api` (Bollard) ou `cli` (subprocesso).
    fn transport_label(&self) -> &'static str;
    async fn list_stats(&self) -> anyhow::Result<Vec<RawStats>>;
    async fn inspect_limits(
        &self,
        ids: &[String],
    ) -> anyhow::Result<Vec<(String, ContainerLimits)>>;
    async fn system_info(&self) -> anyhow::Result<(u32, u64)>;
}

// ---------------------------------------------------------------------------
// Cache de limites + system info
// ---------------------------------------------------------------------------

/// Cache `(ctx, short_id) -> limites` com evict; + `(cores, mem)` por contexto.
/// `in_flight`: inspects em background (TUI) p/ não duplicar chamadas entre ticks.
#[derive(Debug, Default)]
pub struct Caches {
    pub limits: HashMap<(String, String), ContainerLimits>,
    pub sysinfo: HashMap<String, (u32, u64)>,
    pub sysinfo_ts: HashMap<String, std::time::Instant>,
    pub in_flight: HashSet<(String, String)>,
}

pub const SYSINFO_TTL: std::time::Duration = std::time::Duration::from_secs(60);
pub const FALLBACK_CORES: u32 = 1;
pub const FALLBACK_MEM: u64 = 16 << 30;

/// Tamanho da fatia de `inspect` em paralelo (CliProvider): lote único de
/// ~57 ids via ssh mediu 25s+ em host lento (Pi); fatias dividem a latência.
const INSPECT_CHUNK: usize = 12;

impl Caches {
    /// Remove entradas cujos ids sumiram no último ciclo.
    pub fn evict_missing(&mut self, ctx: &str, seen: &HashSet<String>) {
        self.limits
            .retain(|(c, short), _| c != ctx || seen.contains(short));
    }
}

pub type SharedCaches = Arc<Mutex<Caches>>;

// ---------------------------------------------------------------------------
// CliProvider — ssh:// (lab-node1, cloud-arm1/2) e fallback geral
// ---------------------------------------------------------------------------

pub struct CliProvider {
    context: String,
    bin: PathBuf,
    /// Modo SSH direto (`--ssh-direct`): `ssh [user@]host docker ...` em vez de
    /// `docker --context` (BatchMode/ConnectTimeout, sem depender de context).
    ssh_host: Option<String>,
    /// Porta ssh do modo direto (`ssh://user@host:port`).
    ssh_port: Option<u16>,
    /// Binário ssh do modo direto (fake nos testes).
    ssh_bin: PathBuf,
    /// Cache TTL de `system_info`: o collector e o `inspect_limits` pedem o
    /// mesmo (cores, mem) no mesmo tick — sem cache seriam 2 chamadas ssh.
    sysinfo_cache: tokio::sync::Mutex<Option<(std::time::Instant, (u32, u64))>>,
    stats_timeout: Duration,
    inspect_timeout: Duration,
    info_timeout: Duration,
}

impl CliProvider {
    pub fn new(context: String, stats: Duration, inspect: Duration, info: Duration) -> Self {
        Self {
            context,
            bin: PathBuf::from("docker"),
            ssh_host: None,
            ssh_port: None,
            ssh_bin: PathBuf::from("ssh"),
            sysinfo_cache: tokio::sync::Mutex::new(None),
            stats_timeout: stats,
            inspect_timeout: inspect,
            info_timeout: info,
        }
    }

    /// Binário alternativo (testes com `docker` fake — sem PATH global).
    pub fn with_bin(mut self, bin: PathBuf) -> Self {
        self.bin = bin;
        self
    }

    /// Habilita o modo SSH direto contra `[user@]host` (endpoint `ssh://`).
    pub fn with_ssh_host(mut self, host: Option<String>) -> Self {
        self.ssh_host = host;
        self
    }

    /// Porta ssh do modo direto (endpoint `ssh://user@host:port`).
    pub fn with_ssh_port(mut self, port: Option<u16>) -> Self {
        self.ssh_port = port;
        self
    }

    /// Binário ssh alternativo p/ o modo direto (testes com fake).
    pub fn with_ssh_bin(mut self, bin: PathBuf) -> Self {
        self.ssh_bin = bin;
        self
    }

    /// Descrição do comando p/ mensagens de erro.
    fn describe(&self, args: &[&str]) -> String {
        let args = self.strip_local_context(args);
        match &self.ssh_host {
            Some(host) => format!("ssh {} docker {}", host, args.join(" ")),
            None => format!("{} {}", self.bin.display(), args.join(" ")),
        }
    }

    /// `--context X` é um conceito do docker LOCAL — no modo SSH direto o
    /// comando roda no host remoto e não pode levar o par.
    fn strip_local_context<'a>(&self, args: &'a [&'a str]) -> &'a [&'a str] {
        if self.ssh_host.is_some() && args.len() >= 2 && args[0] == "--context" {
            &args[2..]
        } else {
            args
        }
    }

    async fn run_docker(&self, args: &[&str], timeout: Duration) -> anyhow::Result<String> {
        let args = self.strip_local_context(args);
        // No modo ssh os args docker vão DENTRO do comando remoto único
        // (quotados); acrescentá-los de novo como argv do ssh os concatena ao
        // comando e o shell remoto os trata como nomes de container.
        let mut cmd = match &self.ssh_host {
            Some(host) => {
                let mut c = tokio::process::Command::new(&self.ssh_bin);
                c.args(crate::transport::ssh_base_args(host, self.ssh_port));
                c.arg(crate::transport::ssh_remote_command(args));
                c
            }
            None => {
                let mut c = tokio::process::Command::new(&self.bin);
                c.args(args);
                c
            }
        };
        // CRÍTICO: tokio output() só pipa stdout/stderr — sem isso o stdin
        // herda o TTY e o ssh drena a fila de input, engolindo as teclas
        // do usuário antes do crossterm ler (setas de ordenação "sumiam").
        cmd.stdin(std::process::Stdio::null());
        cmd.kill_on_drop(true);
        let out = tokio::time::timeout(timeout, cmd.output())
            .await
            .map_err(|_| {
                anyhow::anyhow!("timeout {}s: {}", timeout.as_secs(), self.describe(args))
            })?
            .with_context(|| format!("spawn {}", self.describe(args)))?;
        if !out.status.success() {
            anyhow::bail!(
                "{} falhou: {}",
                self.describe(args),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }
}

#[async_trait::async_trait]
impl DockerProvider for CliProvider {
    fn context_name(&self) -> &str {
        &self.context
    }
    fn transport_label(&self) -> &'static str {
        "cli"
    }

    async fn list_stats(&self) -> anyhow::Result<Vec<RawStats>> {
        let fmt = "{{.ID}}\t{{.Name}}\t{{.CPUPerc}}\t{{.MemUsage}}\t{{.MemPerc}}\t{{.NetIO}}\t{{.BlockIO}}\t{{.PIDs}}";
        let stdout = self
            .run_docker(
                &[
                    "--context",
                    &self.context,
                    "stats",
                    "--no-stream",
                    "--format",
                    fmt,
                ],
                self.stats_timeout,
            )
            .await?;
        let mut rows = Vec::new();
        for line in stdout.lines() {
            let line = crate::parsers::strip_ansi(line);
            if line.trim().is_empty() {
                continue;
            }
            if let Some(parts) = crate::parsers::parse_stats_line(&line) {
                rows.push(RawStats::from_strings(parts));
            }
        }
        Ok(rows)
    }

    async fn inspect_limits(
        &self,
        ids: &[String],
    ) -> anyhow::Result<Vec<(String, ContainerLimits)>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let (cores, mem) = self
            .system_info()
            .await
            .unwrap_or((FALLBACK_CORES, FALLBACK_MEM));
        let fmt = "{{.Id}}\t{{.HostConfig.NanoCpus}}\t{{.HostConfig.CpuQuota}}\t{{.HostConfig.CpuPeriod}}\t{{.HostConfig.CpusetCpus}}\t{{.HostConfig.Memory}}";
        // Lote único de ~57 ids via ssh mediu 25s+ (daemon remoto serializa os
        // inspects); fatias paralelas dividem a latência. Fatia que falha cai
        // no fallback per-id (container pode ter morrido) — igual ao Python.
        let chunks: Vec<&[String]> = ids.chunks(INSPECT_CHUNK).collect();
        let futs = chunks.into_iter().map(|chunk| {
            let mut args: Vec<String> = vec![
                "--context".into(),
                self.context.clone(),
                "inspect".into(),
                "--format".into(),
                fmt.into(),
            ];
            args.extend(chunk.iter().cloned());
            async move {
                let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
                (
                    chunk.to_vec(),
                    self.run_docker(&arg_refs, self.inspect_timeout).await,
                )
            }
        });
        let results = futures_util::future::join_all(futs).await;
        let mut acc = String::new();
        for (chunk, res) in results {
            match res {
                Ok(s) => {
                    acc.push_str(&s);
                    acc.push('\n');
                }
                Err(_) => {
                    for id in &chunk {
                        let one = vec![
                            "--context",
                            self.context.as_str(),
                            "inspect",
                            "--format",
                            fmt,
                            id.as_str(),
                        ];
                        if let Ok(s) = self.run_docker(&one, self.inspect_timeout).await {
                            acc.push_str(&s);
                            acc.push('\n');
                        }
                    }
                }
            }
        }
        let mut out = Vec::new();
        for line in acc.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Some((full_id, nano, quota, period, cpuset, mem_l)) =
                crate::parsers::parse_inspect_line(line)
            {
                out.push((
                    short_id(&full_id).to_string(),
                    ContainerLimits::from_inspect(nano, quota, period, &cpuset, mem_l, cores, mem),
                ));
            }
        }
        Ok(out)
    }

    async fn system_info(&self) -> anyhow::Result<(u32, u64)> {
        {
            let cached = self.sysinfo_cache.lock().await;
            if let Some((ts, v)) = *cached {
                if ts.elapsed() < SYSINFO_TTL {
                    return Ok(v);
                }
            }
        }
        let stdout = self
            .run_docker(
                &[
                    "--context",
                    &self.context,
                    "info",
                    "--format",
                    "{{.NCPU}}\t{{.MemTotal}}",
                ],
                self.info_timeout,
            )
            .await?;
        let parts: Vec<&str> = stdout.trim().split('\t').collect();
        let parsed = parts.first().zip(parts.get(1)).and_then(|(c, m)| {
            match (c.trim().parse::<u32>(), m.trim().parse::<u64>()) {
                (Ok(c), Ok(m)) => Some((c.max(1), m)),
                _ => None,
            }
        });
        if let Some(v) = parsed {
            *self.sysinfo_cache.lock().await = Some((std::time::Instant::now(), v));
            return Ok(v);
        }
        anyhow::bail!("info parse falhou p/ {}", self.context)
    }
}

// ---------------------------------------------------------------------------
// BollardProvider — unix:// local via API (rustls)
// ---------------------------------------------------------------------------

pub struct BollardProvider {
    context: String,
    #[allow(dead_code)]
    socket: String,
    docker: Option<bollard::Docker>,
    info_timeout: Duration,
    stats_timeout: Duration,
    inspect_timeout: Duration,
}

impl BollardProvider {
    pub fn new(context: String, socket: String, info_timeout: Duration) -> Self {
        Self::new_full(context, socket, info_timeout, info_timeout, info_timeout)
    }

    pub fn new_full(
        context: String,
        socket: String,
        info_timeout: Duration,
        stats_timeout: Duration,
        inspect_timeout: Duration,
    ) -> Self {
        let path = socket
            .strip_prefix("unix://")
            .unwrap_or(&socket)
            .to_string();
        // Sem panic: sem socket (ex. Mac sem Docker Desktop) vira erro tratável no collect.
        let docker =
            bollard::Docker::connect_with_socket(&path, 10, bollard::API_DEFAULT_VERSION).ok();
        Self {
            context,
            socket,
            docker,
            info_timeout,
            stats_timeout,
            inspect_timeout,
        }
    }

    fn connect(&self) -> anyhow::Result<bollard::Docker> {
        self.docker
            .clone()
            .ok_or_else(|| anyhow::anyhow!("sem socket docker em {}", self.socket))
    }
}

#[async_trait::async_trait]
impl DockerProvider for BollardProvider {
    fn context_name(&self) -> &str {
        &self.context
    }
    fn transport_label(&self) -> &'static str {
        "api"
    }

    async fn system_info(&self) -> anyhow::Result<(u32, u64)> {
        let docker = self.connect()?;
        let fut = docker.info();
        let info = tokio::time::timeout(self.info_timeout, fut)
            .await
            .map_err(|_| anyhow::anyhow!("timeout info {}", self.context))??;
        Ok((
            info.ncpu.unwrap_or(FALLBACK_CORES as i64) as u32,
            info.mem_total.unwrap_or(FALLBACK_MEM as i64) as u64,
        ))
    }

    async fn list_stats(&self) -> anyhow::Result<Vec<RawStats>> {
        use bollard::query_parameters::ListContainersOptionsBuilder;
        use futures_util::StreamExt;
        let docker = self.connect()?;
        let containers = tokio::time::timeout(
            self.info_timeout,
            docker.list_containers(Some(
                ListContainersOptionsBuilder::default().all(false).build(),
            )),
        )
        .await
        .map_err(|_| anyhow::anyhow!("timeout list {}", self.context))??;
        // stats one-shot em paralelo (FuturesUnordered, limite 8) — evita N×timeout sequencial.
        let mut set = futures_util::stream::FuturesUnordered::new();
        for c in containers.into_iter().take(128) {
            let Some(id) = c.id else { continue };
            let name = c
                .names
                .as_ref()
                .and_then(|n| n.first())
                .map(|n| n.trim_start_matches('/').to_string())
                .unwrap_or_else(|| short_id(&id).to_string());
            let d = docker.clone();
            let t = self.stats_timeout;
            set.push(async move {
                let opts = bollard::query_parameters::StatsOptionsBuilder::default()
                    .stream(false)
                    .one_shot(true)
                    .build();
                let mut stream = d.stats(&id, Some(opts));
                let Ok(Some(Ok(s))) = tokio::time::timeout(t, stream.next()).await else {
                    return None;
                };
                Some((id, name, s))
            });
        }
        let mut rows = Vec::new();
        while let Some(item) = futures_util::StreamExt::next(&mut set).await {
            let Some((id, name, s)) = item else { continue };
            let (cpu_perc, mem_used, mem_limit, net_rx, net_tx, blk_r, blk_w, pids) =
                bollard_sample_to_numbers(&s);
            rows.push(RawStats {
                id: id.clone(),
                name,
                raw_cpu: format!("{cpu_perc:.2}%"),
                raw_mem_usage: format!(
                    "{} / {}",
                    crate::parsers::format_bytes_human(mem_used, 2),
                    crate::parsers::format_bytes_human(mem_limit, 2)
                ),
                raw_mem_perc: format!(
                    "{:.2}%",
                    if mem_limit > 0.0 {
                        mem_used / mem_limit * 100.0
                    } else {
                        0.0
                    }
                ),
                raw_net: format!(
                    "{} / {}",
                    crate::parsers::format_bytes_human(net_rx, 2),
                    crate::parsers::format_bytes_human(net_tx, 2)
                ),
                raw_block: format!(
                    "{} / {}",
                    crate::parsers::format_bytes_human(blk_r, 2),
                    crate::parsers::format_bytes_human(blk_w, 2)
                ),
                raw_pids: pids.to_string(),
                cpu: Some(cpu_perc),
                mem_used: Some(mem_used),
                mem_limit_stats: Some(mem_limit),
                net_rx: Some(net_rx),
                net_tx: Some(net_tx),
                block_read: Some(blk_r),
                block_write: Some(blk_w),
                pids: Some(pids),
            });
        }
        Ok(rows)
    }

    async fn inspect_limits(
        &self,
        ids: &[String],
    ) -> anyhow::Result<Vec<(String, ContainerLimits)>> {
        let docker = self.connect()?;
        let (cores, mem) = self
            .system_info()
            .await
            .unwrap_or((FALLBACK_CORES, FALLBACK_MEM));
        let mut out = Vec::new();
        for id in ids {
            let fut = docker.inspect_container(
                id,
                None::<bollard::query_parameters::InspectContainerOptions>,
            );
            let Ok(info) = tokio::time::timeout(self.inspect_timeout, fut)
                .await
                .map_err(|_| anyhow::anyhow!("timeout inspect {}", id))
                .and_then(|r| r.map_err(|e| anyhow::anyhow!(e)))
            else {
                continue;
            };
            let hc = info.host_config.unwrap_or_default();
            // bollard stubs: NanoCpus pode ser Option<i64>
            let nano = hc.nano_cpus.unwrap_or(0);
            let quota = hc.cpu_quota.unwrap_or(0);
            let period = hc.cpu_period.unwrap_or(0);
            let cpuset = hc.cpuset_cpus.unwrap_or_default();
            let mem_l = hc.memory.unwrap_or(0);
            out.push((
                short_id(id).to_string(),
                ContainerLimits::from_inspect(nano, quota, period, &cpuset, mem_l, cores, mem),
            ));
        }
        Ok(out)
    }
}

fn bollard_sample_to_numbers(
    s: &bollard::models::ContainerStatsResponse,
) -> (f64, f64, f64, f64, f64, f64, f64, u32) {
    let cpu = s.cpu_stats.as_ref();
    let pre = s.precpu_stats.as_ref();
    let cpu_total = cpu
        .and_then(|c| c.cpu_usage.as_ref())
        .and_then(|u| u.total_usage)
        .unwrap_or(0);
    let cpu_prev = pre
        .and_then(|c| c.cpu_usage.as_ref())
        .and_then(|u| u.total_usage)
        .unwrap_or(0);
    let sys_total = cpu.and_then(|c| c.system_cpu_usage).unwrap_or(0);
    let sys_prev = pre.and_then(|c| c.system_cpu_usage).unwrap_or(0);
    let online = cpu.and_then(|c| c.online_cpus).unwrap_or(1) as f64;
    let cpu_delta = cpu_total.saturating_sub(cpu_prev) as f64;
    let sys_delta = sys_total.saturating_sub(sys_prev) as f64;
    let cpu_perc = if sys_delta > 0.0 {
        cpu_delta / sys_delta * online * 100.0
    } else {
        0.0
    };
    let mem = s.memory_stats.as_ref();
    let mem_used = mem.and_then(|m| m.usage).unwrap_or(0) as f64;
    let mem_limit = mem.and_then(|m| m.limit).unwrap_or(0) as f64;
    let mut rx = 0u64;
    let mut tx = 0u64;
    if let Some(nets) = &s.networks {
        for v in nets.values() {
            rx += v.rx_bytes.unwrap_or(0);
            tx += v.tx_bytes.unwrap_or(0);
        }
    }
    let mut r = 0u64;
    let mut w = 0u64;
    if let Some(blk) = s
        .blkio_stats
        .as_ref()
        .and_then(|b| b.io_service_bytes_recursive.as_ref())
    {
        for e in blk.iter() {
            match e.op.as_deref() {
                Some("read") => r += e.value.unwrap_or(0),
                Some("write") => w += e.value.unwrap_or(0),
                _ => {}
            }
        }
    }
    let pids = s.pids_stats.as_ref().and_then(|p| p.current).unwrap_or(0) as u32;
    (
        cpu_perc, mem_used, mem_limit, rx as f64, tx as f64, r as f64, w as f64, pids,
    )
}

// ---------------------------------------------------------------------------
// MockProvider — testes / snapshot
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct MockProvider {
    pub context: String,
    pub rows: Vec<RawStats>,
    pub limits: HashMap<String, ContainerLimits>,
    pub sysinfo: (u32, u64),
}

impl MockProvider {
    pub fn new(context: &str, rows: Vec<RawStats>) -> Self {
        Self {
            context: context.to_string(),
            rows,
            limits: HashMap::new(),
            sysinfo: (4, 8 * 1024 * 1024 * 1024),
        }
    }
}

#[async_trait::async_trait]
impl DockerProvider for MockProvider {
    fn context_name(&self) -> &str {
        &self.context
    }
    fn transport_label(&self) -> &'static str {
        "mock"
    }
    async fn list_stats(&self) -> anyhow::Result<Vec<RawStats>> {
        Ok(self.rows.clone())
    }
    async fn inspect_limits(
        &self,
        _ids: &[String],
    ) -> anyhow::Result<Vec<(String, ContainerLimits)>> {
        Ok(self
            .limits
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect())
    }
    async fn system_info(&self) -> anyhow::Result<(u32, u64)> {
        Ok(self.sysinfo)
    }
}

/// Constrói um snapshot enriquecido a partir do `RawStats` + caches.
pub fn build_snapshot(
    raw: &RawStats,
    context: &str,
    transport: &str,
    caches: &Caches,
    ctx_cores: u32,
    ctx_mem: u64,
) -> ContainerSnapshot {
    let short = short_id(&raw.id).to_string();
    let limits = caches.limits.get(&(context.to_string(), short));
    let mut snap = ContainerSnapshot::from_raw(
        raw.id.clone(),
        raw.name.clone(),
        raw.raw_cpu.clone(),
        raw.raw_mem_usage.clone(),
        raw.raw_mem_perc.clone(),
        raw.raw_net.clone(),
        raw.raw_block.clone(),
        raw.raw_pids.clone(),
        context.to_string(),
        transport.to_string(),
        limits,
        ctx_cores,
        ctx_mem,
    );
    // Se o provider já trouxe números (Bollard), prefere-os ao re-parse.
    if let (Some(c), Some(mu), Some(ml)) = (raw.cpu, raw.mem_used, raw.mem_limit_stats) {
        snap.cpu = c;
        snap.mem_used = mu;
        if !snap.has_mem_limit {
            snap.mem_limit = if ml > 0.0 { ml } else { ctx_mem as f64 };
            snap.has_mem_limit = ml > 0.0;
        }
        let lim = ContainerLimits {
            cpu_limit: snap.cpu_limit,
            has_cpu_limit: snap.has_cpu_limit,
            mem_limit: snap.mem_limit,
            has_mem_limit: snap.has_mem_limit,
        };
        snap.apply_limits(&lim);
    }
    if let (Some(rx), Some(tx)) = (raw.net_rx, raw.net_tx) {
        snap.net_rx = rx;
        snap.net_tx = tx;
    }
    if let (Some(r), Some(w)) = (raw.block_read, raw.block_write) {
        snap.block_read = r;
        snap.block_write = w;
    }
    if let Some(p) = raw.pids {
        snap.pids = p;
    }
    snap
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_FAKE_DOCKER_ID: AtomicU64 = AtomicU64::new(0);

    /// Escreve um `docker` fake executável e retorna seu caminho + dir p/ limpeza.
    fn fake_docker_bin(tag: &str, script: &str) -> (PathBuf, PathBuf) {
        let unique = NEXT_FAKE_DOCKER_ID.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("dtcli-{tag}-{}-{unique}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("docker");
        let mut f = std::fs::File::create(&bin).unwrap();
        f.write_all(script.as_bytes()).unwrap();
        drop(f);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        (bin, dir)
    }

    async fn read_unix_http_request(stream: &mut tokio::net::UnixStream) -> String {
        use tokio::io::AsyncReadExt;

        let mut request = Vec::new();
        let mut buffer = [0u8; 1024];
        loop {
            let read = stream.read(&mut buffer).await.expect("read HTTP request");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        String::from_utf8_lossy(&request).into_owned()
    }

    async fn write_unix_http_response(stream: &mut tokio::net::UnixStream, body: &str) {
        use tokio::io::AsyncWriteExt;

        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream
            .write_all(response.as_bytes())
            .await
            .expect("write HTTP response");
    }

    #[test]
    fn cache_evict_missing() {
        let mut c = Caches::default();
        c.limits.insert(
            ("ctx".into(), "aaa".into()),
            ContainerLimits {
                cpu_limit: 1.0,
                has_cpu_limit: true,
                mem_limit: 1.0,
                has_mem_limit: true,
            },
        );
        c.limits.insert(
            ("ctx".into(), "bbb".into()),
            ContainerLimits {
                cpu_limit: 1.0,
                has_cpu_limit: true,
                mem_limit: 1.0,
                has_mem_limit: true,
            },
        );
        let seen: HashSet<String> = ["aaa".to_string()].into_iter().collect();
        c.evict_missing("ctx", &seen);
        assert!(c
            .limits
            .contains_key(&("ctx".to_string(), "aaa".to_string())));
        assert!(!c
            .limits
            .contains_key(&("ctx".to_string(), "bbb".to_string())));
    }

    #[tokio::test]
    async fn mock_provider_roundtrip() {
        let raw = RawStats::from_strings([
            "abcdef123456".into(),
            "web".into(),
            "10.0%".into(),
            "100MiB / 1GiB".into(),
            "9.7%".into(),
            "1MiB / 2MiB".into(),
            "3MiB / 4MiB".into(),
            "7".into(),
        ]);
        let mut m = MockProvider::new("default", vec![raw]);
        assert_eq!(m.context_name(), "default");
        assert_eq!(m.transport_label(), "mock");
        assert_eq!(m.list_stats().await.unwrap().len(), 1);
        m.limits.insert(
            "abcdef123456".into(),
            ContainerLimits {
                cpu_limit: 2.0,
                has_cpu_limit: true,
                mem_limit: 1_073_741_824.0,
                has_mem_limit: true,
            },
        );
        let limits = m.inspect_limits(&["abcdef123456".into()]).await.unwrap();
        assert_eq!(limits.len(), 1);
        assert_eq!(limits[0].1.cpu_limit, 2.0);
        let (c, mm) = m.system_info().await.unwrap();
        assert_eq!((c, mm), (4, 8 * 1024 * 1024 * 1024));
    }

    #[tokio::test]
    async fn cli_provider_lists_stats_and_info_with_fake_docker() {
        let script = "#!/bin/sh\nif printf '%s' \"$*\" | grep -q 'stats'; then printf 'abcdef123456\\tweb\\t5.0%%\\t100MiB / 1GiB\\t9.7%%\\t1MiB / 1MiB\\t1MiB / 1MiB\\t3\\n'; elif printf '%s' \"$*\" | grep -q 'inspect'; then printf 'abcdef1234567890\\t0\\t0\\t0\\t\\t536870912\\n'; elif printf '%s' \"$*\" | grep -q 'info'; then printf '4\\t8589934592\\n'; else exit 1; fi\n";
        let (bin, dir) = fake_docker_bin("ok", script);
        let p = CliProvider::new(
            "default".into(),
            Duration::from_secs(5),
            Duration::from_secs(5),
            Duration::from_secs(5),
        )
        .with_bin(bin);
        assert_eq!(p.context_name(), "default");
        assert_eq!(p.transport_label(), "cli");
        let rows = p.list_stats().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "web");
        let (cores, mem) = p.system_info().await.unwrap();
        assert_eq!((cores, mem), (4, 8589934592));
        let lim = p
            .inspect_limits(&["abcdef123456".to_string()])
            .await
            .unwrap();
        assert_eq!(lim.len(), 1);
        assert!((lim[0].1.mem_limit - 536870912.0).abs() < 1.0);
        let empty = p.inspect_limits(&[]).await.unwrap();
        assert!(empty.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn cli_provider_skips_blank_and_malformed_stats_rows() {
        let script = "#!/bin/sh\nprintf '%s' \"$*\" | grep -q 'stats' && printf '\\nmalformed row\\n\\033[31mabcdef123456\\tweb\\t5.0%%\\t100MiB / 1GiB\\t9.7%%\\t1MiB / 1MiB\\t1MiB / 1MiB\\t3\\033[0m\\n'\n";
        let (bin, dir) = fake_docker_bin("stats-filter", script);
        let provider = CliProvider::new(
            "default".into(),
            Duration::from_secs(2),
            Duration::from_secs(2),
            Duration::from_secs(2),
        )
        .with_bin(bin);

        let rows = provider.list_stats().await.expect("list fake stats");

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "web");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn cli_provider_info_error_propagates() {
        let (bin, dir) = fake_docker_bin("fail", "#!/bin/sh\nexit 1\n");
        let p = CliProvider::new(
            "ctxdown".into(),
            Duration::from_secs(2),
            Duration::from_secs(2),
            Duration::from_secs(2),
        )
        .with_bin(bin);
        assert!(p.system_info().await.is_err());
        assert!(p.list_stats().await.is_err());
        assert!(p
            .inspect_limits(&["zzz".to_string()])
            .await
            .unwrap()
            .is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn cli_provider_tolerates_malformed_info_and_failed_fallback_inspects() {
        let script = "#!/bin/sh\ncase \"$*\" in\n  *info*) printf 'invalid\\tinvalid\\n' ;;\n  *inspect*) echo simulated inspect failure >&2; exit 1 ;;\n  *) exit 2 ;;\nesac\n";
        let (bin, dir) = fake_docker_bin("inspect-fallback-failures", script);
        let provider = CliProvider::new(
            "default".into(),
            Duration::from_secs(2),
            Duration::from_secs(2),
            Duration::from_secs(2),
        )
        .with_bin(bin);

        let limits = provider
            .inspect_limits(&["abcdef1234567890".into()])
            .await
            .expect("inspect failures are tolerated");

        assert!(limits.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn cli_provider_inspect_falls_back_to_individual_ids() {
        let first_id = "abcdef1234567890";
        let second_id = "fedcba6543210987";
        let script = format!(
            "#!/bin/sh\ncase \"$*\" in\n  *\"{first_id} {second_id}\"*) exit 1 ;;\n  *\"{first_id}\"*) printf '{first_id}\\t0\\t0\\t0\\t\\t268435456\\n' ;;\n  *\"{second_id}\"*) printf '{second_id}\\t0\\t0\\t0\\t\\t536870912\\n' ;;\n  *info*) printf '4\\t8589934592\\n' ;;\n  *) exit 2 ;;\nesac\n"
        );
        let (bin, dir) = fake_docker_bin("inspect-fallback", &script);
        let provider = CliProvider::new(
            "default".into(),
            Duration::from_secs(3),
            Duration::from_secs(3),
            Duration::from_secs(3),
        )
        .with_bin(bin);

        let limits = provider
            .inspect_limits(&[first_id.into(), second_id.into()])
            .await
            .expect("individual inspect fallback");
        assert_eq!(limits.len(), 2);
        assert_eq!(limits[0].0, "abcdef123456");
        assert_eq!(limits[1].0, "fedcba654321");
        assert_eq!(limits[0].1.mem_limit, 268_435_456.0);
        assert_eq!(limits[1].1.mem_limit, 536_870_912.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn cli_provider_ssh_direct_drops_local_context_and_runs_remote_docker() {
        let dir = std::env::temp_dir().join(format!("dtsshprov-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("args.log");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> {}\ncase \"$*\" in\n  *stats*) printf 'abcdef123456\\tweb\\t5.0%%\\t100MiB / 1GiB\\t9.7%%\\t1MiB / 1MiB\\t1MiB / 1MiB\\t3\\n' ;;\n  *info*) printf '4\\t8589934592\\n' ;;\n  *) exit 2 ;;\nesac\n",
            log.display()
        );
        let bin = dir.join("ssh");
        std::fs::write(&bin, &script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // bin local propositalmente inexistente: prova que o modo ssh não o usa.
        let p = CliProvider::new(
            "lab-node1".into(),
            Duration::from_secs(3),
            Duration::from_secs(3),
            Duration::from_secs(3),
        )
        .with_bin(PathBuf::from("/nonexistent-dt-docker"))
        .with_ssh_bin(bin)
        .with_ssh_host(Some("home-host".into()));

        let rows = p.list_stats().await.expect("ssh-direct stats");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "web");
        let logged = std::fs::read_to_string(&log).unwrap();
        assert!(logged.contains("BatchMode=yes"), "{logged}");
        assert!(logged.contains("ConnectTimeout="), "{logged}");
        assert!(
            logged.contains("StrictHostKeyChecking=accept-new"),
            "{logged}"
        );
        assert!(logged.lines().any(|l| l == "home-host"), "{logged}");
        // comando remoto único e quotado: template Go não pode sofrer brace
        // expansion no shell remoto (daemon já recebeu "{{.Name}}" como arg).
        let remote = logged
            .lines()
            .find(|l| l.starts_with("docker "))
            .expect("comando remoto presente");
        assert!(remote.starts_with("docker 'stats'"), "{remote}");
        assert!(remote.contains("'--format'"), "{remote}");
        assert!(remote.contains("'{{.ID}}"), "{remote}");
        assert!(remote.ends_with("{{.PIDs}}'"), "{remote}");
        // args docker só dentro do comando remoto — nada além dele no argv
        // (bug real: args crus concatenados ao comando viravam nomes de
        // container no daemon remoto: "No such container: stats").
        // 8 args base: -n + 6 opções/destino (-o BatchMode, -o ConnectTimeout,
        // -o StrictHostKeyChecking, host) + 1 comando remoto.
        assert_eq!(
            logged.lines().count(),
            9,
            "esperados 8 args base (-n incluso) + 1 comando remoto, log: {logged}"
        );
        // --context é conceito do docker local e NÃO atravessa o ssh
        assert!(!logged.contains("--context"), "{logged}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn cli_provider_system_info_uses_ttl_cache_within_same_tick() {
        let dir = std::env::temp_dir().join(format!("dtsyscache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("info.log");
        let script = format!(
            "#!/bin/sh\ncase \"$*\" in\n  *info*) echo called >> {}; printf '4\\t8589934592\\n' ;;\n  *) exit 2 ;;\nesac\n",
            log.display()
        );
        let bin = dir.join("docker");
        std::fs::write(&bin, &script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let p = CliProvider::new(
            "default".into(),
            Duration::from_secs(2),
            Duration::from_secs(2),
            Duration::from_secs(2),
        )
        .with_bin(bin);

        let a = p.system_info().await.expect("info 1");
        let b = p.system_info().await.expect("info 2");
        assert_eq!(a, b);
        let calls = std::fs::read_to_string(&log).unwrap().lines().count();
        assert_eq!(
            calls, 1,
            "2ª chamada deve vir do cache TTL (evita info duplicado)"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// End-to-end Docker API stats + inspect through a fake Unix-socket Engine.
    #[tokio::test]
    async fn bollard_lists_stats_and_inspects_over_unix_socket() {
        use tokio::net::UnixListener;

        let dir = std::env::temp_dir().join(format!("dt-api-stats-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("docker.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let id = format!("abcdef123456{}", "0".repeat(52));
        let server_id = id.clone();
        let server = tokio::spawn(async move {
            for _ in 0..5 {
                let (mut stream, _) = listener.accept().await.expect("accept API connection");
                let request = read_unix_http_request(&mut stream).await;
                let first_line = request.lines().next().unwrap_or_default();
                let body = if first_line.contains("/containers/json") {
                    format!(r#"[{{"Id":"{server_id}","Names":["/web"]}}]"#)
                } else if first_line.contains("/stats?") || first_line.contains("/stats ") {
                    serde_json::json!({
                        "cpu_stats": {
                            "cpu_usage": {"total_usage": 150},
                            "system_cpu_usage": 1000,
                            "online_cpus": 3
                        },
                        "precpu_stats": {
                            "cpu_usage": {"total_usage": 50},
                            "system_cpu_usage": 400
                        },
                        "memory_stats": {"usage": 1048576, "limit": 4194304},
                        "networks": {"eth0": {"rx_bytes": 512, "tx_bytes": 256}},
                        "blkio_stats": {
                            "io_service_bytes_recursive": [
                                {"op": "read", "value": 4096},
                                {"op": "write", "value": 2048}
                            ]
                        },
                        "pids_stats": {"current": 9}
                    })
                    .to_string()
                } else if first_line.contains("/info ") {
                    r#"{"NCPU":4,"MemTotal":8589934592}"#.to_string()
                } else if first_line.contains(&format!("/containers/{server_id}/json")) {
                    serde_json::json!({
                        "Id": server_id,
                        "HostConfig": {
                            "NanoCpus": 1000000000,
                            "CpuQuota": 50000,
                            "CpuPeriod": 100000,
                            "Memory": 536870912
                        }
                    })
                    .to_string()
                } else {
                    panic!("unexpected Docker API request: {first_line}");
                };
                write_unix_http_response(&mut stream, &body).await;
            }
        });

        let provider = BollardProvider::new_full(
            "desktop-linux".into(),
            format!("unix://{}", socket.display()),
            Duration::from_secs(3),
            Duration::from_secs(3),
            Duration::from_secs(3),
        );
        assert_eq!(provider.context_name(), "desktop-linux");
        assert_eq!(provider.transport_label(), "api");
        let rows = provider.list_stats().await.expect("list stats");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id);
        assert_eq!(rows[0].name, "web");
        assert_eq!(rows[0].cpu, Some(50.0));
        assert_eq!(rows[0].mem_used, Some(1_048_576.0));
        assert_eq!(rows[0].net_rx, Some(512.0));
        assert_eq!(rows[0].pids, Some(9));
        assert_eq!(
            provider.system_info().await.expect("system info"),
            (4, 8_589_934_592)
        );

        let limits = provider
            .inspect_limits(std::slice::from_ref(&id))
            .await
            .expect("inspect limits");
        assert_eq!(limits.len(), 1);
        assert_eq!(limits[0].0, short_id(&id));
        assert!(limits[0].1.has_mem_limit);
        assert_eq!(limits[0].1.mem_limit, 536_870_912.0);
        server.await.expect("mock Docker API server");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// End-to-end Unix-socket API test with a fake Docker Engine (macOS/Linux).
    #[tokio::test]
    async fn bollard_connects_to_macos_style_unix_socket() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::UnixListener;

        let dir = std::env::temp_dir().join(format!("dt-api-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("docker.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept API connection");
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            loop {
                let read = stream.read(&mut buffer).await.expect("read API request");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request_line = String::from_utf8_lossy(&request)
                .lines()
                .next()
                .unwrap_or_default()
                .to_string();
            assert!(
                request_line.contains("/info "),
                "unexpected request: {request_line}"
            );

            let body = r#"{"NCPU":8,"MemTotal":17179869184}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("write API response");
        });

        let endpoint = format!("unix://{}", socket.display());
        let provider = BollardProvider::new_full(
            "desktop-linux".into(),
            endpoint,
            Duration::from_secs(2),
            Duration::from_secs(2),
            Duration::from_secs(2),
        );
        assert_eq!(provider.transport_label(), "api");
        assert_eq!(
            provider.system_info().await.expect("mock Docker info"),
            (8, 17_179_869_184)
        );
        server.await.expect("mock Docker server");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn bollard_system_info_defaults_missing_fields() {
        use tokio::net::UnixListener;

        let dir = std::env::temp_dir().join(format!("dt-api-info-defaults-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("docker.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept API connection");
            let request = read_unix_http_request(&mut stream).await;
            assert!(request
                .lines()
                .next()
                .unwrap_or_default()
                .contains("/info "));
            write_unix_http_response(&mut stream, "{}").await;
        });

        let provider = BollardProvider::new(
            "defaults".into(),
            format!("unix://{}", socket.display()),
            Duration::from_secs(2),
        );

        assert_eq!(
            provider.system_info().await.expect("empty info response"),
            (FALLBACK_CORES, FALLBACK_MEM)
        );
        server.await.expect("mock Docker API server");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Smoke-test available local Unix sockets; an engine with zero containers is valid.
    #[tokio::test]
    async fn bollard_live_daemon_if_available() {
        let mut sockets = Vec::new();
        if let Ok(host) = std::env::var("DOCKER_HOST") {
            if let Some(path) = host.strip_prefix("unix://") {
                sockets.push(std::path::PathBuf::from(path));
            }
        }
        sockets.push(std::path::PathBuf::from("/var/run/docker.sock"));
        if let Some(home) = dirs::home_dir() {
            sockets.push(home.join(".docker/run/docker.sock"));
            sockets.push(home.join(".orbstack/run/docker.sock"));
        }

        let mut tested = std::collections::HashSet::new();
        for socket in sockets {
            let Ok(path) = std::fs::canonicalize(&socket) else {
                continue;
            };
            let path = path.to_string_lossy().into_owned();
            if !tested.insert(path.clone()) {
                continue;
            }
            let endpoint = format!("unix://{path}");
            let provider =
                BollardProvider::new("local-smoke".into(), endpoint, Duration::from_secs(10));
            let (cores, mem) = provider.system_info().await.expect("daemon info");
            assert!(cores >= 1 && mem > 0);
            let rows = provider.list_stats().await.expect("daemon stats");
            let ids: Vec<String> = rows.iter().take(2).map(|row| row.id.clone()).collect();
            provider.inspect_limits(&ids).await.expect("daemon inspect");
        }
        if tested.is_empty() {
            eprintln!("pulando teste Bollard live: nenhum socket Docker Unix local");
        }
    }

    #[test]
    fn bollard_stats_convert_cpu_memory_network_block_io_and_pids() {
        let stats: bollard::models::ContainerStatsResponse =
            serde_json::from_value(serde_json::json!({
                "cpu_stats": {
                    "cpu_usage": {"total_usage": 150},
                    "system_cpu_usage": 1000,
                    "online_cpus": 3
                },
                "precpu_stats": {
                    "cpu_usage": {"total_usage": 50},
                    "system_cpu_usage": 400
                },
                "memory_stats": {"usage": 1048576, "limit": 4194304},
                "networks": {
                    "eth0": {"rx_bytes": 512, "tx_bytes": 256},
                    "eth1": {"rx_bytes": 128, "tx_bytes": 64}
                },
                "blkio_stats": {
                    "io_service_bytes_recursive": [
                        {"op": "read", "value": 4096},
                        {"op": "write", "value": 2048},
                        {"op": "discard", "value": 1024}
                    ]
                },
                "pids_stats": {"current": 9}
            }))
            .expect("valid Docker stats fixture");

        let (cpu, used, limit, rx, tx, read, write, pids) = bollard_sample_to_numbers(&stats);
        assert!((cpu - 50.0).abs() < 1e-9);
        assert_eq!((used, limit), (1_048_576.0, 4_194_304.0));
        assert_eq!((rx, tx), (640.0, 320.0));
        assert_eq!((read, write, pids), (4096.0, 2048.0, 9));
    }

    #[test]
    fn bollard_stats_saturate_counter_resets_and_default_missing_fields() {
        let stats: bollard::models::ContainerStatsResponse =
            serde_json::from_value(serde_json::json!({
                "cpu_stats": {
                    "cpu_usage": {"total_usage": 10},
                    "system_cpu_usage": 100
                },
                "precpu_stats": {
                    "cpu_usage": {"total_usage": 20},
                    "system_cpu_usage": 100
                }
            }))
            .expect("valid partial Docker stats fixture");

        assert_eq!(
            bollard_sample_to_numbers(&stats),
            (0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0)
        );
    }

    #[test]
    fn build_snapshot_prefers_bollard_numbers_and_evicts() {
        let caches = Caches::default();
        let mut raw = RawStats::from_strings([
            "abcdef123456".into(),
            "web".into(),
            "xx".into(),
            "yy".into(),
            "zz".into(),
            "aa".into(),
            "bb".into(),
            "cc".into(),
        ]);
        raw.cpu = Some(25.0);
        raw.mem_used = Some(100.0 * 1024.0 * 1024.0);
        raw.mem_limit_stats = Some(1024.0 * 1024.0 * 1024.0);
        raw.net_rx = Some(10.0);
        raw.net_tx = Some(20.0);
        raw.block_read = Some(30.0);
        raw.block_write = Some(40.0);
        raw.pids = Some(9);
        let s = build_snapshot(&raw, "default", "api", &caches, 4, 8 * 1024 * 1024 * 1024);
        assert!((s.cpu - 25.0).abs() < 1e-9);
        assert_eq!(s.pids, 9);
        assert!((s.net_total() - 30.0).abs() < 1e-9);
        assert!((s.block_total() - 70.0).abs() < 1e-9);
        assert_eq!(s.mem_limit, 1024.0 * 1024.0 * 1024.0);
        assert!(s.has_mem_limit);
    }

    #[test]
    fn build_snapshot_preserves_cached_limit_and_does_not_infer_zero() {
        let mut raw = RawStats::from_strings([
            "abcdef123456".into(),
            "web".into(),
            "invalid".into(),
            "invalid".into(),
            "invalid".into(),
            "invalid".into(),
            "invalid".into(),
            "invalid".into(),
        ]);
        raw.cpu = Some(1.0);
        raw.mem_used = Some(100.0);
        raw.mem_limit_stats = Some(1024.0 * 1024.0 * 1024.0);

        let mut caches = Caches::default();
        caches.limits.insert(
            ("default".into(), "abcdef123456".into()),
            ContainerLimits {
                cpu_limit: 2.0,
                has_cpu_limit: true,
                mem_limit: 2.0 * 1024.0 * 1024.0 * 1024.0,
                has_mem_limit: true,
            },
        );
        let cached = build_snapshot(&raw, "default", "api", &caches, 4, 8 * 1024 * 1024 * 1024);
        assert_eq!(cached.mem_limit, 2.0 * 1024.0 * 1024.0 * 1024.0);
        assert!(cached.has_mem_limit);

        let mut zero = raw;
        zero.id = "123456789abc".into();
        zero.mem_limit_stats = Some(0.0);
        let uncached = build_snapshot(
            &zero,
            "default",
            "api",
            &Caches::default(),
            4,
            8 * 1024 * 1024 * 1024,
        );
        assert_eq!(uncached.mem_limit, (8_u64 * 1024 * 1024 * 1024) as f64);
        assert!(!uncached.has_mem_limit);
    }

    #[test]
    fn fallback_memory_is_sixteen_gibibytes() {
        assert_eq!(FALLBACK_MEM, 16 * 1024 * 1024 * 1024);
    }
}
