//! Valida o golden Python (`docs/golden-python-once.txt`): schema estável.
//!
//! Compara colunas, ordem, unidades, exit codes documentados e edges
//! (sem limite → `(H)`; contexto down → linha de erro, sem crash).

use std::path::PathBuf;

fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/golden-python-once.txt")
}

fn golden_text() -> String {
    let p = golden_path();
    assert!(
        p.is_file(),
        "golden ausente: rode scripts/capture-golden.sh ({})",
        p.display()
    );
    std::fs::read_to_string(&p).expect("lê golden")
}

#[test]
fn golden_exists_and_nonempty() {
    let t = golden_text();
    assert!(t.lines().count() > 5, "golden muito curto");
}

#[test]
fn golden_schema_header_columns_order() {
    let t = golden_text();
    // header rich-table (larguras truncadas com …): ID + NAME + MEM.
    let header = t
        .lines()
        .find(|l| {
            let u = l.to_uppercase();
            u.contains("ID") && u.contains("NAME") && u.contains("MEM")
        })
        .expect("header com ID/NAME/MEM no golden");
    let u = header.to_uppercase();
    let pos = |frag: &str| u.find(frag).unwrap();
    assert!(pos("ID") < pos("NAME"), "ordem ID<NAME em {header:?}");
    assert!(pos("NAME") < pos("MEM"), "ordem NAME<MEM em {header:?}");
}

#[test]
fn golden_units_and_edges_documented() {
    let t = golden_text();
    // unidades humanas presentes (GiB/MiB ou máscara)
    assert!(
        t.contains("GiB") || t.contains("MiB") || t.contains("<BYTES>"),
        "unidades ausentes"
    );
    // metadados do golden registram exit code + edges
    assert!(
        t.contains("exit="),
        "golden deve registrar exit code do python --once"
    );
    assert!(
        t.contains("edge") || t.contains("EDGE") || t.contains("sem limite") || t.contains("(H)"),
        "edges (sem limite / contexto down) devem estar documentados"
    );
}
