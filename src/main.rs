//! `docker-top-rs` — port Rust do `docker-top` Python.
//!
//! Startup: CLI → config (figment) → transport dispatch 1x → providers →
//! `--once` (texto) ou TUI event-driven (coleta != render).

use docker_top_rs::{cli, collector, config, model, provider, transport, ui};

use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::Parser;
use crossterm::event::{Event, EventStream, KeyCode, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

use cli::Cli;
use collector::{collect_once, parking_lot_like, CollectOutput};
use config::{default_config_path, AppConfig};
use provider::{Caches, CliProvider, DockerProvider};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config_path = cli.config.clone().unwrap_or_else(default_config_path);

    if cli.generate_config_example {
        let p = std::path::PathBuf::from("config.example.toml");
        config::generate_example_file(&p)?;
        println!("exemplo gerado em {}", p.display());
        return Ok(());
    }

    let mut cfg = AppConfig::load(&cli, &config_path)?;
    if let Some(d) = cli.delay {
        cfg.refresh_delay = d.clamp(0.5, 10.0);
    }

    // Dispatch de transporte 1x no startup (sem flapping por request).
    let ls_timeout = Duration::from_secs_f64(cfg.timeouts.context_ls.max(0.5));
    let discovered = transport::discover(ls_timeout).await;
    if discovered.contexts.is_empty() {
        anyhow::bail!("nenhum contexto docker descoberto");
    }

    let stats_t = Duration::from_secs_f64(cfg.timeouts.stats.max(1.0));
    let inspect_t = Duration::from_secs_f64(cfg.timeouts.inspect.max(1.0));
    let info_t = Duration::from_secs_f64(cfg.timeouts.info.max(1.0));
    let providers: Vec<Arc<dyn DockerProvider>> = discovered
        .contexts
        .iter()
        .map(|c| -> Arc<dyn DockerProvider> {
            match c.transport {
                transport::Transport::Api => Arc::new(provider::BollardProvider::new_full(
                    c.name.clone(),
                    c.endpoint.clone(),
                    info_t,
                    stats_t,
                    inspect_t,
                )),
                transport::Transport::Cli => {
                    Arc::new(CliProvider::new(c.name.clone(), stats_t, inspect_t, info_t))
                }
            }
        })
        .collect();

    let initial_context = cli.context.clone().unwrap_or_else(|| cfg.context.clone());
    let initial_context = if initial_context.is_empty() {
        "all".to_string()
    } else {
        initial_context
    };

    if cli.once {
        return run_once(&cfg, &providers, &discovered, &initial_context).await;
    }

    run_tui(
        cli,
        config_path,
        cfg,
        providers,
        discovered,
        initial_context,
    )
    .await
}

