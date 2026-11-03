//! Coletor: tick de coleta != render. Um ciclo coleta todos os contextos em
//! paralelo, enriquece com limites (inspect sob demanda) e publica via `mpsc`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;

use crate::config::AppConfig;
use crate::logging::Logger;
use crate::model::{short_id, ContainerSnapshot};
use crate::provider::{
    build_snapshot, Caches, DockerProvider, SharedCaches, FALLBACK_CORES, FALLBACK_MEM, SYSINFO_TTL,
};

#[derive(Debug, Clone)]
pub struct CollectOutput {
    pub rows: Vec<ContainerSnapshot>,
    /// `(contexto, mensagem)` — contexto vazio p/ falha sem contexto (join).
    pub errors: Vec<(String, String)>,
    pub totals_cores: u32,
    pub totals_mem: u64,
}

/// Enriquecimento de UM contexto (sysinfo + snapshots; inspect em background).
/// Extraído p/ rodar em PARALELO entre contextos: antes era sequencial e um
/// homelab com 3 ssh hosts fazia o ciclo levar 40s+ (remotos "não apareciam").
///
/// `wait_inspect`: no `--once` o inspect bloqueia (paridade Python — limites
/// presentes no único print). Na TUI é `false`: as rows saem neste tick com os
/// limites em cache e o inspect roda destacado (guarda `in_flight` evita
/// duplicar) — limits refinados no tick seguinte. Motivo: inspect de ~57 ids
/// via ssh num host lento (Pi) mede 25s+ serializado no daemon remoto.
async fn enrich_provider(
    p: Arc<dyn DockerProvider>,
    transport: String,
    list: Vec<crate::provider::RawStats>,
    caches: SharedCaches,
    logger: Logger,
    wait_inspect: bool,
) -> Vec<ContainerSnapshot> {
    let ctx = p.context_name().to_string();
    let t0 = std::time::Instant::now();
    let (cores, mem) = {
        let cached = caches.lock().ok().and_then(|c| {
            let ts_ok = c
                .sysinfo_ts
                .get(&ctx)
                .map(|t| t.elapsed() < SYSINFO_TTL)
                .unwrap_or(false);
            if ts_ok {
                c.sysinfo.get(&ctx).copied()
            } else {
                None
            }
        });
        if let Some(v) = cached {
            v
        } else {
            match p.system_info().await {
                Ok(v) => {
                    if let Ok(mut c) = caches.lock() {
                        c.sysinfo.insert(ctx.clone(), v);
                        c.sysinfo_ts.insert(ctx.clone(), std::time::Instant::now());
                    }
                    v
                }
                Err(_) => {
                    // fallback: cache (mesmo expirado) ou host local
                    if let Ok(c) = caches.lock() {
                        c.sysinfo
                            .get(&ctx)
                            .copied()
                            .unwrap_or((FALLBACK_CORES, FALLBACK_MEM))
                    } else {
                        (FALLBACK_CORES, FALLBACK_MEM)
                    }
                }
            }
        }
    };
    // ids sem limite → inspect (bloqueante no --once, background na TUI)
    let missing: Vec<String> = {
        let c = caches.lock().expect("caches lock");
        list.iter()
            .map(|r| r.id.clone())
            .filter(|id| {
                let short = short_id(id).to_string();
                !c.limits.contains_key(&(ctx.clone(), short.clone()))
                    && !c.in_flight.contains(&(ctx.clone(), short))
            })
            .collect()
    };
    if !missing.is_empty() && wait_inspect {
        let t_inspect = std::time::Instant::now();
        let found = p.inspect_limits(&missing).await;
        logger.log(format!(
            "inspect ctx={ctx} dur={:.2}s n={} ok={}",
            t_inspect.elapsed().as_secs_f64(),
            missing.len(),
            found.is_ok()
        ));
        if let Ok(found) = found {
            if let Ok(mut c) = caches.lock() {
                for (short, lim) in found {
                    c.limits.insert((ctx.clone(), short), lim);
                }
            }
        }
    } else if !missing.is_empty() {
        // TUI: inspeciona em background; rows deste tick usam o cache atual.
        let shorts: Vec<String> = missing.iter().map(|id| short_id(id).to_string()).collect();
        {
            let mut c = caches.lock().expect("caches lock");
            for short in &shorts {
                c.in_flight.insert((ctx.clone(), short.clone()));
            }
        }
        let caches_bg = Arc::clone(&caches);
        let ctx_bg = ctx.clone();
        let logger_bg = logger.clone();
        tokio::spawn(async move {
            let t = std::time::Instant::now();
            let found = p.inspect_limits(&missing).await;
            logger_bg.log(format!(
                "inspect-bg ctx={ctx_bg} dur={:.2}s n={} ok={}",
                t.elapsed().as_secs_f64(),
                shorts.len(),
                found.is_ok()
            ));
            if let Ok(mut c) = caches_bg.lock() {
                if let Ok(found) = found {
                    for (short, lim) in found {
                        c.limits.insert((ctx_bg.clone(), short), lim);
                    }
                }
                for short in &shorts {
                    c.in_flight.remove(&(ctx_bg.clone(), short.clone()));
                }
            }
        });
    }
    // evict ids que sumiram
    {
        let seen: HashSet<String> = list.iter().map(|r| short_id(&r.id).to_string()).collect();
        if let Ok(mut c) = caches.lock() {
            c.evict_missing(&ctx, &seen);
        }
    }
    // monta snapshots (lock único p/ o lote)
    let mut rows = Vec::with_capacity(list.len());
    let guard = caches.lock().expect("caches lock");
    for raw in &list {
        rows.push(build_snapshot(raw, &ctx, &transport, &guard, cores, mem));
    }
    drop(guard);
    logger.log(format!(
        "enrich ctx={ctx} dur={:.2}s rows={}",
        t0.elapsed().as_secs_f64(),
        rows.len()
    ));
    rows
}

