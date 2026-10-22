//! Parsers sem regex pesada (iteração manual) — port fiel do Python.
//!
//! Cobertura: unit tests + proptest (ver `tests` + `parsers_proptest`).

/// `12.5%` / `12.5` → 12.5. Vazio/inválido → 0.
pub fn parse_percent(s: &str) -> f64 {
    let t = s.trim().trim_end_matches('%').trim();
    if t.is_empty() {
        return 0.0;
    }
    t.parse::<f64>().unwrap_or(0.0)
}

/// `--`, `-`, vazio → 0.
pub fn parse_pid(s: &str) -> u32 {
    let t = s.trim();
    if t.is_empty() || t == "--" || t == "-" {
        return 0;
    }
    t.parse::<u32>().unwrap_or(0)
}

/// `1.5GiB`, `512MiB`, `100MB`, `0B` → bytes. Sem unidade → valor bruto.
pub fn parse_bytes(s: &str) -> f64 {
    let t = s.trim();
    if t.is_empty() {
        return 0.0;
    }
    // Separa prefixo numérico do sufixo de unidade sem regex.
    let mut num_end = 0usize;
    for (i, c) in t.char_indices() {
        if c.is_ascii_digit() || c == '.' || c == '+' || c == '-' || c == 'e' || c == 'E' {
            num_end = i + c.len_utf8();
        } else {
            break;
        }
    }
    // `+`/`-`/`e` no meio podem confundir; fallback: tenta split em espaço.
    let (num_part, unit_part) = if num_end == 0 {
        // tenta "1.5 GiB" com espaço? num_end para no espaço; ok.
        (t, "")
    } else {
        t.split_at(num_end)
    };
    let val: f64 = num_part.trim().parse().unwrap_or(0.0);
    if val == 0.0 && !(num_part.trim() == "0" || num_part.trim().starts_with("0.")) {
        // parse falhou de verdade (ex.: "--")
        if num_part.trim().parse::<f64>().is_err() {
            // tenta extrair número inicial de forma tolerante
            let fallback: String = t
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            if fallback.is_empty() {
                return 0.0;
            }
            let v: f64 = fallback.parse().unwrap_or(0.0);
            return v * unit_multiplier(t[fallback.len()..].trim());
        }
    }
    val * unit_multiplier(unit_part)
}

fn unit_multiplier(unit: &str) -> f64 {
    // Normaliza: minúsculo, sem espaços.
    let mut u: String = unit.trim().to_lowercase();
    u.retain(|c| c != ' ');
    match u.as_str() {
        "" | "b" => 1.0,
        "kib" => 1024.0,
        "mib" => 1024.0 * 1024.0,
        "gib" => 1024.0 * 1024.0 * 1024.0,
        "tib" => 1024.0_f64.powi(4),
        "k" | "kb" => 1000.0,
        "m" | "mb" => 1000.0 * 1000.0,
        "g" | "gb" => 1000.0 * 1000.0 * 1000.0,
        "t" | "tb" => 1000.0_f64.powi(4),
        _ => 1.0,
    }
}

/// `rx / tx` → (rx, tx). Sem `/` → (valor, 0).
pub fn parse_double_bytes(s: &str) -> (f64, f64) {
    match s.split_once('/') {
        Some((a, b)) => (parse_bytes(a), parse_bytes(b)),
        None => (parse_bytes(s), 0.0),
    }
}

/// `0-2` → 3, `0,2` → 2, `""` → `fallback`.
pub fn parse_cpuset_count(cpuset: &str, fallback: u32) -> u32 {
    let t = cpuset.trim();
    if t.is_empty() {
        return fallback;
    }
    let mut count = 0u32;
    for part in t.split(',') {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        if let Some((a, b)) = p.split_once('-') {
            match (a.trim().parse::<i64>(), b.trim().parse::<i64>()) {
                (Ok(s), Ok(e)) if e >= s => count += (e - s + 1) as u32,
                _ => count += 1,
            }
        } else {
            count += 1;
        }
    }
    if count == 0 {
        fallback
    } else {
        count
    }
}

/// Linha do `docker stats --format "{{.ID}}\t{{.Name}}\t..."` (8 campos tab).
/// Retorna `None` se malformada.
pub fn parse_stats_line(line: &str) -> Option<[String; 8]> {
    parts_ok(line)
}

fn parts_ok(line: &str) -> Option<[String; 8]> {
    let line = strip_ansi(line);
    let parts: Vec<&str> = line.split('\t').collect();
    if parts.len() < 8 {
        return None;
    }
    Some([
        parts[0].to_string(),
        parts[1].to_string(),
        parts[2].to_string(),
        parts[3].to_string(),
        parts[4].to_string(),
        parts[5].to_string(),
        parts[6].to_string(),
        parts[7].to_string(),
    ])
}

/// Linha do `docker inspect --format "{{.Id}}\t{{.HostConfig.NanoCpus}}\t..."` (6 campos).
pub fn parse_inspect_line(line: &str) -> Option<(String, i64, i64, i64, String, i64)> {
    let parts: Vec<&str> = line.split('\t').collect();
    if parts.len() < 6 {
        return None;
    }
    Some((
        parts[0].to_string(),
        parts[1].trim().parse().unwrap_or(0),
        parts[2].trim().parse().unwrap_or(0),
        parts[3].trim().parse().unwrap_or(0),
        parts[4].trim().to_string(),
        parts[5].trim().parse().unwrap_or(0),
    ))
}

