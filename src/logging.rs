//! Log opcional em arquivo (`--log <path>` / env `DOCKER_TOP_LOG`).
//!
//! Sem dependências novas: timestamp civil derivado de `SystemTime`
//! (mesmo algoritmo de `days_to_ymd` do relógio da TUI, movido p/ cá).
//! Desligado por padrão — `Logger::disabled()` é no-op.

use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Logger append-only; `None` = desligado (no-op). Clonável p/ tasks paralelas.
#[derive(Clone)]
pub struct Logger {
    file: Option<std::sync::Arc<Mutex<std::fs::File>>>,
}

impl Logger {
    /// Abre (trunca) o arquivo de log; sem path → desligado.
    pub fn open(path: Option<&Path>) -> Self {
        let file = path.and_then(|p| {
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::File::create(p)
                .ok()
                .map(|f| std::sync::Arc::new(Mutex::new(f)))
        });
        Self { file }
    }

    /// Logger sem arquivo (no-op) — testes e modo padrão.
    pub fn disabled() -> Self {
        Self { file: None }
    }

    /// Escreve uma linha `"<ts> <msg>"`. Falha de I/O é ignorada (log best-effort).
    pub fn log(&self, msg: impl AsRef<str>) {
        let msg = msg.as_ref();
        if let Some(f) = &self.file {
            use std::io::Write;
            if let Ok(mut f) = f.lock() {
                let _ = writeln!(f, "{} {}", timestamp(), msg);
            }
        }
    }

    pub fn enabled(&self) -> bool {
        self.file.is_some()
    }
}

/// Timestamp civil `YYYY-MM-DD HH:MM:SS` (UTC) sem dependências.
pub fn timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d) = days_to_ymd(secs / 86400);
    format!(
        "{y:04}-{mo:02}-{d:02} {:02}:{:02}:{:02}",
        (secs / 3600) % 24,
        (secs / 60) % 60,
        secs % 60
    )
}

/// Howard Hinnant `days_from_civil` inverso, epoch 1970-01-01.
pub fn days_to_ymd(days: u64) -> (i32, u32, u32) {
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    ((if m <= 2 { y + 1 } else { y }) as i32, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_date_conversion_crosses_month_and_year_boundaries() {
        assert_eq!(days_to_ymd(0), (1970, 1, 1));
        assert_eq!(days_to_ymd(11_017), (2000, 3, 1));
        assert_eq!(days_to_ymd(11_231), (2000, 10, 1));
        assert_eq!(days_to_ymd(11_322), (2000, 12, 31));
    }

    #[test]
    fn timestamp_shape_is_iso_like() {
        let ts = timestamp();
        let parts: Vec<_> = ts.split(&['-', ' ', ':'][..]).collect();
        assert_eq!(parts.len(), 6, "timestamp '{ts}' deve ter 6 campos");
        assert_eq!(parts[0].len(), 4);
        for p in &parts[1..] {
            assert_eq!(p.len(), 2);
            assert!(p.chars().all(|c| c.is_ascii_digit()), "'{p}' não é dígito");
        }
    }

    #[test]
    fn disabled_logger_is_noop_and_enabled_reports() {
        let l = Logger::disabled();
        l.log("nada deve acontecer");
        assert!(!l.enabled());

        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("dtlog.log");
        let l = Logger::open(Some(&p));
        assert!(l.enabled());
        l.log("primeira linha");
        l.log("segunda linha");
        let txt = std::fs::read_to_string(&p).unwrap();
        let lines: Vec<&str> = txt.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(
            lines[0].ends_with(" primeira linha"),
            "linha 0: {}",
            lines[0]
        );
        assert!(
            lines[1].ends_with(" segunda linha"),
            "linha 1: {}",
            lines[1]
        );
    }

    #[test]
    fn open_with_missing_parent_creates_it() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("sub").join("x.log");
        let l = Logger::open(Some(&p));
        assert!(l.enabled());
        l.log("ok");
        assert!(p.is_file());
    }
}