/// Um ciclo de coleta sobre os providers ativos.
/// Fases paralelas por contexto: (1) stats, (2) enriquecimento.
/// `wait_inspect=true` (--once) bloqueia no inspect p/ paridade com o Python.
pub async fn collect_once(
    providers: &[Arc<dyn DockerProvider>],
    caches: &Arc<Mutex<Caches>>,
    cfg: &AppConfig,
    logger: &Logger,
    wait_inspect: bool,
) -> CollectOutput {
    let _ = cfg;
    // fase 1: stats em paralelo por contexto (com duração p/ o log)
    let mut handles = Vec::new();
    for p in providers {
        let p = Arc::clone(p);
        handles.push(tokio::spawn(async move {
            let t0 = std::time::Instant::now();
            let r = p.list_stats().await;
            let dur = t0.elapsed();
            (
                p.context_name().to_string(),
                p.transport_label().to_string(),
                r,
                dur,
            )
        }));
    }
    let mut raws: HashMap<String, (String, Vec<crate::provider::RawStats>)> = HashMap::new();
    let mut errors = Vec::new();
    for h in handles {
        match h.await {
            Ok((ctx, tr, Ok(rows), dur)) => {
                logger.log(format!(
                    "stats ctx={ctx} via={tr} dur={:.2}s rows={}",
                    dur.as_secs_f64(),
                    rows.len()
                ));
                raws.insert(ctx.clone(), (tr, rows));
            }
            Ok((ctx, _, Err(e), dur)) => {
                logger.log(format!(
                    "stats ctx={ctx} erro dur={:.2}s msg=\"{e:#}\"",
                    dur.as_secs_f64()
                ));
                errors.push((ctx, format!("{e:#}")));
            }
            Err(e) => errors.push((String::new(), format!("join falhou: {e:#}"))),
        }
    }

    // fase 2: enriquecimento em paralelo por contexto
    let t_enrich = std::time::Instant::now();
    let mut handles = Vec::new();
    for p in providers {
        let ctx = p.context_name();
        let Some((transport, list)) = raws.get(ctx) else {
            continue;
        };
        handles.push(tokio::spawn(enrich_provider(
            Arc::clone(p),
            transport.clone(),
            list.clone(),
            Arc::clone(caches),
            logger.clone(),
            wait_inspect,
        )));
    }
    let mut rows = Vec::new();
    for h in handles {
        match h.await {
            Ok(mut r) => rows.append(&mut r),
            Err(e) => errors.push((String::new(), format!("join falhou (enrich): {e:#}"))),
        }
    }
    logger.log(format!(
        "enrich-total dur={:.2}s rows={}",
        t_enrich.elapsed().as_secs_f64(),
        rows.len()
    ));

    let (totals_cores, totals_mem) = {
        let mut tc = 0u32;
        let mut tm = 0u64;
        if let Ok(c) = caches.lock() {
            for p in providers {
                if let Some((co, me)) = c.sysinfo.get(p.context_name()) {
                    tc += *co;
                    tm += *me;
                }
            }
        }
        (tc, tm)
    };

    CollectOutput {
        rows,
        errors,
        totals_cores,
        totals_mem,
    }
}

