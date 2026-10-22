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
#[derive(Debug, Default)]
pub struct Caches {
    pub limits: HashMap<(String, String), ContainerLimits>,
    pub sysinfo: HashMap<String, (u32, u64)>,
}

impl Caches {
    /// Remove entradas cujos ids sumiram no último ciclo.
    pub fn evict_missing(&mut self, ctx: &str, seen: &HashSet<String>) {
        self.limits
            .retain(|(c, short), _| c != ctx || seen.contains(short));
    }
}

pub type SharedCaches = Arc<Mutex<Caches>>;

// ---------------------------------------------------------------------------
// CliProvider — ssh:// (homeserver, oci-ampere-1/2) e fallback geral
// ---------------------------------------------------------------------------

pub struct CliProvider {
    context: String,
    bin: PathBuf,
    stats_timeout: Duration,
    inspect_timeout: Duration,
    info_timeout: Duration,
}

impl CliProvider {
    pub fn new(context: String, stats: Duration, inspect: Duration, info: Duration) -> Self {
        Self {
            context,
            bin: PathBuf::from("docker"),
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

    async fn run_docker(&self, args: &[&str], timeout: Duration) -> anyhow::Result<String> {
        let mut cmd = tokio::process::Command::new(&self.bin);
        cmd.args(args).kill_on_drop(true);
        let out = tokio::time::timeout(timeout, cmd.output())
            .await
            .map_err(|_| {
                anyhow::anyhow!("timeout {}s: docker {}", timeout.as_secs(), args.join(" "))
            })?
            .with_context(|| format!("spawn {} {}", self.bin.display(), args.join(" ")))?;
        if !out.status.success() {
            anyhow::bail!(
                "docker {} falhou: {}",
                args.join(" "),
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
        let (cores, mem) = self.system_info().await.unwrap_or((1, 16 << 30));
        let fmt = "{{.Id}}\t{{.HostConfig.NanoCpus}}\t{{.HostConfig.CpuQuota}}\t{{.HostConfig.CpuPeriod}}\t{{.HostConfig.CpusetCpus}}\t{{.HostConfig.Memory}}";
        let mut args: Vec<String> = vec![
            "--context".into(),
            self.context.clone(),
            "inspect".into(),
            "--format".into(),
            fmt.into(),
        ];
        args.extend(ids.iter().cloned());
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let stdout = match self.run_docker(&arg_refs, self.inspect_timeout).await {
            Ok(s) => s,
            Err(_) => {
                // Fallback individual (container pode ter morrido) — igual ao Python.
                let mut acc = String::new();
                for id in ids {
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
                        if !s.ends_with('\n') {
                            acc.push('\n');
                        }
                    }
                }
                acc
            }
        };
        let mut out = Vec::new();
        for line in stdout.lines() {
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
        if parts.len() >= 2 {
            if let (Ok(c), Ok(m)) = (
                parts[0].trim().parse::<u32>(),
                parts[1].trim().parse::<u64>(),
            ) {
                return Ok((c.max(1), m));
            }
        }
        anyhow::bail!("info parse falhou p/ {}", self.context)
    }
}

// ---------------------------------------------------------------------------
// BollardProvider — unix:// local via API (rustls)
// ---------------------------------------------------------------------------

pub struct BollardProvider {
    context: String,
    socket: String,
    info_timeout: Duration,
}

impl BollardProvider {
    pub fn new(context: String, socket: String, info_timeout: Duration) -> Self {
        Self {
            context,
            socket,
            info_timeout,
        }
    }

    fn connect(&self) -> anyhow::Result<bollard::Docker> {
        let path = self.socket.strip_prefix("unix://").unwrap_or(&self.socket);
        bollard::Docker::connect_with_socket(path, 120, bollard::API_DEFAULT_VERSION)
            .context("bollard connect unix socket")
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
            info.ncpu.unwrap_or(1) as u32,
            info.mem_total.unwrap_or(16 << 30) as u64,
        ))
    }

    async fn list_stats(&self) -> anyhow::Result<Vec<RawStats>> {
        use bollard::query_parameters::ListContainersOptionsBuilder;
        use futures_util::StreamExt;
        let docker = self.connect()?;
        let containers = tokio::time::timeout(
            self.info_timeout,
            docker.list_containers(Some(
                ListContainersOptionsBuilder::default().all(true).build(),
            )),
        )
        .await
        .map_err(|_| anyhow::anyhow!("timeout list {}", self.context))??;
        let mut rows = Vec::new();
        for c in containers {
            let Some(id) = c.id else { continue };
            let name = c
                .names
                .as_ref()
                .and_then(|n| n.first())
                .map(|n| n.trim_start_matches('/').to_string())
                .unwrap_or_else(|| short_id(&id).to_string());
            // one-shot stats (sem stream)
            let opts = bollard::query_parameters::StatsOptionsBuilder::default()
                .stream(false)
                .one_shot(true)
                .build();
            let mut stream = docker.stats(&id, Some(opts));
            let Ok(Some(Ok(s))) = tokio::time::timeout(self.info_timeout, stream.next()).await
            else {
                continue;
            };
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
        let (cores, mem) = self.system_info().await.unwrap_or((1, 16 << 30));
        let mut out = Vec::new();
        for id in ids {
            let Ok(info) = docker
                .inspect_container(
                    id,
                    None::<bollard::query_parameters::InspectContainerOptions>,
                )
                .await
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

    /// Escreve um `docker` fake executável e retorna seu caminho + dir p/ limpeza.
    fn fake_docker_bin(tag: &str, script: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("dtcli-{tag}-{}", std::process::id()));
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
        let m = MockProvider::new("default", vec![raw]);
        assert_eq!(m.transport_label(), "mock");
        assert_eq!(m.list_stats().await.unwrap().len(), 1);
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

    /// Bollard contra o daemon real (pula sem socket — documentado).
    #[tokio::test]
    async fn bollard_live_daemon_if_available() {
        if !std::path::Path::new("/var/run/docker.sock").exists() {
            eprintln!("pulando teste Bollard: sem /var/run/docker.sock");
            return;
        }
        let p = BollardProvider::new(
            "default".into(),
            "unix:///var/run/docker.sock".into(),
            Duration::from_secs(10),
        );
        assert_eq!(p.transport_label(), "api");
        let (cores, mem) = p.system_info().await.expect("daemon info");
        assert!(cores >= 1 && mem > 0);
        let rows = p.list_stats().await.expect("daemon stats");
        assert!(
            !rows.is_empty(),
            "esperava ao menos 1 container no daemon local"
        );
        let ids: Vec<String> = rows.iter().take(2).map(|r| r.id.clone()).collect();
        let lim = p.inspect_limits(&ids).await.expect("daemon inspect");
        assert!(!lim.is_empty());
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
    }
}
