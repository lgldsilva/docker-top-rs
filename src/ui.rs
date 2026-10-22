//! TUI `ratatui+crossterm`: tick de coleta != render (event-driven).
//!
//! - 3 layouts responsivos (larguras do config, default 100/160).
//! - Sort 16 colunas, filtro substring, `Tab` troca contexto.
//! - `--once`: tabela texto sem alt-screen (schema comparável ao Python).
//! - Badge `api/cli` por contexto (decisão 1x no startup, ver `transport.rs`).

use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use ratatui::Frame;

use crate::config::AppConfig;
use crate::model::{normalize_sort, sort_snapshots, visible_columns, ContainerSnapshot};
use crate::parsers::format_bytes_human_fmt;
use crate::transport::DockerContext;

#[derive(Debug, Clone)]
pub struct AppState {
    pub rows: Vec<ContainerSnapshot>,
    pub errors: Vec<String>,
    pub sort_by: String,
    pub desc: bool,
    pub filter: String,
    pub selected_context: String,
    pub contexts: Vec<DockerContext>,
    pub total_cores: u32,
    pub total_mem: u64,
    pub editing_filter: bool,
    pub filter_draft: String,
    /// Fixa o relógio do header (testes/snapshot). `None` = hora atual.
    pub clock_override: Option<String>,
}

impl AppState {
    pub fn visible_rows(&self, cfg: &AppConfig, term_width: u16) -> Vec<ContainerSnapshot> {
        let show_ctx = self.selected_context == "all";
        let cols_cfg = &cfg.columns;
        let mut cols = visible_columns(
            term_width,
            cfg.layout.compact_width,
            cfg.layout.wide_width,
            cols_cfg,
        );
        if show_ctx && !cols.contains(&"context".to_string()) {
            if let Some(pos) = cols.iter().position(|c| c == "id") {
                cols.insert(pos + 1, "context".to_string());
            }
        }
        let sort = normalize_sort(&self.sort_by, &cols);
        let mut rows: Vec<ContainerSnapshot> = self
            .rows
            .iter()
            .filter(|r| {
                if self.selected_context != "all" && r.context != self.selected_context {
                    return false;
                }
                if self.filter.is_empty() {
                    return true;
                }
                r.name.to_lowercase().contains(&self.filter.to_lowercase())
            })
            .cloned()
            .collect();
        sort_snapshots(&mut rows, &sort, self.desc);
        // pagina: reserva header+footer
        let max_rows = 4096usize; // TUI usa scroll natural; --once limita por altura
        rows.truncate(max_rows);
        rows
    }
}

/// Header KPI (texto puro, reutilizado por TUI e --once).
pub fn summary_text(state: &AppState, cfg: &AppConfig) -> String {
    let total: usize = state.rows.len();
    let running = state
        .rows
        .iter()
        .filter(|c| c.cpu > 0.0 || c.pids > 0)
        .count();
    let filtered = state.visible_rows(cfg, 200).len();
    let tot_cpu: f64 = state.rows.iter().map(|c| c.cpu).sum();
    let tot_mem: f64 = state.rows.iter().map(|c| c.mem_used).sum();
    let (rx, tx, br, bw) = state.rows.iter().fold((0.0, 0.0, 0.0, 0.0), |a, c| {
        (
            a.0 + c.net_rx,
            a.1 + c.net_tx,
            a.2 + c.block_read,
            a.3 + c.block_write,
        )
    });
    let p = cfg.display.precision;
    let dec = cfg.display.byte_format == "decimal";
    let badges: Vec<String> = state
        .contexts
        .iter()
        .map(|c| format!("{}:{}", c.name, c.transport.badge()))
        .collect();
    let badge_str = if cfg.display.show_transport_badge && !badges.is_empty() {
        format!(" [{}]", badges.join(" "))
    } else {
        String::new()
    };
    format!(
        "Ctx: {} ({}T {}A {}F) | CPU: {:.1}%/{}% | Mem: {}/{} | Net: ↓{} ↑{} | Disco: R {} W {} | Refresh: {:.1}s{}",
        state.selected_context,
        total,
        running,
        filtered,
        tot_cpu,
        state.total_cores * 100,
        format_bytes_human_fmt(tot_mem, p, dec),
        format_bytes_human_fmt(state.total_mem as f64, p, dec),
        format_bytes_human_fmt(rx, p, dec),
        format_bytes_human_fmt(tx, p, dec),
        format_bytes_human_fmt(br, p, dec),
        format_bytes_human_fmt(bw, p, dec),
        cfg.refresh_delay,
        badge_str,
    )
}