/// Loop de coleta: `interval(refresh_delay)` → `tx.send(output)`.
/// Render é event-driven no `ui.rs` (não 10fps fixo).
/// Filtra providers pelo contexto selecionado (igual Python); Tab atualiza o shared.
pub fn spawn_collector(
    providers: Vec<Arc<dyn DockerProvider>>,
    caches: Arc<Mutex<Caches>>,
    cfg: Arc<parking_lot_like::RwLock<AppConfig>>,
    selected: Arc<Mutex<String>>,
    logger: Arc<Logger>,
    tx: mpsc::Sender<CollectOutput>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // primeira coleta imediata
        loop {
            let t0 = std::time::Instant::now();
            let snapshot_cfg = { cfg.read().clone() };
            let sel = {
                selected
                    .lock()
                    .ok()
                    .map(|s| s.clone())
                    .unwrap_or_else(|| "all".to_string())
            };
            let active: Vec<Arc<dyn DockerProvider>> = providers
                .iter()
                .filter(|p| sel == "all" || p.context_name() == sel)
                .cloned()
                .collect();
            let out = collect_once(&active, &caches, &snapshot_cfg, &logger, false).await;
            logger.log(format!(
                "tick sel={sel} dur={:.2}s rows={} erros={}",
                t0.elapsed().as_secs_f64(),
                out.rows.len(),
                out.errors.len()
            ));
            if tx.send(out).await.is_err() {
                break;
            }
            let delay = snapshot_cfg.refresh_delay.clamp(0.5, 10.0);
            tokio::time::sleep(Duration::from_secs_f64(delay)).await;
        }
    })
}

/// RwLock síncrono mínimo sem nova dependência (stdlib).
pub mod parking_lot_like {
    use std::sync::{RwLockReadGuard, RwLockWriteGuard};

    pub struct RwLock<T>(std::sync::RwLock<T>);
    impl<T> RwLock<T> {
        pub fn new(v: T) -> Self {
            Self(std::sync::RwLock::new(v))
        }
        pub fn read(&self) -> RwLockReadGuard<'_, T> {
            self.0.read().expect("rwlock")
        }
        pub fn write(&self) -> RwLockWriteGuard<'_, T> {
            self.0.write().expect("rwlock")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{MockProvider, RawStats};

    struct FailingProvider(&'static str);

    struct InfoFailProvider(&'static str);

    #[async_trait::async_trait]
    impl DockerProvider for InfoFailProvider {
        fn context_name(&self) -> &str {
            self.0
        }
        fn transport_label(&self) -> &'static str {
            "cli"
        }
        async fn list_stats(&self) -> anyhow::Result<Vec<RawStats>> {
            Ok(vec![raw("abcdeffedcba1234", "web")])
        }
        async fn inspect_limits(
            &self,
            _ids: &[String],
        ) -> anyhow::Result<Vec<(String, crate::model::ContainerLimits)>> {
            anyhow::bail!("inspect unavailable")
        }
        async fn system_info(&self) -> anyhow::Result<(u32, u64)> {
            anyhow::bail!("info unavailable")
        }
    }

    #[async_trait::async_trait]
    impl DockerProvider for FailingProvider {
        fn context_name(&self) -> &str {
            self.0
        }
        fn transport_label(&self) -> &'static str {
            "cli"
        }
        async fn list_stats(&self) -> anyhow::Result<Vec<RawStats>> {
            anyhow::bail!("daemon down")
        }
        async fn inspect_limits(
            &self,
            _ids: &[String],
        ) -> anyhow::Result<Vec<(String, crate::model::ContainerLimits)>> {
            Ok(vec![])
        }
        async fn system_info(&self) -> anyhow::Result<(u32, u64)> {
            anyhow::bail!("daemon down")
        }
    }