/// Remove escapes ANSI sem regex (iteração byte a byte, igual ao Python).
pub fn strip_ansi(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            i += 2;
            while i < bytes.len() && !(bytes[i] >= 0x40 && bytes[i] <= 0x7E) {
                i += 1;
            }
            i += 1; // consome o byte final
        } else {
            // preserva UTF-8: avança por char
            let ch = s[i..].chars().next().expect("char boundary");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// Formata bytes em unidade binária (GiB/MiB/KiB/B) — igual ao Python.
pub fn format_bytes_human(val: f64, precision: usize) -> String {
    format_bytes_human_fmt(val, precision, false)
}

/// Variante decimal (GB/MB/KB/B) quando `display.byte_format == "decimal"`.
pub fn format_bytes_human_fmt(val: f64, precision: usize, decimal: bool) -> String {
    if decimal {
        const G: f64 = 1000.0 * 1000.0 * 1000.0;
        const M: f64 = 1000.0 * 1000.0;
        const K: f64 = 1000.0;
        if val >= G {
            format!("{:.*} GB", precision, val / G)
        } else if val >= M {
            format!("{:.*} MB", precision, val / M)
        } else if val >= K {
            format!("{:.*} KB", precision, val / K)
        } else {
            format!("{:.0} B", val.max(0.0))
        }
    } else {
        const G: f64 = 1024.0 * 1024.0 * 1024.0;
        const M: f64 = 1024.0 * 1024.0;
        const K: f64 = 1024.0;
        if val >= G {
            format!("{:.*} GiB", precision, val / G)
        } else if val >= M {
            format!("{:.*} MiB", precision, val / M)
        } else if val >= K {
            format!("{:.*} KiB", precision, val / K)
        } else {
            format!("{:.0} B", val.max(0.0))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_basics() {
        assert!((parse_percent("12.5%") - 12.5).abs() < 1e-9);
        assert!((parse_percent(" 3.0 ") - 3.0).abs() < 1e-9);
        assert_eq!(parse_percent(""), 0.0);
        assert_eq!(parse_percent("abc"), 0.0);
    }

    #[test]
    fn pid_edges() {
        assert_eq!(parse_pid("38"), 38);
        assert_eq!(parse_pid("--"), 0);
        assert_eq!(parse_pid("-"), 0);
        assert_eq!(parse_pid(""), 0);
        assert_eq!(parse_pid("xx"), 0);
    }

    #[test]
    fn bytes_units_binary_and_decimal() {
        assert!((parse_bytes("1GiB") - 1024.0f64.powi(3)).abs() < 1.0);
        assert!((parse_bytes("512MiB") - 512.0 * 1024.0 * 1024.0).abs() < 1.0);
        assert!((parse_bytes("1KiB") - 1024.0).abs() < 1e-9);
        assert!((parse_bytes("1GB") - 1e9).abs() < 1.0);
        assert!((parse_bytes("1MB") - 1e6).abs() < 1.0);
        assert!((parse_bytes("0B") - 0.0).abs() < 1e-9);
        assert!((parse_bytes("42") - 42.0).abs() < 1e-9);
        assert_eq!(parse_bytes(""), 0.0);
        assert_eq!(parse_bytes("--"), 0.0);
    }

    #[test]
    fn double_bytes_split() {
        let (a, b) = parse_double_bytes("1MiB / 2MiB");
        assert!((a - 1024.0 * 1024.0).abs() < 1.0);
        assert!((b - 2.0 * 1024.0 * 1024.0).abs() < 1.0);
        let (c, d) = parse_double_bytes("5MiB");
        assert!(c > 0.0 && d == 0.0);
    }

    #[test]
    fn cpuset_counts() {
        assert_eq!(parse_cpuset_count("0-2", 8), 3);
        assert_eq!(parse_cpuset_count("0,2", 8), 2);
        assert_eq!(parse_cpuset_count("", 8), 8);
        assert_eq!(parse_cpuset_count("0-1,3", 8), 3);
    }

    #[test]
    fn stats_line_ok_and_bad() {
        let good = "abc\tname\t1.0%\t10MiB / 1GiB\t2%\t1kB / 2kB\t3kB / 4kB\t5";
        assert!(parse_stats_line(good).is_some());
        assert!(parse_stats_line("curta\tlinha").is_none());
    }

    #[test]
    fn inspect_line_ok_and_bad() {
        let good = "abcdef123456\t0\t0\t0\t\t536870912";
        let p = parse_inspect_line(good).unwrap();
        assert_eq!(p.0, "abcdef123456");
        assert_eq!(p.5, 536870912);
        assert!(parse_inspect_line("a\tb").is_none());
    }

    #[test]
    fn ansi_stripped() {
        assert_eq!(strip_ansi("\x1b[32mok\x1b[0m"), "ok");
        assert_eq!(strip_ansi("plain"), "plain");
    }

    #[test]
    fn human_format_thresholds() {
        assert!(format_bytes_human(2.0 * 1024.0f64.powi(3), 2).contains("GiB"));
        assert!(format_bytes_human(3.0 * 1024.0 * 1024.0, 2).contains("MiB"));
        assert!(format_bytes_human(2048.0, 2).contains("KiB"));
        assert!(format_bytes_human(10.0, 2).contains("B"));
    }
}