fn cell_style_rel(v: f64, cfg: &AppConfig) -> Style {
    if v > cfg.thresholds.crit {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else if v > cfg.thresholds.warn {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::Green)
    }
}

/// Títulos curtos p/ modos estreitos (igual ao Python: CPR/MER/NET/DISC/PID).
fn short_title(key: &str, mode: u8) -> String {
    if mode >= 2 {
        return key.to_uppercase();
    }
    match key {
        "id" => "ID".to_string(),
        "context" => "CTX".to_string(),
        "name" => "NOME".to_string(),
        "cpu_rel" => "CPR".to_string(),
        "mem_rel" => "MER".to_string(),
        "cpu" => "CPU".to_string(),
        "mem_used" => "MEM".to_string(),
        "cpu_lim" => "LIM".to_string(),
        "mem_lim" => "LIM".to_string(),
        "net" => "NET".to_string(),
        "net_rx" => "NET".to_string(),
        "net_tx" => "NET".to_string(),
        "block" => "DISC".to_string(),
        "block_read" => "DISC".to_string(),
        "block_write" => "DISC".to_string(),
        "pids" => "PID".to_string(),
        other => other.to_uppercase(),
    }
}

fn header_title(base: &str, key: &str, sort_by: &str, desc: bool) -> String {
    if normalize_sort(sort_by, &[sort_by.to_string()]) == key || sort_by == key {
        let arrow = if desc { "▼" } else { "▲" };
        format!("{base} {arrow}")
    } else {
        base.to_string()
    }
}

/// Colunas efetivas p/ render (inclui context quando `all`).
pub fn effective_columns(state: &AppState, cfg: &AppConfig, width: u16) -> Vec<String> {
    let mut cols = visible_columns(
        width,
        cfg.layout.compact_width,
        cfg.layout.wide_width,
        &cfg.columns,
    );
    if state.selected_context == "all" && !cols.contains(&"context".to_string()) {
        if let Some(pos) = cols.iter().position(|c| c == "id") {
            cols.insert(pos + 1, "context".to_string());
        }
    }
    cols
}

fn col_width(key: &str, mode: u8) -> Constraint {
    // mode: 0 ultra-compacto, 1 compacto, 2 full.
    match (key, mode) {
        ("id", 0) => Constraint::Length(8),
        ("id", 1) => Constraint::Length(10),
        ("id", _) => Constraint::Length(12),
        ("context", 0) => Constraint::Length(8),
        ("context", 1) => Constraint::Length(12),
        ("context", _) => Constraint::Length(14),
        ("name", 0) => Constraint::Min(6),
        ("name", _) => Constraint::Min(8),
        ("cpu_rel" | "mem_rel", 0) => Constraint::Length(6),
        ("cpu_rel" | "mem_rel", _) => Constraint::Length(8),
        ("cpu", 0) => Constraint::Length(6),
        ("cpu", _) => Constraint::Length(8),
        ("pids", 0) => Constraint::Length(4),
        ("pids", _) => Constraint::Length(5),
        ("mem_used" | "net" | "block", 0) => Constraint::Length(7),
        ("mem_used" | "mem_lim" | "net_rx" | "net_tx" | "block_read" | "block_write", 2) => {
            Constraint::Length(11)
        }
        ("mem_used" | "mem_lim" | "net" | "block", 1) => Constraint::Length(10),
        (_, 0) => Constraint::Length(8),
        (_, 1) => Constraint::Length(9),
        (_, _) => Constraint::Length(9),
    }
}

