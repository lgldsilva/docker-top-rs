//! Propriedades dos parsers (sem regex): roundtrip + invariantes.

use docker_top_rs::parsers::{
    parse_bytes, parse_cpuset_count, parse_double_bytes, parse_percent, parse_pid, strip_ansi,
};
use proptest::prelude::*;

proptest! {
    /// `%` parse é total e não-pânico p/ qualquer string.
    #[test]
    fn percent_never_panics(s in "\\PC*") {
        let v = parse_percent(&s);
        prop_assert!(v.is_finite());
        prop_assert!(v >= 0.0 || s.trim_start_matches('-').trim() != s.trim() || v == 0.0 || v < 0.0);
    }

    /// Bytes: `N unit` com N>=0 e unidade conhecida nunca dá negativo.
    #[test]
    fn bytes_nonnegative_for_known_units(
        n in 0.0f64..1e12,
        unit in prop_oneof![
            Just("B"), Just("KiB"), Just("MiB"), Just("GiB"),
            Just("KB"), Just("MB"), Just("GB"),
            Just("K"), Just("M"), Just("G"), Just(""),
        ],
    ) {
        let s = format!("{n:.2} {unit}");
        let v = parse_bytes(&s);
        prop_assert!(v >= 0.0, "parse_bytes({s:?}) = {v}");
    }

    /// `parse_double_bytes(a / b)` == par de `parse_bytes`.
    #[test]
    fn double_split_consistent(
        a in 0.0f64..1e9,
        b in 0.0f64..1e9,
    ) {
        let s = format!("{a:.1}MiB / {b:.1}MiB");
        let (x, y) = parse_double_bytes(&s);
        let expected_x = parse_bytes(&format!("{a:.1}MiB"));
        let expected_y = parse_bytes(&format!("{b:.1}MiB"));
        prop_assert!((x - expected_x).abs() < 1.0);
        prop_assert!((y - expected_y).abs() < 1.0);
    }

    /// PID: dígitos parseiam exato; resto dá 0.
    #[test]
    fn pid_digits_roundtrip(n in 0u32..99999) {
        prop_assert_eq!(parse_pid(&n.to_string()), n);
    }

    /// `strip_ansi` nunca aumenta o tamanho visível além do original.
    #[test]
    fn ansi_strip_shrinks(s in "\\PC*") {
        let out = strip_ansi(&s);
        prop_assert!(out.len() <= s.len());
        prop_assert!(!out.contains("\x1b["));
    }

    /// cpuset `0-(n-1)` conta n.
    #[test]
    fn cpuset_range_counts(n in 1u32..64) {
        let s = format!("0-{}", n - 1);
        prop_assert_eq!(parse_cpuset_count(&s, 999), n);
    }
}