async fn run_once(
    cfg: &AppConfig,
    providers: &[Arc<dyn DockerProvider>],
    discovered: &transport::DiscoveredContexts,
    selected: &str,
) -> anyhow::Result<()> {
    let caches = Arc::new(Mutex::new(Caches::default()));
    let out = collect_once(providers, &caches, cfg).await;
    let width = crossterm::terminal::size().map(|(w, _)| w).unwrap_or(200);
    let state = ui::AppState {
        rows: out.rows,
        errors: out.errors,
        sort_by: cfg.sort.clone(),
        desc: !cfg.ascending,
        filter: cfg.filter.clone(),
        selected_context: selected.to_string(),
        contexts: discovered.contexts.clone(),
        total_cores: out.totals_cores,
        total_mem: out.totals_mem,
        editing_filter: false,
        filter_draft: String::new(),
        clock_override: None,
    };
    // Tabulação simples vira texto TSV (schema estável p/ golden).
    print!("{}", ui::render_once_text(&state, cfg, width));
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_tui(
    cli: Cli,
    config_path: std::path::PathBuf,
    mut cfg: AppConfig,
    providers: Vec<Arc<dyn DockerProvider>>,
    discovered: transport::DiscoveredContexts,
    initial_context: String,
) -> anyhow::Result<()> {
    let cfg_shared = Arc::new(parking_lot_like::RwLock::new(cfg.clone()));
    let caches = Arc::new(Mutex::new(Caches::default()));
    let (tx, mut rx) = mpsc::channel::<CollectOutput>(4);
    let _collector =
        collector::spawn_collector(providers, Arc::clone(&caches), Arc::clone(&cfg_shared), tx);

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    // RAII: garante restore do terminal em panic/early-return.
    struct TermGuard;
    impl Drop for TermGuard {
        fn drop(&mut self) {
            let _ = crossterm::terminal::disable_raw_mode();
            let _ = std::io::stdout().execute(crossterm::terminal::LeaveAlternateScreen);
        }
    }
    let _guard = TermGuard;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut state = ui::AppState {
        rows: vec![],
        errors: vec![],
        sort_by: cfg.sort.clone(),
        desc: !cfg.ascending,
        filter: cfg.filter.clone(),
        selected_context: initial_context,
        contexts: discovered.contexts.clone(),
        total_cores: 0,
        total_mem: 0,
        editing_filter: false,
        filter_draft: String::new(),
        clock_override: None,
    };

    // SIGHUP → hot reload (além da tecla R).
    #[cfg(unix)]
    let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()).ok();
    #[cfg(not(unix))]
    let mut sighup: Option<tokio::signal::unix::Signal> = None;

    let mut events = EventStream::new();
    terminal.draw(|f| ui::render(f, &state, &cfg))?;

    loop {
        tokio::select! {
            biased;
            Some(out) = rx.recv() => {
                state.rows = out.rows;
                state.errors = out.errors;
                state.total_cores = out.totals_cores;
                state.total_mem = out.totals_mem;
                // evict é feito no collector; aqui só re-render
                terminal.draw(|f| ui::render(f, &state, &cfg))?;
            }
            Some(Ok(ev)) = events.next() => {
                if handle_event(ev, &mut state, &mut cfg, &cli, &config_path, &cfg_shared)? {
                    break;
                }
                terminal.draw(|f| ui::render(f, &state, &cfg))?;
            }
            _ = async {
                #[cfg(unix)]
                if let Some(sig) = sighup.as_mut() {
                    sig.recv().await;
                } else {
                    futures_util::future::pending::<()>().await
                }
                #[cfg(not(unix))]
                futures_util::future::pending::<()>().await
            } => {
                reload_cfg(&cli, &config_path, &mut cfg, &cfg_shared, &mut state);
                terminal.draw(|f| ui::render(f, &state, &cfg))?;
            }
        }
    }

    disable_raw_mode()?;
    terminal.backend_mut().execute(LeaveAlternateScreen)?;
    println!("docker-top-rs finalizado.");
    Ok(())
}