fn row_cells(
    s: &ContainerSnapshot,
    cols: &[String],
    cfg: &AppConfig,
    narrow: bool,
) -> Vec<Cell<'static>> {
    let p = cfg.display.precision;
    let dec = cfg.display.byte_format == "decimal";
    let cpu_lim = if s.has_cpu_limit {
        format!("{:.2}c", s.cpu_limit)
    } else {
        format!("{:.1}c (H)", s.cpu_limit)
    };
    let mem_lim = if s.has_mem_limit {
        format_bytes_human_fmt(s.mem_limit, p, dec)
    } else {
        format!("{} (H)", format_bytes_human_fmt(s.mem_limit, p, dec))
    };
    let mut out = Vec::new();
    for c in cols {
        let txt = match c.as_str() {
            "id" => s.id.chars().take(12).collect::<String>(),
            "context" => s.context.clone(),
            "name" => s.name.clone(),
            "cpu_rel" => format!("{:.1}%", s.cpu_rel),
            "mem_rel" => format!("{:.1}%", s.mem_rel),
            "cpu" => s.raw_cpu.clone(),
            "mem_used" => format_bytes_human_fmt(s.mem_used, p, dec),
            "cpu_lim" => cpu_lim.clone(),
            "mem_lim" => mem_lim.clone(),
            "net_rx" => format_bytes_human_fmt(s.net_rx, p, dec),
            "net_tx" => format_bytes_human_fmt(s.net_tx, p, dec),
            // Modos estreitos: total único em vez de "rx / tx" (não cabe).
            "net" if narrow => format_bytes_human_fmt(s.net_total(), p, dec),
            "net" => s.raw_net.clone(),
            "block_read" => format_bytes_human_fmt(s.block_read, p, dec),
            "block_write" => format_bytes_human_fmt(s.block_write, p, dec),
            "block" if narrow => format_bytes_human_fmt(s.block_total(), p, dec),
            "block" => s.raw_block.clone(),
            "pids" => s.pids.to_string(),
            _ => String::new(),
        };
        let style = match c.as_str() {
            "cpu_rel" => cell_style_rel(s.cpu_rel, cfg),
            "mem_rel" => cell_style_rel(s.mem_rel, cfg),
            "cpu" => {
                if s.cpu >= cfg.thresholds.crit {
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
                } else if s.cpu >= cfg.thresholds.cpu_abs_warn {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default()
                }
            }
            _ => Style::default(),
        };
        out.push(Cell::from(txt).style(style));
    }
    out
}