    fn raw(id: &str, name: &str) -> RawStats {
        RawStats::from_strings([
            id.to_string(),
            name.to_string(),
            "5.0%".to_string(),
            "100MiB / 1GiB".to_string(),
            "9.7%".to_string(),
            "1MiB / 1MiB".to_string(),
            "1MiB / 1MiB".to_string(),
            "3".to_string(),
        ])
    }

    #[tokio::test]
    async fn collect_merges_contexts_and_evicts() {
        let cfg = AppConfig::default();
        let caches = Arc::new(Mutex::new(Caches::default()));
        let p1: Arc<dyn DockerProvider> = Arc::new(MockProvider::new(
            "default",
            vec![raw("aaa111111111", "web")],
        ));
        let p2: Arc<dyn DockerProvider> = Arc::new(MockProvider::new(
            "homeserver",
            vec![raw("bbb222222222", "db")],
        ));
        let out = collect_once(&[p1, p2], &caches, &cfg, &Logger::disabled(), true).await;
        assert_eq!(out.rows.len(), 2);
        assert!(out.errors.is_empty());
    }

    #[tokio::test]
    async fn collect_reports_errors_when_all_down() {
        let cfg = AppConfig::default();
        let caches = Arc::new(Mutex::new(Caches::default()));
        let p: Arc<dyn DockerProvider> = Arc::new(FailingProvider("ctxdown"));
        let out = collect_once(
            std::slice::from_ref(&p),
            &caches,
            &cfg,
            &Logger::disabled(),
            true,
        )
        .await;
        assert!(out.rows.is_empty());
        assert_eq!(out.errors.len(), 1);
        assert_eq!(out.errors[0].0, "ctxdown");
        assert!(out.errors[0].1.contains("daemon down"));
    }

