//! Modelo de domínio: limites, snapshots e ordenação (16 colunas, port do Python).

use crate::parsers;

/// As 16 colunas ordenáveis (ordem canônica = Python `SORT_COLUMNS`).
pub const SORT_COLUMNS: [&str; 16] = [
    "id",
    "context",
    "name",
    "cpu_rel",
    "mem_rel",
    "cpu",
    "mem_used",
    "cpu_lim",
    "mem_lim",
    "net_rx",
    "net_tx",
    "net",
    "block_read",
    "block_write",
    "block",
    "pids",
];

/// Limites efetivos de um container (cores + bytes), com flags `has_*`.
#[derive(Debug, Clone)]
pub struct ContainerLimits {
    pub cpu_limit: f64,
    pub has_cpu_limit: bool,
    pub mem_limit: f64,
    pub has_mem_limit: bool,
}

impl ContainerLimits {
    pub fn from_inspect(
        nano_cpus: i64,
        cpu_quota: i64,
        cpu_period: i64,
        cpuset_cpus: &str,
        memory: i64,
        ctx_cores: u32,
        ctx_mem: u64,
    ) -> Self {
        let (cpu_limit, has_cpu_limit) = if nano_cpus > 0 {
            (nano_cpus as f64 / 1_000_000_000.0, true)
        } else if cpu_quota > 0 && cpu_period > 0 {
            (cpu_quota as f64 / cpu_period as f64, true)
        } else if !cpuset_cpus.trim().is_empty() {
            (
                parsers::parse_cpuset_count(cpuset_cpus, ctx_cores) as f64,
                true,
            )
        } else {
            (ctx_cores as f64, false)
        };
        let (mem_limit, has_mem_limit) = if memory > 0 {
            (memory as f64, true)
        } else {
            (ctx_mem as f64, false)
        };
        Self {
            cpu_limit: if cpu_limit > 0.0 { cpu_limit } else { 1.0 },
            has_cpu_limit,
            mem_limit: if mem_limit > 0.0 { mem_limit } else { 1.0 },
            has_mem_limit,
        }
    }

    pub fn fallback(ctx_cores: u32, ctx_mem: u64, stats_mem_limit: f64) -> Self {
        let (mem_limit, has_mem_limit) = if stats_mem_limit > 0.0 {
            (stats_mem_limit, true)
        } else {
            (ctx_mem as f64, false)
        };
        Self {
            cpu_limit: ctx_cores as f64,
            has_cpu_limit: false,
            mem_limit,
            has_mem_limit,
        }
    }
}

/// Linha coletada + enriquecida (equivalente a `ContainerStats` Python).
#[derive(Debug, Clone)]
pub struct ContainerSnapshot {
    pub id: String,
    pub name: String,
    pub context: String,
    pub transport: String,
    pub cpu: f64,
    pub raw_cpu: String,
    pub mem_used: f64,
    pub mem_perc: f64,
    pub raw_mem_usage: String,
    pub net_rx: f64,
    pub net_tx: f64,
    pub raw_net: String,
    pub block_read: f64,
    pub block_write: f64,
    pub raw_block: String,
    pub pids: u32,
    pub cpu_limit: f64,
    pub has_cpu_limit: bool,
    pub mem_limit: f64,
    pub has_mem_limit: bool,
    pub cpu_rel: f64,
    pub mem_rel: f64,
}

impl ContainerSnapshot {
    pub fn net_total(&self) -> f64 {
        self.net_rx + self.net_tx
    }
    pub fn block_total(&self) -> f64 {
        self.block_read + self.block_write
    }