pub fn render(frame: &mut Frame, state: &AppState, cfg: &AppConfig) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(3),
        ])
        .split(area);
    let clock = state
        .clock_override
        .clone()
        .unwrap_or_else(|| chrono_like_now(&cfg.display.clock_format));
    let header = Paragraph::new(summary_text(state, cfg)).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!("Docker Top RS — {clock}")),
    );
    frame.render_widget(header, chunks[0]);

    let cols = effective_columns(state, cfg, area.width);
    let sort = normalize_sort(&state.sort_by, &cols);
    // modo 0/1/2 = ultra-compacto / compacto / full (larguras do config).
    let mode: u8 = if area.width < cfg.layout.compact_width {
        0
    } else if area.width < cfg.layout.wide_width {
        1
    } else {
        2
    };
    let header_cells: Vec<Cell> = cols
        .iter()
        .map(|c| {
            let base = short_title(c, mode);
            Cell::from(header_title(&base, c, &sort, state.desc)).style(
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            )
        })
        .collect();
    let widths: Vec<Constraint> = cols.iter().map(|c| col_width(c, mode)).collect();
    let mut rows: Vec<ContainerSnapshot> = state.visible_rows(cfg, area.width);
    // reserva linhas p/ não esconder o footer
    let max_rows = (chunks[1].height as usize).saturating_sub(2).max(1);
    rows.truncate(max_rows);
    let narrow = mode < 2;
    let body: Vec<Row> = rows
        .iter()
        .map(|s| Row::new(row_cells(s, &cols, cfg, narrow)))
        .collect();
    let table = Table::new(body, widths)
        .header(Row::new(header_cells))
        .block(Block::default().borders(Borders::ALL).title("containers"));
    frame.render_widget(table, chunks[1]);

    let filter_info = if state.editing_filter {
        format!("editando: {}_", state.filter_draft)
    } else if state.filter.is_empty() {
        "nenhum (f p/ filtrar)".to_string()
    } else {
        format!("ativo: '{}'", state.filter)
    };
    let footer_txt = format!(
        "filtro {filter_info} | Tab ctx | ←/→ col ↑/↓ ordem | R reload | +/- refresh | q sair"
    );
    let footer = Paragraph::new(Line::from(vec![Span::raw(footer_txt)]))
        .block(Block::default().borders(Borders::ALL).title("atalhos"));
    frame.render_widget(footer, chunks[2]);
}

/// Relógio sem nova dependência (respeita `clock_format` com %H %M %S %Y %m %d).
fn chrono_like_now(fmt: &str) -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // UTC base; suficiente p/ header sem chrono. Dias simplificados (1970-01-01 + secs).
    let h = (secs / 3600) % 24;
    let m = (secs / 60) % 60;
    let s = secs % 60;
    let days = secs / 86400;
    // aproximação civil p/ %Y-%m-%d (algoritmo dias -> data)
    let (y, mo, d) = days_to_ymd(days);
    fmt.replace("%H", &format!("{h:02}"))
        .replace("%M", &format!("{m:02}"))
        .replace("%S", &format!("{s:02}"))
        .replace("%Y", &format!("{y:04}"))
        .replace("%m", &format!("{mo:02}"))
        .replace("%d", &format!("{d:02}"))
}

fn days_to_ymd(days: u64) -> (i32, u32, u32) {
    // Howard Hinnant days_from_civil inverso, epoch 1970-01-01.
    let z = days as i64 + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    ((if m <= 2 { y + 1 } else { y }) as i32, m, d)
}

