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
use crate::logging::days_to_ymd;
use crate::model::{normalize_sort, sort_snapshots, visible_columns, ContainerSnapshot};
use crate::parsers::format_bytes_human_fmt;
use crate::transport::DockerContext;

#[derive(Debug, Clone)]
pub struct AppState {
    pub rows: Vec<ContainerSnapshot>,
    /// Erros do último tick como `(contexto, mensagem)` — renderizados no
    /// footer da TUI (antes eram engolidos: remoto fora parecia "vazio").
    pub errors: Vec<(String, String)>,
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
        let cols = effective_columns(self, cfg, term_width);
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
    let errored: std::collections::HashSet<&str> =
        state.errors.iter().map(|(c, _)| c.as_str()).collect();
    let badges: Vec<String> = state
        .contexts
        .iter()
        .map(|c| {
            // `!` marca contexto com erro no último tick (visível também no --once)
            format!(
                "{}:{}{}",
                c.name,
                c.transport.badge(),
                if errored.contains(c.name.as_str()) {
                    "!"
                } else {
                    ""
                }
            )
        })
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

/// Banda de layout pela largura: 0 ultra-compacto, 1 compacto, 2 full.
/// Limiares vêm do config — único ponto de verdade (render, colunas e testes).
fn layout_mode(width: u16, compact_width: u16, wide_width: u16) -> u8 {
    if width < compact_width {
        0
    } else if width < wide_width {
        1
    } else {
        2
    }
}

/// Colunas efetivas p/ render (inclui context quando `all`). Em painéis
/// ultra-estreitos (< compact_width) retira as colunas finais até o conjunto
/// caber com a seta de ordenação — antes o ratatui espremia/truncava os
/// títulos e a seta do sort ativo sumia (inclusive nas duas primeiras
/// colunas: ID/CTX), reporte 13/11 pós-PR #5.
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
    let mode = layout_mode(width, cfg.layout.compact_width, cfg.layout.wide_width);
    // inner do bloco da tabela = width - 2 bordas; +1 de gap por coluna.
    // Só o ultra-compacto perde colunas: lá o título+seta ocupa quase toda a
    // largura (slack ≤ 2) e o solver comia a seta ao espremer. No compacto/full
    // os títulos curtos têm slack ≥ 3 e a seta sobrevive ao squeeze (coberto
    // pelo teste de regressão em todas as bandas).
    if mode == 0 {
        let avail = width.saturating_sub(2);
        let needed = |cols: &[String]| -> u16 {
            cols.iter()
                .map(|c| min_col_width(c, mode))
                .sum::<u16>()
                .saturating_add(cols.len().saturating_sub(1) as u16)
        };
        // 1ª fase: derruba colunas finais preservando o trio id/[ctx]/name.
        while cols.len() > 3 && needed(&cols) > avail {
            cols.pop();
        }
        // 2ª fase (issue #8): painel abaixo do mínimo do trio (id+ctx+name
        // ≈ 26 cols no ctx=all, id+name+1 ≈ 24 no single) — derruba o
        // `context` e segue até o par id+name (mínimo absoluto, ≥ 17 cols)
        // caber; o piso antigo `len > 3` mantinha o estouro e o solver
        // espremia os Length até sumir com a seta.
        while cols.len() > 2 && needed(&cols) > avail {
            if let Some(pos) = cols.iter().position(|c| c == "context") {
                cols.remove(pos);
            } else {
                cols.pop();
            }
        }
    }
    cols
}