    #[allow(clippy::too_many_arguments)]
    pub fn from_raw(
        id: String,
        name: String,
        raw_cpu: String,
        raw_mem_usage: String,
        raw_mem_perc: String,
        raw_net: String,
        raw_block: String,
        raw_pids: String,
        context: String,
        transport: String,
        limits: Option<&ContainerLimits>,
        ctx_cores: u32,
        ctx_mem: u64,
    ) -> Self {
        let cpu = parsers::parse_percent(&raw_cpu);
        let mem_perc = parsers::parse_percent(&raw_mem_perc);
        let (mem_used, stats_mem_limit) = parsers::parse_double_bytes(&raw_mem_usage);
        let (net_rx, net_tx) = parsers::parse_double_bytes(&raw_net);
        let (block_read, block_write) = parsers::parse_double_bytes(&raw_block);
        let pids = parsers::parse_pid(&raw_pids);
        let fb;
        let lim = match limits {
            Some(l) => l,
            None => {
                fb = ContainerLimits::fallback(ctx_cores, ctx_mem, stats_mem_limit);
                &fb
            }
        };
        let cpu_rel = if lim.cpu_limit > 0.0 {
            cpu / lim.cpu_limit
        } else {
            0.0
        };
        let mem_rel = if lim.mem_limit > 0.0 {
            mem_used / lim.mem_limit * 100.0
        } else {
            mem_perc
        };
        Self {
            id,
            name,
            context,
            transport,
            cpu,
            raw_cpu,
            mem_used,
            mem_perc,
            raw_mem_usage,
            net_rx,
            net_tx,
            raw_net,
            block_read,
            block_write,
            raw_block,
            pids,
            cpu_limit: lim.cpu_limit,
            has_cpu_limit: lim.has_cpu_limit,
            mem_limit: lim.mem_limit,
            has_mem_limit: lim.has_mem_limit,
            cpu_rel,
            mem_rel,
        }
    }

    /// Reaplica limites frescos (worker de inspect) — igual ao Python.
    pub fn apply_limits(&mut self, lim: &ContainerLimits) {
        self.cpu_limit = lim.cpu_limit;
        self.has_cpu_limit = lim.has_cpu_limit;
        self.mem_limit = lim.mem_limit;
        self.has_mem_limit = lim.has_mem_limit;
        if self.cpu_limit > 0.0 {
            self.cpu_rel = self.cpu / self.cpu_limit;
        }
        if self.mem_limit > 0.0 {
            self.mem_rel = self.mem_used / self.mem_limit * 100.0;
        }
    }
}

pub fn short_id(id: &str) -> &str {
    if id.len() >= 12 {
        &id[..12]
    } else {
        id
    }
}

/// Ordena por qualquer uma das 16 colunas.
pub fn sort_snapshots(rows: &mut [ContainerSnapshot], sort_by: &str, desc: bool) {
    let ord = |a: f64, b: f64| {
        if desc {
            b.partial_cmp(&a).unwrap_or(std::cmp::Ordering::Equal)
        } else {
            a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal)
        }
    };
    match sort_by {
        "id" => rows.sort_by(|a, b| {
            if desc {
                b.id.cmp(&a.id)
            } else {
                a.id.cmp(&b.id)
            }
        }),
        "context" => rows.sort_by(|a, b| {
            if desc {
                b.context.cmp(&a.context)
            } else {
                a.context.cmp(&b.context)
            }
        }),
        "name" => rows.sort_by(|a, b| {
            if desc {
                b.name.cmp(&a.name)
            } else {
                a.name.cmp(&b.name)
            }
        }),
        "cpu" => rows.sort_by(|a, b| ord(a.cpu, b.cpu)),
        "cpu_lim" => rows.sort_by(|a, b| ord(a.cpu_limit, b.cpu_limit)),
        "cpu_rel" => rows.sort_by(|a, b| ord(a.cpu_rel, b.cpu_rel)),
        "mem_used" => rows.sort_by(|a, b| ord(a.mem_used, b.mem_used)),
        "mem_lim" => rows.sort_by(|a, b| ord(a.mem_limit, b.mem_limit)),
        "mem_rel" => rows.sort_by(|a, b| ord(a.mem_rel, b.mem_rel)),
        "net" => rows.sort_by(|a, b| ord(a.net_total(), b.net_total())),
        "net_rx" => rows.sort_by(|a, b| ord(a.net_rx, b.net_rx)),
        "net_tx" => rows.sort_by(|a, b| ord(a.net_tx, b.net_tx)),
        "block" => rows.sort_by(|a, b| ord(a.block_total(), b.block_total())),
        "block_read" => rows.sort_by(|a, b| ord(a.block_read, b.block_read)),
        "block_write" => rows.sort_by(|a, b| ord(a.block_write, b.block_write)),
        "pids" => rows.sort_by(|a, b| {
            if desc {
                b.pids.cmp(&a.pids)
            } else {
                a.pids.cmp(&b.pids)
            }
        }),
        _ => rows.sort_by(|a, b| ord(a.cpu, b.cpu)),
    }
}

