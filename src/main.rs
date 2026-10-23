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
    let docker_bin = transport::resolve_docker_bin();
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
                transport::Transport::Cli => Arc::new(
                    CliProvider::new(c.name.clone(), stats_t, inspect_t, info_t)
                        .with_bin(docker_bin.clone()),
                ),
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
    // Igual ao Python: coleta só o contexto selecionado (all = todos).
    let active: Vec<Arc<dyn DockerProvider>> = providers
        .iter()
        .filter(|p| selected == "all" || p.context_name() == selected)
        .cloned()
        .collect();
    let caches = Arc::new(Mutex::new(Caches::default()));
    let out = collect_once(&active, &caches, cfg).await;
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
    let selected_shared = Arc::new(Mutex::new(initial_context.clone()));
    let (tx, mut rx) = mpsc::channel::<CollectOutput>(4);
    let _collector = collector::spawn_collector(
        providers,
        Arc::clone(&caches),
        Arc::clone(&cfg_shared),
        Arc::clone(&selected_shared),
        tx,
    );

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
                if handle_event(
                    ev,
                    &mut state,
                    &mut cfg,
                    &cli,
                    &config_path,
                    &cfg_shared,
                    &selected_shared,
                )? {
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
    selected_shared: &Arc<Mutex<String>>,
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
        cycle_context(state, selected_shared);
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
                cycle_context(state, selected_shared);
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

fn cycle_context(state: &mut ui::AppState, selected_shared: &Arc<Mutex<String>>) {
    let mut options: Vec<String> = vec!["all".to_string()];
    options.extend(state.contexts.iter().map(|c| c.name.clone()));
    let idx = options
        .iter()
        .position(|o| *o == state.selected_context)
        .unwrap_or(0);
    state.selected_context = options[(idx + 1) % options.len()].clone();
    if let Ok(mut s) = selected_shared.lock() {
        *s = state.selected_context.clone();
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_dir(label: &str) -> std::path::PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "docker-top-main-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn app_state() -> ui::AppState {
        ui::AppState {
            rows: vec![],
            errors: vec![],
            sort_by: "id".into(),
            desc: true,
            filter: "web".into(),
            selected_context: "all".into(),
            contexts: vec![
                transport::DockerContext {
                    name: "default".into(),
                    endpoint: "unix:///var/run/docker.sock".into(),
                    transport: transport::Transport::Api,
                },
                transport::DockerContext {
                    name: "remote".into(),
                    endpoint: "ssh://remote".into(),
                    transport: transport::Transport::Cli,
                },
            ],
            total_cores: 0,
            total_mem: 0,
            editing_filter: false,
            filter_draft: String::new(),
            clock_override: None,
        }
    }

    fn test_cli() -> Cli {
        Cli::try_parse_from(["docker-top-rs"]).expect("valid default CLI")
    }

    // NOSONAR: this test adapter forwards the complete handle_event fixture unchanged.
    #[allow(clippy::too_many_arguments)]
    fn send_key(
        code: KeyCode,
        modifiers: KeyModifiers,
        state: &mut ui::AppState,
        cfg: &mut AppConfig,
        cli: &Cli,
        config_path: &std::path::Path,
        cfg_shared: &Arc<parking_lot_like::RwLock<AppConfig>>,
        selected_shared: &Arc<Mutex<String>>,
    ) -> anyhow::Result<bool> {
        handle_event(
            Event::Key(crossterm::event::KeyEvent::new(code, modifiers)),
            state,
            cfg,
            cli,
            config_path,
            cfg_shared,
            selected_shared,
        )
    }

    #[test]
    fn handles_non_key_control_and_navigation_events() {
        let cli = test_cli();
        let mut cfg = AppConfig::default();
        let path = temp_dir("events");
        std::fs::write(&path, "").unwrap();
        let cfg_shared = Arc::new(parking_lot_like::RwLock::new(cfg.clone()));
        let selected_shared = Arc::new(Mutex::new("all".into()));
        let mut state = app_state();

        assert!(!handle_event(
            Event::Resize(80, 24),
            &mut state,
            &mut cfg,
            &cli,
            &path,
            &cfg_shared,
            &selected_shared,
        )
        .unwrap());
        assert!(!send_key(
            KeyCode::Char('x'),
            KeyModifiers::CONTROL,
            &mut state,
            &mut cfg,
            &cli,
            &path,
            &cfg_shared,
            &selected_shared,
        )
        .unwrap());
        assert!(send_key(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
            &mut state,
            &mut cfg,
            &cli,
            &path,
            &cfg_shared,
            &selected_shared,
        )
        .unwrap());
        assert!(!send_key(
            KeyCode::Enter,
            KeyModifiers::NONE,
            &mut state,
            &mut cfg,
            &cli,
            &path,
            &cfg_shared,
            &selected_shared,
        )
        .unwrap());
        assert!(send_key(
            KeyCode::Esc,
            KeyModifiers::NONE,
            &mut state,
            &mut cfg,
            &cli,
            &path,
            &cfg_shared,
            &selected_shared,
        )
        .unwrap());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn filter_edit_mode_handles_commit_cancel_backspace_character_and_other_keys() {
        let cli = test_cli();
        let mut cfg = AppConfig::default();
        let path = temp_dir("filter");
        std::fs::write(&path, "").unwrap();
        let cfg_shared = Arc::new(parking_lot_like::RwLock::new(cfg.clone()));
        let selected_shared = Arc::new(Mutex::new("all".into()));
        let mut state = app_state();
        state.editing_filter = true;
        state.filter_draft = "webx".into();

        send_key(
            KeyCode::Backspace,
            KeyModifiers::NONE,
            &mut state,
            &mut cfg,
            &cli,
            &path,
            &cfg_shared,
            &selected_shared,
        )
        .unwrap();
        assert_eq!(state.filter_draft, "web");
        send_key(
            KeyCode::Char('!'),
            KeyModifiers::NONE,
            &mut state,
            &mut cfg,
            &cli,
            &path,
            &cfg_shared,
            &selected_shared,
        )
        .unwrap();
        assert_eq!(state.filter_draft, "web!");
        send_key(
            KeyCode::Left,
            KeyModifiers::NONE,
            &mut state,
            &mut cfg,
            &cli,
            &path,
            &cfg_shared,
            &selected_shared,
        )
        .unwrap();
        assert_eq!(state.filter_draft, "web!");
        send_key(
            KeyCode::Enter,
            KeyModifiers::NONE,
            &mut state,
            &mut cfg,
            &cli,
            &path,
            &cfg_shared,
            &selected_shared,
        )
        .unwrap();
        assert_eq!(state.filter, "web!");
        assert!(!state.editing_filter);

        state.editing_filter = true;
        state.filter_draft = "discard".into();
        send_key(
            KeyCode::Esc,
            KeyModifiers::NONE,
            &mut state,
            &mut cfg,
            &cli,
            &path,
            &cfg_shared,
            &selected_shared,
        )
        .unwrap();
        assert!(state.filter_draft.is_empty());
        assert!(!state.editing_filter);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn keybindings_cover_sort_toggle_filter_context_and_refresh_actions() {
        let cli = test_cli();
        let mut cfg = AppConfig::default();
        let path = temp_dir("keybindings");
        std::fs::write(&path, "").unwrap();
        let cfg_shared = Arc::new(parking_lot_like::RwLock::new(cfg.clone()));
        let selected_shared = Arc::new(Mutex::new("all".into()));
        let cases = [
            (cfg.keybindings.sort_cpu_rel.clone(), "cpu_rel"),
            (cfg.keybindings.sort_mem_rel.clone(), "mem_rel"),
            (cfg.keybindings.sort_cpu.clone(), "cpu"),
            (cfg.keybindings.sort_mem_used.clone(), "mem_used"),
            (cfg.keybindings.sort_cpu_lim.clone(), "cpu_lim"),
            (cfg.keybindings.sort_mem_lim.clone(), "mem_lim"),
            (cfg.keybindings.sort_id.clone(), "id"),
            (cfg.keybindings.sort_name.clone(), "name"),
            (cfg.keybindings.sort_context.clone(), "context"),
            (cfg.keybindings.sort_pids.clone(), "pids"),
            (cfg.keybindings.sort_net_rx.clone(), "net_rx"),
            (cfg.keybindings.sort_net_tx.clone(), "net_tx"),
            (cfg.keybindings.sort_net.clone(), "net"),
            (cfg.keybindings.sort_block_read.clone(), "block_read"),
            (cfg.keybindings.sort_block_write.clone(), "block_write"),
            (cfg.keybindings.sort_block.clone(), "block"),
        ];
        let mut state = app_state();
        for (key, expected) in cases {
            state.sort_by = "unknown".into();
            send_key(
                KeyCode::Char(key.chars().next().unwrap()),
                KeyModifiers::NONE,
                &mut state,
                &mut cfg,
                &cli,
                &path,
                &cfg_shared,
                &selected_shared,
            )
            .unwrap();
            assert_eq!(state.sort_by, expected, "key binding {key}");
        }
        cfg.keybindings.next_context = "z".into();
        send_key(
            KeyCode::Char('z'),
            KeyModifiers::NONE,
            &mut state,
            &mut cfg,
            &cli,
            &path,
            &cfg_shared,
            &selected_shared,
        )
        .unwrap();
        assert_eq!(state.selected_context, "default");

        let previous_order = state.desc;
        send_key(
            KeyCode::Char(cfg.keybindings.toggle_order.chars().next().unwrap()),
            KeyModifiers::NONE,
            &mut state,
            &mut cfg,
            &cli,
            &path,
            &cfg_shared,
            &selected_shared,
        )
        .unwrap();
        assert_eq!(state.desc, !previous_order);
        send_key(
            KeyCode::Char(cfg.keybindings.filter.chars().next().unwrap()),
            KeyModifiers::NONE,
            &mut state,
            &mut cfg,
            &cli,
            &path,
            &cfg_shared,
            &selected_shared,
        )
        .unwrap();
        assert!(state.editing_filter);
        assert_eq!(state.filter_draft, state.filter);
        state.editing_filter = false;
        state.desc = true;
        cfg.refresh_delay = 9.9;
        send_key(
            KeyCode::Char('+'),
            KeyModifiers::NONE,
            &mut state,
            &mut cfg,
            &cli,
            &path,
            &cfg_shared,
            &selected_shared,
        )
        .unwrap();
        assert_eq!(cfg.refresh_delay, 10.0);
        cfg.refresh_delay = 0.6;
        send_key(
            KeyCode::Char('-'),
            KeyModifiers::NONE,
            &mut state,
            &mut cfg,
            &cli,
            &path,
            &cfg_shared,
            &selected_shared,
        )
        .unwrap();
        assert_eq!(cfg.refresh_delay, 0.5);
        send_key(
            KeyCode::Char('?'),
            KeyModifiers::NONE,
            &mut state,
            &mut cfg,
            &cli,
            &path,
            &cfg_shared,
            &selected_shared,
        )
        .unwrap();
        assert!(send_key(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
            &mut state,
            &mut cfg,
            &cli,
            &path,
            &cfg_shared,
            &selected_shared
        )
        .unwrap());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn tab_cycles_contexts_and_sort_arrows_wrap_columns() {
        let cli = test_cli();
        let mut cfg = AppConfig::default();
        let path = temp_dir("navigation");
        std::fs::write(&path, "").unwrap();
        let cfg_shared = Arc::new(parking_lot_like::RwLock::new(cfg.clone()));
        let selected_shared = Arc::new(Mutex::new("all".into()));
        let mut state = app_state();
        state.rows.push(model::ContainerSnapshot::from_raw(
            "id".into(),
            "web".into(),
            "0%".into(),
            "0 / 0".into(),
            "0%".into(),
            "0 / 0".into(),
            "0 / 0".into(),
            "0".into(),
            "default".into(),
            "api".into(),
            None,
            1,
            1,
        ));
        send_key(
            KeyCode::Tab,
            KeyModifiers::NONE,
            &mut state,
            &mut cfg,
            &cli,
            &path,
            &cfg_shared,
            &selected_shared,
        )
        .unwrap();
        assert_eq!(state.selected_context, "default");
        assert_eq!(*selected_shared.lock().unwrap(), "default");
        assert!(state.rows.is_empty());
        send_key(
            KeyCode::Tab,
            KeyModifiers::NONE,
            &mut state,
            &mut cfg,
            &cli,
            &path,
            &cfg_shared,
            &selected_shared,
        )
        .unwrap();
        assert_eq!(state.selected_context, "remote");
        state.selected_context = "missing".into();
        cycle_context(&mut state, &selected_shared);
        assert_eq!(state.selected_context, "default");
        state.sort_by = "id".into();
        send_key(
            KeyCode::Left,
            KeyModifiers::NONE,
            &mut state,
            &mut cfg,
            &cli,
            &path,
            &cfg_shared,
            &selected_shared,
        )
        .unwrap();
        assert_ne!(state.sort_by, "id");
        send_key(
            KeyCode::Right,
            KeyModifiers::NONE,
            &mut state,
            &mut cfg,
            &cli,
            &path,
            &cfg_shared,
            &selected_shared,
        )
        .unwrap();
        assert!(!state.sort_by.is_empty());
        send_key(
            KeyCode::Up,
            KeyModifiers::NONE,
            &mut state,
            &mut cfg,
            &cli,
            &path,
            &cfg_shared,
            &selected_shared,
        )
        .unwrap();
        assert!(!state.desc);
        send_key(
            KeyCode::Down,
            KeyModifiers::NONE,
            &mut state,
            &mut cfg,
            &cli,
            &path,
            &cfg_shared,
            &selected_shared,
        )
        .unwrap();
        assert!(state.desc);

        cfg.columns.full.clear();
        cfg.columns.compact.clear();
        cfg.columns.ultra_compact.clear();
        let previous_sort = state.sort_by.clone();
        move_sort(&mut state, &cfg, 1);
        assert_eq!(state.sort_by, previous_sort);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reload_updates_shared_config_and_records_invalid_config() {
        let cli = test_cli();
        let path = temp_dir("reload");
        std::fs::write(&path, "sort = 'mem_rel'\nascending = true\n").unwrap();
        let mut cfg = AppConfig::default();
        let cfg_shared = Arc::new(parking_lot_like::RwLock::new(cfg.clone()));
        let selected_shared = Arc::new(Mutex::new("all".into()));
        let mut state = app_state();

        send_key(
            KeyCode::Char('R'),
            KeyModifiers::NONE,
            &mut state,
            &mut cfg,
            &cli,
            &path,
            &cfg_shared,
            &selected_shared,
        )
        .unwrap();
        assert_eq!(cfg.sort, "mem_rel");
        assert_eq!(state.sort_by, "mem_rel");
        assert!(!state.desc);
        assert_eq!(cfg_shared.read().sort, "mem_rel");

        std::fs::write(&path, "unknown_field = 1\n").unwrap();
        send_key(
            KeyCode::Char('R'),
            KeyModifiers::NONE,
            &mut state,
            &mut cfg,
            &cli,
            &path,
            &cfg_shared,
            &selected_shared,
        )
        .unwrap();
        assert!(state
            .errors
            .iter()
            .any(|e| e.starts_with("reload config falhou:")));
        let _ = std::fs::remove_file(path);
    }

    struct TestProvider(String);

    #[async_trait::async_trait]
    impl DockerProvider for TestProvider {
        fn context_name(&self) -> &str {
            &self.0
        }
        fn transport_label(&self) -> &'static str {
            "cli"
        }
        async fn list_stats(&self) -> anyhow::Result<Vec<provider::RawStats>> {
            Ok(vec![provider::RawStats::from_strings([
                "abcdef1234567890".into(),
                "web".into(),
                "1.0%".into(),
                "1MiB / 1GiB".into(),
                "0.1%".into(),
                "1KiB / 2KiB".into(),
                "1KiB / 2KiB".into(),
                "1".into(),
            ])])
        }
        async fn inspect_limits(
            &self,
            _ids: &[String],
        ) -> anyhow::Result<Vec<(String, model::ContainerLimits)>> {
            Ok(vec![])
        }
        async fn system_info(&self) -> anyhow::Result<(u32, u64)> {
            Ok((2, 4 * 1024 * 1024 * 1024))
        }
    }

    #[tokio::test]
    async fn once_collects_all_or_only_the_selected_context() {
        let cfg = AppConfig::default();
        let providers: Vec<Arc<dyn DockerProvider>> = vec![
            Arc::new(TestProvider("default".into())),
            Arc::new(TestProvider("remote".into())),
        ];
        let discovered = transport::DiscoveredContexts {
            contexts: vec![
                transport::DockerContext {
                    name: "default".into(),
                    endpoint: "unix:///var/run/docker.sock".into(),
                    transport: transport::Transport::Api,
                },
                transport::DockerContext {
                    name: "remote".into(),
                    endpoint: "ssh://remote".into(),
                    transport: transport::Transport::Cli,
                },
            ],
        };
        for selected in ["all", "default", "missing"] {
            run_once(&cfg, &providers, &discovered, selected)
                .await
                .unwrap();
        }
    }
}