    #[tokio::test]
    async fn tui_mode_returns_rows_and_fills_limits_in_background() {
        let cfg = AppConfig::default();
        let caches = Arc::new(Mutex::new(Caches::default()));
        let mut mock = MockProvider::new("ctx", vec![raw("aaa111111111", "web")]);
        mock.limits.insert(
            "aaa111111111".into(),
            crate::model::ContainerLimits {
                cpu_limit: 2.0,
                has_cpu_limit: true,
                mem_limit: 1024.0,
                has_mem_limit: true,
            },
        );
        let p: Arc<dyn DockerProvider> = Arc::new(mock);

        // wait_inspect=false (TUI): rows saem neste tick, inspect vai em background
        let out = collect_once(
            std::slice::from_ref(&p),
            &caches,
            &cfg,
            &Logger::disabled(),
            false,
        )
        .await;
        assert_eq!(out.rows.len(), 1);

        // background concluído (mock é instantâneo): limite no cache, in_flight limpo
        for _ in 0..100 {
            {
                let c = caches.lock().unwrap();
                if c.limits
                    .contains_key(&("ctx".into(), "aaa111111111".into()))
                    && c.in_flight.is_empty()
                {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let c = caches.lock().unwrap();
        assert!(
            c.limits
                .contains_key(&("ctx".into(), "aaa111111111".into())),
            "limite deve chegar via inspect em background"
        );
        assert!(c.in_flight.is_empty(), "in_flight deve limpar ao concluir");
    }

    #[tokio::test]
    async fn spawn_emits_at_least_one_batch() {
        let cfg = AppConfig {
            refresh_delay: 0.01,
            ..AppConfig::default()
        };
        let shared = Arc::new(parking_lot_like::RwLock::new(cfg));
        let caches = Arc::new(Mutex::new(Caches::default()));
        let p: Arc<dyn DockerProvider> = Arc::new(MockProvider::new(
            "default",
            vec![raw("aaa111111111", "web")],
        ));
        let (tx, mut rx) = mpsc::channel(4);
        let selected = Arc::new(Mutex::new("all".to_string()));
        let handle = spawn_collector(
            vec![p],
            caches,
            shared,
            selected,
            Arc::new(Logger::disabled()),
            tx,
        );
        let out = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("timeout")
            .expect("canal");
        assert_eq!(out.rows.len(), 1);
        handle.abort();
    }

    #[tokio::test]
    async fn fresh_system_cache_skips_info_and_inspect_for_empty_context() {
        let cfg = AppConfig::default();
        let mut initial = Caches::default();
        initial.sysinfo.insert("cached".into(), (6, 12_000));
        initial
            .sysinfo_ts
            .insert("cached".into(), std::time::Instant::now());
        let caches = Arc::new(Mutex::new(initial));
        let provider: Arc<dyn DockerProvider> = Arc::new(MockProvider::new("cached", Vec::new()));

        let out = collect_once(&[provider], &caches, &cfg, &Logger::disabled(), true).await;
        assert!(out.rows.is_empty());
        assert_eq!(out.totals_cores, 6);
        assert_eq!(out.totals_mem, 12_000);
    }

    #[tokio::test]
    async fn info_failure_without_cached_values_keeps_collection_available() {
        let cfg = AppConfig::default();
        let caches = Arc::new(Mutex::new(Caches::default()));
        let provider: Arc<dyn DockerProvider> = Arc::new(InfoFailProvider("uncached"));

        let out = collect_once(&[provider], &caches, &cfg, &Logger::disabled(), true).await;

        assert_eq!(out.rows.len(), 1);
        assert!(out.errors.is_empty());
        assert_eq!((out.totals_cores, out.totals_mem), (0, 0));
    }

    #[tokio::test]
    async fn stale_system_cache_is_used_when_info_and_inspect_fail() {
        let cfg = AppConfig::default();
        let mut initial = Caches::default();
        initial.sysinfo.insert("stale".into(), (3, 9_000));
        initial.sysinfo_ts.insert(
            "stale".into(),
            std::time::Instant::now() - crate::provider::SYSINFO_TTL - Duration::from_secs(1),
        );
        let caches = Arc::new(Mutex::new(initial));
        let provider: Arc<dyn DockerProvider> = Arc::new(InfoFailProvider("stale"));

        let out = collect_once(&[provider], &caches, &cfg, &Logger::disabled(), true).await;
        assert_eq!(out.rows.len(), 1);
        assert_eq!(out.totals_cores, 3);
        assert_eq!(out.totals_mem, 9_000);
        assert!(out.errors.is_empty());
    }

    #[tokio::test]
    async fn spawn_filters_providers_by_selected_context() {
        let cfg = AppConfig {
            refresh_delay: 0.01,
            ..AppConfig::default()
        };
        let shared = Arc::new(parking_lot_like::RwLock::new(cfg));
        let caches = Arc::new(Mutex::new(Caches::default()));
        let providers: Vec<Arc<dyn DockerProvider>> = vec![
            Arc::new(MockProvider::new(
                "default",
                vec![raw("aaa111111111", "web")],
            )),
            Arc::new(MockProvider::new("remote", vec![raw("bbb222222222", "db")])),
        ];
        let (tx, mut rx) = mpsc::channel(1);
        let selected = Arc::new(Mutex::new("default".to_string()));
        let handle = spawn_collector(
            providers,
            caches,
            shared,
            selected,
            Arc::new(Logger::disabled()),
            tx,
        );
        let out = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("timeout")
            .expect("channel");
        assert_eq!(out.rows.len(), 1);
        assert_eq!(out.rows[0].context, "default");
        handle.abort();
    }

    #[tokio::test]
    async fn empty_provider_set_and_closed_output_channel_terminate_cleanly() {
        let cfg = AppConfig::default();
        let caches = Arc::new(Mutex::new(Caches::default()));
        let out = collect_once(&[], &caches, &cfg, &Logger::disabled(), true).await;
        assert!(out.rows.is_empty());
        assert!(out.errors.is_empty());

        let shared = Arc::new(parking_lot_like::RwLock::new(cfg));
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let selected = Arc::new(Mutex::new("no-context".to_string()));
        let handle = spawn_collector(
            vec![],
            caches,
            shared,
            selected,
            Arc::new(Logger::disabled()),
            tx,
        );
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("closed receiver should stop collector")
            .expect("collector task should finish");
    }
}