/// Colunas visíveis por largura (ultra <100, compacto <160, full) — limites vêm do config.
pub fn visible_columns(
    term_width: u16,
    compact_w: u16,
    wide_w: u16,
    cols_cfg: &crate::config::Columns,
) -> Vec<String> {
    if term_width < compact_w {
        cols_cfg.ultra_compact.clone()
    } else if term_width < wide_w {
        cols_cfg.compact.clone()
    } else {
        cols_cfg.full.clone()
    }
}

/// Normaliza sort para coluna visível (agrupa rx/tx→net, read/write→block).
pub fn normalize_sort(sort_by: &str, visible: &[String]) -> String {
    if visible.contains(&sort_by.to_string()) {
        return sort_by.to_string();
    }
    if ["net_rx", "net_tx", "net"].contains(&sort_by) && visible.contains(&"net".to_string()) {
        return "net".to_string();
    }
    if ["block_read", "block_write", "block"].contains(&sort_by)
        && visible.contains(&"block".to_string())
    {
        return "block".to_string();
    }
    visible
        .first()
        .cloned()
        .unwrap_or_else(|| "cpu".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<ContainerSnapshot> {
        vec![
            ContainerSnapshot::from_raw(
                "aaa".into(),
                "web".into(),
                "10.0%".into(),
                "100MiB / 1GiB".into(),
                "10%".into(),
                "1MiB / 1MiB".into(),
                "1MiB / 1MiB".into(),
                "5".into(),
                "default".into(),
                "api".into(),
                None,
                4,
                8 * 1024 * 1024 * 1024,
            ),
            ContainerSnapshot::from_raw(
                "bbb".into(),
                "db".into(),
                "50.0%".into(),
                "500MiB / 1GiB".into(),
                "50%".into(),
                "2MiB / 2MiB".into(),
                "2MiB / 2MiB".into(),
                "9".into(),
                "default".into(),
                "api".into(),
                None,
                4,
                8 * 1024 * 1024 * 1024,
            ),
        ]
    }

    #[test]
    fn sort_by_cpu_desc_puts_db_first() {
        let mut rows = sample();
        sort_snapshots(&mut rows, "cpu", true);
        assert_eq!(rows[0].name, "db");
    }

    #[test]
    fn all_16_sorts_do_not_panic() {
        for col in SORT_COLUMNS {
            let mut rows = sample();
            sort_snapshots(&mut rows, col, true);
            let mut rows2 = sample();
            sort_snapshots(&mut rows2, col, false);
        }
    }

    #[test]
    fn rel_metrics_use_limits() {
        let rows = sample();
        // 100MiB/1GiB ≈ 9.77%
        assert!((rows[0].mem_rel - 9.765625).abs() < 0.01);
    }

    #[test]
    fn normalize_groups_net_block() {
        let vis = vec!["net".to_string(), "block".to_string()];
        assert_eq!(normalize_sort("net_rx", &vis), "net");
        assert_eq!(normalize_sort("block_write", &vis), "block");
        assert_eq!(normalize_sort("pids", &vis), "net");
    }

    #[test]
    fn inspect_branches_nano_quota_cpuset_fallback() {
        // nano_cpus manda
        let l = ContainerLimits::from_inspect(2_000_000_000, 0, 0, "", 0, 8, 16 << 30);
        assert!((l.cpu_limit - 2.0).abs() < 1e-9 && l.has_cpu_limit);
        // quota/period
        let l = ContainerLimits::from_inspect(0, 200000, 100000, "", 0, 8, 16 << 30);
        assert!((l.cpu_limit - 2.0).abs() < 1e-9 && l.has_cpu_limit);
        // cpuset
        let l = ContainerLimits::from_inspect(0, 0, 0, "0-3", 0, 8, 16 << 30);
        assert!((l.cpu_limit - 4.0).abs() < 1e-9 && l.has_cpu_limit);
        // fallback host + mem do stats
        let l = ContainerLimits::from_inspect(0, 0, 0, "", 0, 8, 16 << 30);
        assert!(!l.has_cpu_limit && !l.has_mem_limit);
        let l = ContainerLimits::fallback(8, 16 << 30, 1.0 * 1024.0 * 1024.0 * 1024.0);
        assert!(l.has_mem_limit && !l.has_cpu_limit);
    }

    #[test]
    fn short_id_and_apply_limits() {
        assert_eq!(short_id("abcdef1234567890"), "abcdef123456");
        assert_eq!(short_id("abc"), "abc");
        let mut rows = sample();
        let lim = ContainerLimits {
            cpu_limit: 2.0,
            has_cpu_limit: true,
            mem_limit: 2.0 * 1024.0 * 1024.0 * 1024.0,
            has_mem_limit: true,
        };
        rows[0].apply_limits(&lim);
        assert!((rows[0].cpu_limit - 2.0).abs() < 1e-9);
    }

    #[test]
    fn visible_columns_all_bands() {
        let cfg = crate::config::AppConfig::default();
        let ultra = visible_columns(
            80,
            cfg.layout.compact_width,
            cfg.layout.wide_width,
            &cfg.columns,
        );
        let compact = visible_columns(
            120,
            cfg.layout.compact_width,
            cfg.layout.wide_width,
            &cfg.columns,
        );
        let full = visible_columns(
            200,
            cfg.layout.compact_width,
            cfg.layout.wide_width,
            &cfg.columns,
        );
        assert!(ultra.len() < compact.len() && compact.len() < full.len());
        assert!(!normalize_sort("pids", &[]).is_empty());
    }

    #[test]
    fn limit_selection_and_zero_denominators_use_safe_fallbacks() {
        let quota_without_period = ContainerLimits::from_inspect(0, 100_000, 0, "", 0, 0, 0);
        assert_eq!(quota_without_period.cpu_limit, 1.0);
        assert!(!quota_without_period.has_cpu_limit);
        assert_eq!(quota_without_period.mem_limit, 1.0);
        assert!(!quota_without_period.has_mem_limit);

        let cpu_from_cpuset = ContainerLimits::from_inspect(0, 0, 0, "0-1", 0, 8, 0);
        assert_eq!(cpu_from_cpuset.cpu_limit, 2.0);
        assert_eq!(cpu_from_cpuset.mem_limit, 1.0);
        let quota_without_period = ContainerLimits::from_inspect(0, 200_000, 0, "", 0, 8, 1);
        assert!(!quota_without_period.has_cpu_limit);
        let period_without_quota = ContainerLimits::from_inspect(0, 0, 100_000, "", 0, 8, 1);
        assert!(!period_without_quota.has_cpu_limit);

        let fallback = ContainerLimits::fallback(0, 0, 0.0);
        assert_eq!(fallback.cpu_limit, 0.0);
        assert_eq!(fallback.mem_limit, 0.0);
        assert!(!fallback.has_mem_limit);

        let mut row = ContainerSnapshot::from_raw(
            "id".into(),
            "web".into(),
            "10%".into(),
            "100B / 0B".into(),
            "25%".into(),
            "0B / 0B".into(),
            "0B / 0B".into(),
            "0".into(),
            "default".into(),
            "api".into(),
            Some(&fallback),
            0,
            0,
        );
        assert_eq!(row.cpu_rel, 0.0);
        assert_eq!(row.mem_rel, 25.0);
        row.apply_limits(&fallback);
        assert_eq!(row.cpu_rel, 0.0);
        assert_eq!(row.mem_rel, 25.0);
    }

    #[test]
    fn sort_normalization_handles_visible_and_fallback_columns() {
        let visible = vec!["id".to_string(), "net".to_string()];
        assert_eq!(normalize_sort("id", &visible), "id");
        assert_eq!(normalize_sort("net_tx", &visible), "net");
        assert_eq!(normalize_sort("net_rx", &["id".into()]), "id");
        assert_eq!(normalize_sort("block_read", &visible), "id");
        assert_eq!(normalize_sort("block_write", &["block".into()]), "block");
        assert_eq!(normalize_sort("anything", &[]), "cpu");
        assert_eq!(normalize_sort("block", &["block".into()]), "block");
        let mut rows = sample();
        sort_snapshots(&mut rows, "unknown-column", false);
        assert_eq!(rows[0].id, "aaa");
    }
}
