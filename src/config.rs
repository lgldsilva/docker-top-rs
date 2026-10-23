//! Configuração dinâmica via TOML (figment + serde `deny_unknown_fields`).
//!
//! Hierarquia (último vence): [`AppConfig::default`] < arquivo
//! `~/.config/docker-top/config.toml` < env `DOCKER_TOP_*` (nested com `__`)
//! < CLI (`--delay/--sort/...`).
//! Hot reload: tecla `R` na TUI + `SIGHUP` (ver `main.rs`).

use std::collections::HashMap;
use std::path::PathBuf;

use figment::providers::{Env, Format, Serialized, Toml};
use figment::Figment;
use serde::{Deserialize, Serialize};

use crate::cli::Cli;

/// Caminho padrão do arquivo de configuração.
pub fn default_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("docker-top")
        .join("config.toml")
}

/// Caminho do `config.example.toml` versionado (gerado por [`generate_example_file`]).
pub fn example_config_path() -> PathBuf {
    PathBuf::from("config.example.toml")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Timeouts {
    /// `docker context ls` (s).
    pub context_ls: f64,
    /// `docker info` por contexto (s).
    pub info: f64,
    /// `docker stats` por contexto (s).
    pub stats: f64,
    /// `docker inspect` por contexto (s).
    pub inspect: f64,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            context_ls: 3.0,
            info: 3.0,
            stats: 10.0,
            inspect: 30.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Thresholds {
    /// Amarelo a partir de (%).
    pub warn: f64,
    /// Vermelho a partir de (%).
    pub crit: f64,
    /// CPU absoluta amarela a partir de (%).
    pub cpu_abs_warn: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            warn: 50.0,
            crit: 85.0,
            cpu_abs_warn: 50.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Layout {
    /// Largura abaixo da qual usa layout ultra-compacto.
    pub compact_width: u16,
    /// Largura abaixo da qual usa layout compacto.
    pub wide_width: u16,
    /// Linhas reservadas (header + footer) ao paginar.
    pub reserved_rows: u16,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            compact_width: 100,
            wide_width: 160,
            reserved_rows: 8,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Display {
    /// Formato de bytes: `binary` (GiB/MiB) ou `decimal` (GB/MB).
    pub byte_format: String,
    /// Casas decimais nos formatos humanos.
    pub precision: usize,
    /// Formato do relógio no header (`chrono`-like simplificado HH:MM:SS).
    pub clock_format: String,
    /// Mostra badge `api/cli` por contexto no header.
    pub show_transport_badge: bool,
}

impl Default for Display {
    fn default() -> Self {
        Self {
            byte_format: "binary".to_string(),
            precision: 2,
            clock_format: "%H:%M:%S".to_string(),
            show_transport_badge: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Keybindings {
    pub quit: String,
    pub filter: String,
    pub sort_id: String,
    pub sort_cpu: String,
    pub sort_cpu_rel: String,
    pub sort_cpu_lim: String,
    pub sort_mem_used: String,
    pub sort_mem_rel: String,
    pub sort_mem_lim: String,
    pub sort_net_rx: String,
    pub sort_net_tx: String,
    pub sort_net: String,
    pub sort_block_read: String,
    pub sort_block_write: String,
    pub sort_block: String,
    pub sort_pids: String,
    pub sort_name: String,
    pub sort_context: String,
    pub toggle_order: String,
    pub next_context: String,
    pub refresh_inc: String,
    pub refresh_dec: String,
    pub reload_config: String,
}

impl Default for Keybindings {
    fn default() -> Self {
        Self {
            quit: "q".to_string(),
            filter: "f".to_string(),
            sort_id: "i".to_string(),
            sort_cpu: "c".to_string(),
            sort_cpu_rel: "x".to_string(),
            sort_cpu_lim: "l".to_string(),
            sort_mem_used: "u".to_string(),
            sort_mem_rel: "m".to_string(),
            sort_mem_lim: "k".to_string(),
            sort_net_rx: "1".to_string(),
            sort_net_tx: "2".to_string(),
            sort_net: "r".to_string(),
            sort_block_read: "3".to_string(),
            sort_block_write: "4".to_string(),
            sort_block: "d".to_string(),
            sort_pids: "p".to_string(),
            sort_name: "n".to_string(),
            sort_context: "o".to_string(),
            toggle_order: "s".to_string(),
            next_context: "tab".to_string(),
            refresh_inc: "+".to_string(),
            refresh_dec: "-".to_string(),
            reload_config: "R".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Columns {
    /// Colunas do layout ultra-compacto (< compact_width).
    pub ultra_compact: Vec<String>,
    /// Colunas do layout compacto (< wide_width).
    pub compact: Vec<String>,
    /// Colunas do layout completo.
    pub full: Vec<String>,
}

impl Default for Columns {
    fn default() -> Self {
        Self {
            ultra_compact: vec![
                "id", "name", "cpu_rel", "mem_rel", "cpu", "mem_used", "net", "block", "pids",
            ]
            .into_iter()
            .map(str::to_string)
            .collect(),
            compact: vec![
                "id", "name", "cpu_rel", "mem_rel", "cpu", "mem_used", "cpu_lim", "mem_lim", "net",
                "block", "pids",
            ]
            .into_iter()
            .map(str::to_string)
            .collect(),
            full: vec![
                "id",
                "name",
                "cpu_rel",
                "mem_rel",
                "cpu",
                "mem_used",
                "cpu_lim",
                "mem_lim",
                "net_rx",
                "net_tx",
                "block_read",
                "block_write",
                "pids",
            ]
            .into_iter()
            .map(str::to_string)
            .collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AppConfig {
    /// Intervalo de coleta (s). Clamp aplicado em `main.rs`.
    pub refresh_delay: f64,
    /// Coluna inicial de sort (uma das 16 de `SORT_COLUMNS`).
    pub sort: String,
    /// Inicia ascendente quando true.
    pub ascending: bool,
    /// Filtro inicial de nome.
    pub filter: String,
    /// Contexto inicial (`all` ou nome).
    pub context: String,
    pub timeouts: Timeouts,
    pub thresholds: Thresholds,
    pub layout: Layout,
    pub display: Display,
    pub keybindings: Keybindings,
    pub columns: Columns,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            refresh_delay: 2.0,
            sort: "cpu".to_string(),
            ascending: false,
            filter: String::new(),
            context: "all".to_string(),
            timeouts: Timeouts::default(),
            thresholds: Thresholds::default(),
            layout: Layout::default(),
            display: Display::default(),
            keybindings: Keybindings::default(),
            columns: Columns::default(),
        }
    }
}

impl AppConfig {
    /// Monta figment com a hierarquia default < arquivo < env < CLI.
    pub fn figment(cli: &Cli, config_path: &std::path::Path) -> Figment {
        let mut cli_map: HashMap<String, figment::value::Value> = HashMap::new();
        if let Some(d) = cli.delay {
            cli_map.insert("refresh_delay".to_string(), figment::value::Value::from(d));
        }
        if let Some(ref s) = cli.sort {
            cli_map.insert("sort".to_string(), figment::value::Value::from(s.clone()));
        }
        if cli.asc {
            cli_map.insert("ascending".to_string(), figment::value::Value::from(true));
        }
        if let Some(ref f) = cli.filter {
            cli_map.insert("filter".to_string(), figment::value::Value::from(f.clone()));
        }
        if let Some(ref c) = cli.context {
            cli_map.insert(
                "context".to_string(),
                figment::value::Value::from(c.clone()),
            );
        }
        Figment::from(Serialized::defaults(AppConfig::default()))
            .merge(Toml::file(config_path))
            .merge(
                Env::prefixed("DOCKER_TOP_")
                    .split("__")
                    .ignore(&["DELAY", "SORT", "ASC", "FILTER", "CONTEXT", "CONFIG"]),
            )
            .merge(Serialized::defaults(cli_map))
    }

    /// Carrega e valida (clamp + sort conhecido).
    pub fn load(cli: &Cli, config_path: &std::path::Path) -> anyhow::Result<Self> {
        let mut cfg: AppConfig = Self::figment(cli, config_path).extract().map_err(|e| {
            anyhow::anyhow!("config inválida (deny_unknown_fields, ver config.example.toml): {e}")
        })?;
        cfg.refresh_delay = cfg.refresh_delay.clamp(0.5, 10.0);
        if !crate::model::SORT_COLUMNS.contains(&cfg.sort.as_str()) {
            anyhow::bail!(
                "sort inicial desconhecido '{}'. Válidos: {:?}",
                cfg.sort,
                crate::model::SORT_COLUMNS
            );
        }
        Ok(cfg)
    }

    /// Hot reload: relê arquivo+env mantendo overrides da CLI.
    pub fn reload(&mut self, cli: &Cli, config_path: &std::path::Path) -> anyhow::Result<()> {
        let fresh = Self::load(cli, config_path)?;
        *self = fresh;
        Ok(())
    }
}

/// Gera o `config.example.toml` versionado a partir do default serializado.
pub fn generate_example_file(path: &std::path::Path) -> anyhow::Result<()> {
    let cfg = AppConfig::default();
    let mut doc = String::from(
        "# docker-top-rs — exemplo de configuração (copie p/ ~/.config/docker-top/config.toml)\n\
         # Hierarquia: default < arquivo < env DOCKER_TOP_* (nested com __) < CLI\n\
         # Hot reload: tecla R ou SIGHUP. Campos desconhecidos = erro (deny_unknown_fields).\n\n",
    );
    doc.push_str(&toml::to_string_pretty(&cfg)?);
    doc.push_str(
        "\n# Env exemplos:\n\
         #   DOCKER_TOP_REFRESH_DELAY=1.5\n\
         #   DOCKER_TOP_SORT=cpu_rel\n\
         #   DOCKER_TOP_TIMEOUTS__STATS=10\n\
         # CLI: --delay 2 --sort cpu_rel --filter api --context all --once\n",
    );
    std::fs::write(path, doc)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;

    fn test_cli() -> Cli {
        Cli {
            delay: None,
            sort: None,
            asc: false,
            filter: None,
            context: None,
            once: false,
            config: None,
            generate_config_example: false,
        }
    }

    #[test]
    fn default_load_without_file_ok() {
        let cli = test_cli();
        let cfg = AppConfig::load(&cli, &PathBuf::from("/nonexistent/docker-top.toml")).unwrap();
        assert!((cfg.refresh_delay - 2.0).abs() < f64::EPSILON);
        assert_eq!(cfg.sort, "cpu");
        assert!((cfg.timeouts.context_ls - 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn cli_overrides_win() {
        let cli = Cli {
            delay: Some(1.5),
            sort: Some("cpu_rel".to_string()),
            asc: true,
            filter: Some("web".to_string()),
            context: Some("remote".to_string()),
            ..test_cli()
        };
        let cfg = AppConfig::load(&cli, &PathBuf::from("/nonexistent/x.toml")).unwrap();
        assert!((cfg.refresh_delay - 1.5).abs() < f64::EPSILON);
        assert_eq!(cfg.sort, "cpu_rel");
        assert!(cfg.ascending);
        assert_eq!(cfg.filter, "web");
        assert_eq!(cfg.context, "remote");
    }

    #[test]
    fn unknown_field_rejected() {
        let dir = std::env::temp_dir().join(format!("dt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("bad.toml");
        std::fs::write(&p, "refresh_delay = 2.0\ncampo_inexistente = 1\n").unwrap();
        let cli = test_cli();
        assert!(AppConfig::load(&cli, &p).is_err());
    }

    #[test]
    fn toml_file_overrides_default() {
        let dir = std::env::temp_dir().join(format!("dt2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("ok.toml");
        std::fs::write(&p, "[thresholds]\nwarn = 60.0\ncrit = 90.0\n").unwrap();
        let cli = test_cli();
        let cfg = AppConfig::load(&cli, &p).unwrap();
        assert!((cfg.thresholds.warn - 60.0).abs() < f64::EPSILON);
    }

    #[test]
    fn example_roundtrip_loads_clean() {
        let dir = std::env::temp_dir().join(format!("dtex-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.example.toml");
        generate_example_file(&p).unwrap();
        let cli = test_cli();
        let cfg = AppConfig::load(&cli, &p).unwrap();
        assert!((cfg.refresh_delay - 2.0).abs() < f64::EPSILON);
        assert_eq!(cfg.sort, "cpu");
    }

    #[test]
    fn reload_applies_file_changes() {
        let dir = std::env::temp_dir().join(format!("dtrel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("c.toml");
        std::fs::write(&p, "refresh_delay = 2.0\n").unwrap();
        let cli = test_cli();
        let mut cfg = AppConfig::load(&cli, &p).unwrap();
        std::fs::write(&p, "refresh_delay = 4.0\n").unwrap();
        cfg.reload(&cli, &p).unwrap();
        assert!((cfg.refresh_delay - 4.0).abs() < f64::EPSILON);
    }

    #[test]
    fn invalid_sort_rejected() {
        let dir = std::env::temp_dir().join(format!("dtbad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("c.toml");
        std::fs::write(&p, "sort = \"nope\"\n").unwrap();
        let cli = test_cli();
        assert!(AppConfig::load(&cli, &p).is_err());
    }

    #[test]
    fn helper_paths_nonempty() {
        assert!(default_config_path()
            .to_string_lossy()
            .contains("docker-top"));
        assert_eq!(example_config_path(), PathBuf::from("config.example.toml"));
    }
}
