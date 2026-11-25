//! Regressão (2026-11-25): testes de `config.rs` e `logging.rs` criavam
//! `temp_dir()/dt*-{pid}` e nunca removiam. Sob `cargo mutants` (milhares de
//! execuções) isso deixou ~94 mil diretórios em /tmp do homeserver.
//!
//! Novos testes devem usar `tempfile::tempdir()` (removido no Drop). Os
//! arquivos da allowlist usam `temp_dir()` com `remove_dir_all` explícito.

use std::fs;
use std::path::Path;

const ALLOWLIST: &[&str] = &["main.rs", "provider.rs", "transport.rs"];

#[test]
fn src_tests_do_not_use_raw_temp_dir() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    for entry in fs::read_dir(&src).expect("ler src/") {
        let path = entry.expect("entrada").path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if path.extension().is_none_or(|e| e != "rs") || ALLOWLIST.contains(&name.as_str()) {
            continue;
        }
        let text = fs::read_to_string(&path).expect("ler arquivo");
        for (i, line) in text.lines().enumerate() {
            if line.contains("std::env::temp_dir()") {
                offenders.push(format!("{name}:{}", i + 1));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "use tempfile::tempdir() em vez de std::env::temp_dir() (vaza em /tmp): {offenders:?}"
    );
}