/// Largura mínima de uma coluna (o que o `Length`/`Min` de `col_width` exige).
fn min_col_width(key: &str, mode: u8) -> u16 {
    match col_width(key, mode) {
        Constraint::Length(n) | Constraint::Min(n) => n,
        _ => 8,
    }
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
        ("cpu_rel" | "mem_rel", 1) => Constraint::Length(8),
        // full: "CPU_REL ▼"/"MEM_REL ▼" = 9 chars — 8 truncava a seta.
        ("cpu_rel" | "mem_rel", 2) => Constraint::Length(9),
        ("cpu", 0) => Constraint::Length(6),
        ("cpu", _) => Constraint::Length(8),
        // ultra: "PID ▼" = 5 chars; compacto/full: 6 chars.
        ("pids", 0) => Constraint::Length(5),
        ("pids", _) => Constraint::Length(6),
        ("mem_used" | "net" | "block", 0) => Constraint::Length(7),
        ("mem_used" | "mem_lim" | "net_rx" | "net_tx", 2) => Constraint::Length(11),
        // full: títulos + seta — 11 truncava ("BLOCK_READ ▼"=12, "BLOCK_WRITE ▼"=13).
        ("block_read", 2) => Constraint::Length(12),
        ("block_write", 2) => Constraint::Length(13),
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
    // footer cresce 1 linha quando há erros p/ exibi-los (antes: silêncio total)
    let footer_h: u16 = if state.errors.is_empty() { 3 } else { 4 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(footer_h),
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
    let mode = layout_mode(area.width, cfg.layout.compact_width, cfg.layout.wide_width);
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
    let mut footer_lines = vec![Line::from(Span::raw(footer_txt))];
    if !state.errors.is_empty() {
        let joined = state
            .errors
            .iter()
            .map(|(c, m)| {
                if c.is_empty() {
                    m.clone()
                } else {
                    format!("{c}: {m}")
                }
            })
            .collect::<Vec<_>>()
            .join(" | ");
        footer_lines.push(Line::from(Span::styled(
            format!("⚠ {joined}"),
            Style::default().fg(Color::Red),
        )));
    }
    let footer =
        Paragraph::new(footer_lines).block(Block::default().borders(Borders::ALL).title("atalhos"));
    frame.render_widget(footer, chunks[2]);
}

/// Relógio sem nova dependência (respeita `clock_format` com %H %M %S %Y %m %d).
/// UTC base; suficiente p/ header sem chrono. Dias simplificados (1970-01-01 + secs).
fn chrono_like_now(fmt: &str) -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let h = (secs / 3600) % 24;
    let m = (secs / 60) % 60;
    let s = secs % 60;
    let days = secs / 86400;
    // aproximação civil p/ %Y-%m-%d (algoritmo dias -> data em logging::days_to_ymd)
    let (y, mo, d) = days_to_ymd(days);
    fmt.replace("%H", &format!("{h:02}"))
        .replace("%M", &format!("{m:02}"))
        .replace("%S", &format!("{s:02}"))
        .replace("%Y", &format!("{y:04}"))
        .replace("%m", &format!("{mo:02}"))
        .replace("%d", &format!("{d:02}"))
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
        for (c, m) in &state.errors {
            if c.is_empty() {
                out.push_str(&format!("  - {m}\n"));
            } else {
                out.push_str(&format!("  - {c}: {m}\n"));
            }
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

    /// Regressão: em full mode o título longo + seta (ex.: "MEM_REL ▼" = 9
    /// chars) era truncado por col_width (Length(8)) — a seta sumia nas
    /// colunas de título largo (CPU_REL, MEM_REL, BLOCK_READ, BLOCK_WRITE,
    /// PIDS) ao circular com ←/→ ou usar as hotkeys.
    #[test]
    fn header_arrow_visible_for_every_column_at_full_width() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let cases = [
            ("id", "ID"),
            ("context", "CONTEXT"),
            ("name", "NAME"),
            ("cpu_rel", "CPU_REL"),
            ("mem_rel", "MEM_REL"),
            ("cpu", "CPU"),
            ("mem_used", "MEM_USED"),
            ("cpu_lim", "CPU_LIM"),
            ("mem_lim", "MEM_LIM"),
            ("net_rx", "NET_RX"),
            ("net_tx", "NET_TX"),
            ("block_read", "BLOCK_READ"),
            ("block_write", "BLOCK_WRITE"),
            ("pids", "PIDS"),
        ];
        for (key, title) in cases {
            for (desc, arrow) in [(true, "▼"), (false, "▲")] {
                let (mut state, cfg) = sample_state();
                state.sort_by = key.to_string();
                state.desc = desc;
                let backend = TestBackend::new(223, 30);
                let mut terminal = Terminal::new(backend).unwrap();
                terminal.draw(|f| render(f, &state, &cfg)).unwrap();
                let text = terminal.backend().to_string();
                let want = format!("{title} {arrow}");
                assert!(
                    text.contains(&want),
                    "sort_by={key} desc={desc}: '{want}' ausente do header (truncado?)\n{text}"
                );
            }
        }
    }

    /// Regressão (reporte 13/11, pós-PR #5): a seta de ordenação seguia
    /// sumindo em painéis estreitos — em ultra-compacto a tabela estourava e o
    /// ratatui truncava o fim do título (a seta), inclusive nas DUAS PRIMEIRAS
    /// colunas (ID em ≤ 23 cols, CTX em ≤ 26) e em CPR/MER/CPU/MEM/NET/DISC/PID
    /// até ~68 cols. O PR #5 só cobriu o full mode a 223 cols.
    /// Invariante: em toda largura onde as 3 colunas mínimas cabem, TODA coluna
    /// visível exibe a seta quando é a coluna ordenada.
    #[test]
    fn header_arrow_visible_for_every_visible_column_at_any_width() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        // amostra as bandas: faixa estreita crítica inteira (26..=75), fronteiras
        // de banda ±1 e algumas larguras full (incl. 223, geometria do usuário).
        let mut widths: Vec<u16> = (26..=75).collect();
        widths.extend([76, 99, 100, 101, 158, 159, 160, 161, 200, 223, 250]);
        for ctx_mode in ["all", "default"] {
            for w in &widths {
                let w = *w;
                let (mut state, cfg) = sample_state();
                state.selected_context = ctx_mode.to_string();
                let cols = effective_columns(&state, &cfg, w);
                let mode = layout_mode(w, cfg.layout.compact_width, cfg.layout.wide_width);
                for key in &cols {
                    state.sort_by = key.clone();
                    state.desc = true;
                    let mut terminal = Terminal::new(TestBackend::new(w, 30)).unwrap();
                    terminal.draw(|f| render(f, &state, &cfg)).unwrap();
                    let text = terminal.backend().to_string();
                    let want = format!("{} ▼", short_title(key, mode));
                    assert!(
                        text.contains(&want),
                        "ctx={ctx_mode} w={w} sort={key}: '{want}' ausente do header\n{text}"
                    );
                }
            }
        }
    }

    /// Regressão (issue #8): abaixo do mínimo do trio id+[ctx]+name (~26 cols
    /// no ctx=all) o piso `cols.len() > 3` mantinha um conjunto que não cabia —
    /// o solver espremia os Length e a seta sumia de novo (ID perdia em ≤ 23,
    /// CTX em ≤ 25 com ctx=all). Invariante: em toda largura onde o par
    /// id+name cabe (≥ 17 cols), TODA coluna visível exibe a seta; abaixo do
    /// mínimo do trio o `context` cai e o par id+name permanece.
    #[test]
    fn header_arrow_visible_below_minimum_trio_width() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        for ctx_mode in ["all", "default"] {
            for w in 17..=25u16 {
                let (mut state, cfg) = sample_state();
                state.selected_context = ctx_mode.to_string();
                let cols = effective_columns(&state, &cfg, w);
                let mode = layout_mode(w, cfg.layout.compact_width, cfg.layout.wide_width);
                assert!(cols.len() >= 2, "ctx={ctx_mode} w={w}: sem colunas mínimas");
                for key in &cols {
                    state.sort_by = key.clone();
                    state.desc = true;
                    let mut terminal = Terminal::new(TestBackend::new(w, 30)).unwrap();
                    terminal.draw(|f| render(f, &state, &cfg)).unwrap();
                    let text = terminal.backend().to_string();
                    let want = format!("{} ▼", short_title(key, mode));
                    assert!(
                        text.contains(&want),
                        "ctx={ctx_mode} w={w} sort={key}: '{want}' ausente do header\n{text}"
                    );
                }
            }
        }
    }

    /// Issue #8: compact_width customizado move a banda ultra — a dedução de
    /// colunas precisa seguir o limiar do config, não o default 100. Em w=50
    /// com compact_width=60 a renderização é ultra (antes seria compacta) e
    /// cada coluna visível mantém a seta com o conjunto derrubado até caber.
    #[test]
    fn custom_compact_width_moves_band_and_keeps_arrows_visible() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let (mut state, mut cfg) = sample_state();
        state.selected_context = "all".to_string();
        cfg.layout.compact_width = 60;
        let w = 50u16;
        let cols = effective_columns(&state, &cfg, w);
        // banda ultra: conjunto derrubado até caber com a seta (não é a lista
        // cheia de 9 colunas do ultra default — o corte precisou agir).
        assert!(
            cols.len() < 10,
            "w={w}: conjunto não foi derrubado: {cols:?}"
        );
        for key in &cols {
            state.sort_by = key.clone();
            state.desc = true;
            let mut terminal = Terminal::new(TestBackend::new(w, 30)).unwrap();
            terminal.draw(|f| render(f, &state, &cfg)).unwrap();
            let text = terminal.backend().to_string();
            let want = format!("{} ▼", short_title(key, 0));
            assert!(
                text.contains(&want),
                "w={w} sort={key}: '{want}' ausente do header (cols={cols:?})\n{text}"
            );
        }
    }

    /// Dedução de banda (helper extraído na issue #8): fronteiras exatas e
    /// limiares customizados do config movem a banda.
    #[test]
    fn layout_mode_band_boundaries_follow_config() {
        assert_eq!(layout_mode(99, 100, 160), 0);
        assert_eq!(layout_mode(100, 100, 160), 1);
        assert_eq!(layout_mode(159, 100, 160), 1);
        assert_eq!(layout_mode(160, 100, 160), 2);
        assert_eq!(layout_mode(50, 60, 160), 0);
        assert_eq!(layout_mode(60, 60, 160), 1);
        assert_eq!(layout_mode(120, 100, 110), 2);
    }

    /// Regressão (issue #7): no full mode (≥ 160 cols) as hotkeys r/d pedem
    /// sort por net/block — agregados não visíveis — e a seta caía no ID sem
    /// hint. Deve mostrar a seta na coluna detalhada visível (NET_RX /
    /// BLOCK_READ), nunca no ID.
    #[test]
    fn net_block_hotkeys_show_arrow_on_visible_split_columns_in_full_mode() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        for (sort_by, want) in [("net", "NET_RX ▼"), ("block", "BLOCK_READ ▼")] {
            let (mut state, cfg) = sample_state();
            state.sort_by = sort_by.to_string();
            state.desc = true;
            let mut terminal = Terminal::new(TestBackend::new(223, 30)).unwrap();
            terminal.draw(|f| render(f, &state, &cfg)).unwrap();
            let text = terminal.backend().to_string();
            assert!(
                text.contains(want),
                "sort={sort_by}: '{want}' ausente do header (caiu no id?)\n{text}"
            );
            assert!(
                !text.contains("ID ▼"),
                "sort={sort_by}: seta caiu no ID\n{text}"
            );
        }
    }

    #[test]
    fn render_error_and_filter_edit_states() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let (mut state, cfg) = sample_state();
        state.errors = vec![(
            "homeserver".to_string(),
            "timeout 30s: ssh homeserver docker stats".to_string(),
        )];
        state.editing_filter = true;
        state.filter_draft = "we".to_string();
        let backend = TestBackend::new(170, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &state, &cfg)).unwrap();
        let text = terminal.backend().to_string();
        assert!(text.contains("editando"));
        // erro visível no footer (antes era engolido pela TUI)
        assert!(text.contains("⚠ homeserver: timeout 30s"), "{text}");
    }

    #[test]
    fn summary_badge_marks_errored_context_with_bang() {
        let (mut state, cfg) = sample_state();
        state.errors = vec![("homeserver".to_string(), "daemon down".to_string())];
        let s = summary_text(&state, &cfg);
        assert!(s.contains("homeserver:cli!"), "{s}");
        assert!(s.contains("default:api "), "{s}");
        state.errors.clear();
        let s = summary_text(&state, &cfg);
        assert!(s.contains("homeserver:cli]"), "{s}");
        assert!(!s.contains("homeserver:cli!"), "{s}");
    }

    #[test]
    fn once_text_lists_errors_per_context() {
        let (mut state, cfg) = sample_state();
        state.errors = vec![
            ("homeserver".to_string(), "timeout".to_string()),
            (String::new(), "join falhou".to_string()),
        ];
        let txt = render_once_text(&state, &cfg, 200);
        assert!(txt.contains("errors:"));
        assert!(txt.contains("  - homeserver: timeout"));
        assert!(txt.contains("  - join falhou"));
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

    #[test]
    fn summary_and_visible_rows_cover_empty_badge_and_filter_states() {
        let (mut state, mut cfg) = sample_state();
        state.filter = "WEB".into();
        let rows = state.visible_rows(&cfg, 200);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "web");
        state.filter = "not-present".into();
        assert!(state.visible_rows(&cfg, 200).is_empty());

        cfg.display.show_transport_badge = false;
        let text = summary_text(&state, &cfg);
        assert!(!text.contains("default:api"));
        state.contexts.clear();
        assert!(!summary_text(&state, &cfg).contains(" ["));
        cfg.display.show_transport_badge = true;
        assert!(!summary_text(&state, &cfg).contains(" ["));
    }

    #[test]
    fn summary_counts_idle_rows_and_keeps_zero_width_filter_valid() {
        let (mut state, cfg) = sample_state();
        state.rows.push(ContainerSnapshot::from_raw(
            "ccc333333333".into(),
            "idle".into(),
            "0%".into(),
            "0B / 1GiB".into(),
            "0%".into(),
            "0B / 0B".into(),
            "0B / 0B".into(),
            "0".into(),
            "default".into(),
            "api".into(),
            None,
            4,
            8 * 1024 * 1024 * 1024,
        ));
        state.filter.clear();
        assert_eq!(state.visible_rows(&cfg, 200).len(), 3);
        let summary = summary_text(&state, &cfg);
        assert!(summary.contains("3T 2A 3F"), "{summary}");
    }

    #[test]
    fn cell_styles_and_headers_cover_threshold_and_sort_states() {
        let cfg = AppConfig::default();
        assert_eq!(
            cell_style_rel(cfg.thresholds.crit + 1.0, &cfg).fg,
            Some(Color::Red)
        );
        assert_eq!(
            cell_style_rel(cfg.thresholds.warn + 1.0, &cfg).fg,
            Some(Color::Yellow)
        );
        assert_eq!(
            cell_style_rel(cfg.thresholds.warn, &cfg).fg,
            Some(Color::Green)
        );
        assert_eq!(header_title("CPU", "cpu", "cpu", true), "CPU ▼");
        assert_eq!(header_title("CPU", "cpu", "cpu", false), "CPU ▲");
        assert_eq!(header_title("CPU", "cpu", "name", true), "CPU");
        assert_eq!(short_title("unknown", 1), "UNKNOWN");
        assert_eq!(short_title("unknown", 2), "UNKNOWN");
    }

    #[test]
    fn row_cells_render_aggregates_and_unknown_columns_in_both_width_modes() {
        let (state, cfg) = sample_state();
        let cols = vec![
            "net".to_string(),
            "block".to_string(),
            "unknown".to_string(),
        ];
        let narrow_cells = row_cells(&state.rows[0], &cols, &cfg, true);
        let wide_cells = row_cells(&state.rows[0], &cols, &cfg, false);
        assert_eq!(narrow_cells.len(), cols.len());
        assert_eq!(wide_cells.len(), cols.len());
    }

    #[test]
    fn all_context_mode_inserts_context_after_id_when_not_configured() {
        let (mut state, mut cfg) = sample_state();
        state.selected_context = "all".into();
        cfg.columns.full = vec!["id".into(), "name".into()];

        assert_eq!(
            effective_columns(&state, &cfg, 200),
            vec!["id", "context", "name"]
        );
    }

    #[test]
    fn all_context_columns_without_id_do_not_insert_a_context_column() {
        let (mut state, mut cfg) = sample_state();
        state.selected_context = "all".into();
        cfg.columns.full = vec!["name".into()];
        let cols = effective_columns(&state, &cfg, 200);
        assert_eq!(cols, vec!["name".to_string()]);
    }
}