/// `--once`: tabela texto (stdout, sem alt-screen). Schema estável p/ golden.
pub fn render_once_text(state: &AppState, cfg: &AppConfig, width: u16) -> String {
    let cols = effective_columns(state, cfg, width);
    let sort = normalize_sort(&state.sort_by, &cols);
    let mut rows = state.visible_rows(cfg, width);
    sort_snapshots(&mut rows, &sort, state.desc);
    // --once respeita reserved_rows: limita por altura real do terminal.
    if let Ok((_, h)) = crossterm::terminal::size() {
        let max = (h as usize)
            .saturating_sub(cfg.layout.reserved_rows as usize + 2)
            .max(1);
        rows.truncate(max);
    }
    let p = cfg.display.precision;
    let dec = cfg.display.byte_format == "decimal";
    let mut out = String::new();
    out.push_str(&format!("{}\n", summary_text(state, cfg)));
    out.push_str(&cols.join("\t"));
    out.push('\n');
    for s in &rows {
        let cells: Vec<String> = cols
            .iter()
            .map(|c| match c.as_str() {
                "id" => s.id.chars().take(12).collect(),
                "context" => s.context.clone(),
                "name" => s.name.clone(),
                "cpu_rel" => format!("{:.1}", s.cpu_rel),
                "mem_rel" => format!("{:.1}", s.mem_rel),
                "cpu" => format!("{:.2}", s.cpu),
                "mem_used" => format_bytes_human_fmt(s.mem_used, p, dec),
                "cpu_lim" => format!("{:.2}", s.cpu_limit),
                "mem_lim" => format_bytes_human_fmt(s.mem_limit, p, dec),
                "net_rx" => format_bytes_human_fmt(s.net_rx, p, dec),
                "net_tx" => format_bytes_human_fmt(s.net_tx, p, dec),
                "net" => format_bytes_human_fmt(s.net_total(), p, dec),
                "block_read" => format_bytes_human_fmt(s.block_read, p, dec),
                "block_write" => format_bytes_human_fmt(s.block_write, p, dec),
                "block" => format_bytes_human_fmt(s.block_total(), p, dec),
                "pids" => s.pids.to_string(),
                _ => String::new(),
            })
            .collect();
        out.push_str(&cells.join("\t"));
        out.push('\n');
    }
    if !state.errors.is_empty() {
        out.push_str("errors:\n");
        for e in &state.errors {
            out.push_str(&format!("  - {e}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ContainerSnapshot;
    use crate::transport::{DockerContext, Transport};

    pub fn sample_state() -> (AppState, AppConfig) {
        let cfg = AppConfig::default();
        let mk = |id: &str, name: &str, ctx: &str| {
            ContainerSnapshot::from_raw(
                id.into(),
                name.into(),
                "5.0%".into(),
                "100MiB / 1GiB".into(),
                "9.7%".into(),
                "1MiB / 1MiB".into(),
                "1MiB / 1MiB".into(),
                "3".into(),
                ctx.into(),
                "cli".into(),
                None,
                4,
                8 * 1024 * 1024 * 1024,
            )
        };
        let state = AppState {
            rows: vec![
                mk("aaa111111111", "web", "default"),
                mk("bbb222222222", "db", "homeserver"),
            ],
            errors: vec![],
            sort_by: "cpu".into(),
            desc: true,
            filter: String::new(),
            selected_context: "all".into(),
            contexts: vec![
                DockerContext {
                    name: "default".into(),
                    endpoint: "unix:///var/run/docker.sock".into(),
                    transport: Transport::Api,
                },
                DockerContext {
                    name: "homeserver".into(),
                    endpoint: "ssh://x".into(),
                    transport: Transport::Cli,
                },
            ],
            total_cores: 8,
            total_mem: 16 * 1024 * 1024 * 1024,
            editing_filter: false,
            filter_draft: String::new(),
            clock_override: Some("12:00:00".to_string()),
        };
        (state, cfg)
    }

    #[test]
    fn once_text_has_schema_columns() {
        let (state, cfg) = sample_state();
        let txt = render_once_text(&state, &cfg, 200);
        assert!(txt.contains("id\t"));
        assert!(txt.contains("cpu_rel"));
        assert!(txt.contains("web"));
    }

    #[test]
    fn summary_has_badges() {
        let (state, cfg) = sample_state();
        let s = summary_text(&state, &cfg);
        assert!(s.contains("default:api"));
        assert!(s.contains("homeserver:cli"));
    }

    #[test]
    fn render_error_and_filter_edit_states() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let (mut state, cfg) = sample_state();
        state.errors = vec!["Timeout ao conectar ao contexto 'x'".to_string()];
        state.editing_filter = true;
        state.filter_draft = "we".to_string();
        let backend = TestBackend::new(170, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &state, &cfg)).unwrap();
        let text = terminal.backend().to_string();
        assert!(text.contains("editando"));
    }

    #[test]
    fn effective_columns_hides_context_when_single() {
        let (mut state, cfg) = sample_state();
        state.selected_context = "default".to_string();
        let cols = effective_columns(&state, &cfg, 200);
        assert!(!cols.contains(&"context".to_string()));
        let rows = state.visible_rows(&cfg, 200);
        assert!(rows.iter().all(|r| r.context == "default"));
    }
}
