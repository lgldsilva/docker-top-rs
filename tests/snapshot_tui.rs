//! Snapshot da TUI via `ratatui::TestBackend` (insta).

use docker_top_rs::config::AppConfig;
use docker_top_rs::model::ContainerSnapshot;
use docker_top_rs::transport::{DockerContext, Transport};
use docker_top_rs::ui::{render, AppState};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

fn fixture() -> (AppState, AppConfig) {
    let mk = |id: &str, name: &str, ctx: &str, cpu: &str, mem: &str| {
        ContainerSnapshot::from_raw(
            id.into(),
            name.into(),
            cpu.into(),
            mem.into(),
            "9.7%".into(),
            "1.54MiB / 2.00MiB".into(),
            "128MiB / 64MiB".into(),
            "9".into(),
            ctx.into(),
            "cli".into(),
            None,
            4,
            8 * 1024 * 1024 * 1024,
        )
    };
    let state = AppState {
        rows: vec![
            mk(
                "3b5529000001",
                "CI-TASK",
                "cloud-arm2",
                "12.5%",
                "413MiB / 6.00GiB",
            ),
            mk(
                "2e653f000002",
                "cache-db",
                "lab-node1",
                "8.20%",
                "11.50MiB / 512MiB",
            ),
            mk(
                "c06891000003",
                "filebrowser",
                "default",
                "5.10%",
                "17.20MiB / 128MiB",
            ),
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
                name: "lab-node1".into(),
                endpoint: "ssh://admin@192.0.2.10".into(),
                transport: Transport::Cli,
            },
        ],
        total_cores: 12,
        total_mem: 32 * 1024 * 1024 * 1024,
        editing_filter: false,
        filter_draft: String::new(),
        clock_override: Some("12:00:00".to_string()),
    };
    (state, AppConfig::default())
}

#[test]
fn snapshot_full_layout() {
    let (state, cfg) = fixture();
    let backend = TestBackend::new(170, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| render(f, &state, &cfg)).unwrap();
    insta::assert_snapshot!(terminal.backend().to_string());
}

#[test]
fn snapshot_compact_layout() {
    let (state, cfg) = fixture();
    let backend = TestBackend::new(120, 25);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| render(f, &state, &cfg)).unwrap();
    insta::assert_snapshot!(terminal.backend().to_string());
}

#[test]
fn snapshot_ultra_compact_layout() {
    let (state, cfg) = fixture();
    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| render(f, &state, &cfg)).unwrap();
    insta::assert_snapshot!(terminal.backend().to_string());
}