/// Retorna true para sair.
fn handle_event(
    ev: Event,
    state: &mut ui::AppState,
    cfg: &mut AppConfig,
    cli: &Cli,
    config_path: &std::path::Path,
    cfg_shared: &Arc<parking_lot_like::RwLock<AppConfig>>,
) -> anyhow::Result<bool> {
    let Event::Key(key) = ev else {
        return Ok(false); // resize/mouse → só re-render
    };
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        if matches!(key.code, KeyCode::Char('c')) {
            return Ok(true);
        }
        return Ok(false);
    }
    // modo edição de filtro
    if state.editing_filter {
        match key.code {
            KeyCode::Enter => {
                state.filter = state.filter_draft.clone();
                state.editing_filter = false;
            }
            KeyCode::Esc => {
                state.editing_filter = false;
                state.filter_draft.clear();
            }
            KeyCode::Backspace => {
                state.filter_draft.pop();
            }
            KeyCode::Char(c) => {
                state.filter_draft.push(c);
            }
            _ => {}
        }
        return Ok(false);
    }

    let kb = &cfg.keybindings;
    // Tab → próximo contexto (também `o` configurável)
    if key.code == KeyCode::Tab {
        cycle_context(state);
        return Ok(false);
    }
    match key.code {
        KeyCode::Left => {
            move_sort(state, cfg, -1);
        }
        KeyCode::Right => {
            move_sort(state, cfg, 1);
        }
        KeyCode::Up => {
            state.desc = false;
        }
        KeyCode::Down => {
            state.desc = true;
        }
        KeyCode::Esc => return Ok(true),
        KeyCode::Enter => {}
        KeyCode::Char(c) => {
            let s = c.to_string();
            // R maiúsculo = reload (r minúsculo = sort net) — case-sensitive de propósito.
            if s == kb.reload_config {
                reload_cfg(cli, config_path, cfg, cfg_shared, state);
            } else if s == kb.quit {
                return Ok(true);
            } else if s == kb.filter {
                state.editing_filter = true;
                state.filter_draft = state.filter.clone();
            } else if s == kb.toggle_order {
                state.desc = !state.desc;
            } else if s == kb.sort_cpu_rel {
                state.sort_by = "cpu_rel".into();
            } else if s == kb.sort_mem_rel {
                state.sort_by = "mem_rel".into();
            } else if s == kb.sort_cpu {
                state.sort_by = "cpu".into();
            } else if s == kb.sort_mem_used {
                state.sort_by = "mem_used".into();
            } else if s == kb.sort_cpu_lim {
                state.sort_by = "cpu_lim".into();
            } else if s == kb.sort_mem_lim {
                state.sort_by = "mem_lim".into();
            } else if s == kb.sort_id {
                state.sort_by = "id".into();
            } else if s == kb.sort_name {
                state.sort_by = "name".into();
            } else if s == kb.sort_context {
                state.sort_by = "context".into();
            } else if s == kb.sort_pids {
                state.sort_by = "pids".into();
            } else if s == kb.sort_net_rx {
                state.sort_by = "net_rx".into();
            } else if s == kb.sort_net_tx {
                state.sort_by = "net_tx".into();
            } else if s == kb.sort_net {
                state.sort_by = "net".into();
            } else if s == kb.sort_block_read {
                state.sort_by = "block_read".into();
            } else if s == kb.sort_block_write {
                state.sort_by = "block_write".into();
            } else if s == kb.sort_block {
                state.sort_by = "block".into();
            } else if s == kb.next_context {
                cycle_context(state);
            } else if s == kb.refresh_inc {
                cfg.refresh_delay = (cfg.refresh_delay + 0.5).min(10.0);
            } else if s == kb.refresh_dec {
                cfg.refresh_delay = (cfg.refresh_delay - 0.5).max(0.5);
            }
        }
        _ => {}
    }
    Ok(false)
}

fn cycle_context(state: &mut ui::AppState) {
    let mut options: Vec<String> = vec!["all".to_string()];
    options.extend(state.contexts.iter().map(|c| c.name.clone()));
    let idx = options
        .iter()
        .position(|o| *o == state.selected_context)
        .unwrap_or(0);
    state.selected_context = options[(idx + 1) % options.len()].clone();
    state.rows.clear();
}

fn move_sort(state: &mut ui::AppState, cfg: &AppConfig, dir: i32) {
    let width = crossterm::terminal::size().map(|(w, _)| w).unwrap_or(160);
    let cols = ui::effective_columns(state, cfg, width);
    let cur = model::normalize_sort(&state.sort_by, &cols);
    if let Some(i) = cols.iter().position(|c| *c == cur) {
        let n = cols.len() as i32;
        let next = ((i as i32 + dir).rem_euclid(n)) as usize;
        state.sort_by = cols[next].clone();
    }
}

fn reload_cfg(
    cli: &Cli,
    path: &std::path::Path,
    cfg: &mut AppConfig,
    shared: &Arc<parking_lot_like::RwLock<AppConfig>>,
    state: &mut ui::AppState,
) {
    match AppConfig::load(cli, path) {
        Ok(fresh) => {
            *cfg = fresh.clone();
            *shared.write() = fresh.clone();
            state.sort_by = fresh.sort.clone();
            state.desc = !fresh.ascending;
            eprintln!("config recarregada de {}", path.display());
        }
        Err(e) => {
            state.errors.push(format!("reload config falhou: {e:#}"));
        }
    }
}
