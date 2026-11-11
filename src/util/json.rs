// Go's canonicalRequestID: string -> "string:<v>"; integral float64/int/int64
// -> "int64:<decimal>"; non-integral float64 -> "float64:<FormatFloat('f',-1,64)>".
// JSON numbers decode as f64 in Go (both in the before-call hook and in
// notifications/cancelled), so mirror that by converting serde_json::Number to
// f64 first. This also matches Go's loss of precision for integers past 2^53.
//
// Go's int64(f) conversion is implementation-defined outside [MinInt64, 2^63).
// On amd64 2^63 wraps to MinInt64, so the equality check fails and Go emits the
// float64 form; Rust's saturating cast would instead land on MaxInt64, whose
// round-trip back to f64 is exactly 2^63 and would wrongly integralize. Gate the
// conversion to the half-open range Go can represent, then require the
// round-trip.
#[allow(clippy::float_cmp)] // integral detection is an intentional exact compare
pub fn canonical_request_id(v: &serde_json::Value) -> Option<String> {
    const INT64_MIN_F64: f64 = -9_223_372_036_854_775_808.0;
    const INT64_MAX_EXCLUSIVE_F64: f64 = 9_223_372_036_854_775_808.0;

    match v {
        serde_json::Value::String(s) => Some(format!("string:{s}")),
        serde_json::Value::Number(n) => {
            let f = n.as_f64()?;
            if (INT64_MIN_F64..INT64_MAX_EXCLUSIVE_F64).contains(&f) {
                let i = f as i64;
                if f == i as f64 {
                    return Some(format!("int64:{i}"));
                }
            }
            // strconv.FormatFloat(v, 'f', -1, 64) matches Rust's f64
            // Display: the shortest fixed-point decimal that round-trips.
            Some(format!("float64:{f}"))
        }
        _ => None,
    }
}

/// Go `strconv.Quote`-compatible quoting for approval fields. Escapes control
/// bytes, quotes, backslashes and non-printable runes while keeping printable
/// Unicode literal. Deliberately not Rust's `{:?}` (different escape rules).
pub fn go_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' {
            out.push_str("\\\"");
        } else if c == '\\' {
            out.push_str("\\\\");
        } else if is_print(c) {
            out.push(c);
        } else {
            let u = c as u32;
            match u {
                0x07 => out.push_str("\\a"),
                0x08 => out.push_str("\\b"),
                0x09 => out.push_str("\\t"),
                0x0a => out.push_str("\\n"),
                0x0b => out.push_str("\\v"),
                0x0c => out.push_str("\\f"),
                0x0d => out.push_str("\\r"),
                x if x < 0x20 || x == 0x7f => out.push_str(&format!("\\x{x:02x}")),
                x if x < 0x10000 => out.push_str(&format!("\\u{x:04x}")),
                x => out.push_str(&format!("\\U{x:08x}")),
            }
        }
    }
    out.push('"');
    out
}

/// Exact Go-compatible `unicode.IsPrint` (the predicate `strconv.Quote` uses),
/// evaluated by binary search over the generated `GO_PRINT_RANGES` table. This
/// replaces the earlier Rust heuristic, which wrongly treated Cf/Co/Cn/Zs
/// runes (bidi controls, zero-width chars, private-use, unassigned, non-ASCII
/// spaces) as printable and emitted them literally — a prompt-injection vector
/// in approval fields.
fn is_print(c: char) -> bool {
    let u = c as u32;
    super::print_ranges::GO_PRINT_RANGES
        .binary_search_by(|&(start, end)| {
            if u < start {
                std::cmp::Ordering::Greater
            } else if u > end {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// Truncate to `max` bytes on a UTF-8 boundary, appending `…` when it had to
/// cut. Matches Go's `truncateArguments`.
pub fn truncate_utf8(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut cut = max;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    let head = &s[..cut];
    format!("{head}…")
}

/// Truncate to `max` Unicode scalar values (Go `[]rune`), appending `…` when
/// it had to cut. Matches Go's `truncateProgressSummary`.
pub fn truncate_runes(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonical_request_id_brief_cases() {
        assert_eq!(canonical_request_id(&json!(1)), Some("int64:1".into()));
        assert_eq!(canonical_request_id(&json!(1.0)), Some("int64:1".into()));
        assert_eq!(canonical_request_id(&json!(1e0)), Some("int64:1".into()));
        assert_eq!(
            canonical_request_id(&json!(1.5)),
            Some("float64:1.5".into())
        );
        assert_eq!(
            canonical_request_id(&json!("abc")),
            Some("string:abc".into())
        );
        assert_eq!(canonical_request_id(&json!(null)), None);
    }

    #[test]
    fn canonical_request_id_more_types() {
        assert_eq!(canonical_request_id(&json!(0)), Some("int64:0".into()));
        assert_eq!(canonical_request_id(&json!(2.0)), Some("int64:2".into()));
        assert_eq!(canonical_request_id(&json!(1e2)), Some("int64:100".into()));
        assert_eq!(canonical_request_id(&json!(-3)), Some("int64:-3".into()));
        assert_eq!(
            canonical_request_id(&json!(2.5)),
            Some("float64:2.5".into())
        );
        assert_eq!(
            canonical_request_id(&json!(-3.5)),
            Some("float64:-3.5".into())
        );
        assert_eq!(canonical_request_id(&json!(true)), None);
        assert_eq!(canonical_request_id(&json!([1, 2])), None);
        assert_eq!(canonical_request_id(&json!({"a": 1})), None);
    }

    #[test]
    fn canonical_request_id_uses_float64_precision() {
        // Go decodes every JSON number as float64, so 2^53+1 loses its low bit.
        let v: serde_json::Value = serde_json::from_str("9007199254740993").unwrap();
        assert_eq!(
            canonical_request_id(&v),
            Some("int64:9007199254740992".into())
        );
    }

    #[test]
    fn canonical_request_id_matches_go_at_int64_boundaries() {
        // Go's int64(f) conversion is implementation-defined outside
        // [MinInt64, 2^63). On amd64 2^63 wraps to MinInt64, so the round-trip
        // check fails and FormatFloat('f', -1, 64) prints the positional
        // decimal form. Integralize only inside that half-open range.
        let cases: &[(&str, &str)] = &[
            ("9223372036854775808", "float64:9223372036854776000"), // 2^63
            ("9223372036854777856", "float64:9223372036854778000"), // 2^63 + ulp
            ("9223372036854773760", "int64:9223372036854773760"),   // 2^63 - ulp
            ("9223372036854774784", "int64:9223372036854774784"),
            ("-9223372036854775808", "int64:-9223372036854775808"), // MinInt64
            ("-9223372036854777856", "float64:-9223372036854778000"), // MinInt64 - ulp
        ];
        for &(raw, want) in cases {
            let v: serde_json::Value = serde_json::from_str(raw).unwrap();
            assert_eq!(canonical_request_id(&v).as_deref(), Some(want), "raw={raw}");
        }
    }

    #[test]
    fn go_quote_matches_go_strconv_quote() {
        // Goldens captured from Go 1.27 `strconv.Quote`. Covers ", \, \n, \t,
        // NUL, control escapes, DEL, ANSI, printable and high Unicode.
        let cases: &[(&str, &str)] = &[
            ("\"", "\"\\\"\""),                                     // double_quote
            ("\\", "\"\\\\\""),                                     // backslash
            ("line1\nline2", "\"line1\\nline2\""),                  // newline
            ("a\tb", "\"a\\tb\""),                                  // tab
            ("a\x00b", "\"a\\x00b\""),                              // nul
            ("café € 你好", "\"café € 你好\""),                     // non_ascii
            ("hello", "\"hello\""),                                 // plain
            ("\u{1b}[31mred\u{1b}[0m", "\"\\x1b[31mred\\x1b[0m\""), // ansi_red
            ("\u{7}\u{8}\u{c}\u{b}\r", "\"\\a\\b\\f\\v\\r\""),      // ctrl_escapes
            ("a\u{7f}b", "\"a\\x7fb\""),                            // del
            ("a\u{85}b", "\"a\\u0085b\""),                          // latin1_ctrl
            ("a\u{a0}b", "\"a\\u00a0b\""),                          // nbsp
            (
                "say \"hi\"\n\\path\t€\x00end",
                "\"say \\\"hi\\\"\\n\\\\path\\t€\\x00end\"",
            ), // mix
            ("😀", "\"😀\""),                                       // high_unicode
            ("\u{7}", "\"\\a\""),                                   // bell_only
            ("µs", "\"µs\""),                                       // micro
        ];
        for &(input, want) in cases {
            let got = go_quote(input);
            assert_eq!(got.as_str(), want, "input={input:?}");
        }
    }

    #[test]
    fn truncate_runes_ellipsis() {
        assert_eq!(truncate_runes("abcd", 3), "abc…");
        assert_eq!(truncate_runes("abc", 3), "abc");
        assert_eq!(truncate_runes("abc", 5), "abc");
        assert_eq!(truncate_runes("a€b😀cdef", 4), "a€b😀…");
    }

    #[test]
    fn truncate_utf8_ellipsis_and_no_cut() {
        assert_eq!(truncate_utf8("abcd", 3), "abc…");
        assert_eq!(truncate_utf8("abc", 3), "abc");
        assert_eq!(truncate_utf8("abc", 10), "abc");
    }

    #[test]
    fn truncate_utf8_backs_up_to_a_rune_boundary() {
        assert_eq!(truncate_utf8("a€b", 2), "a…");
        assert_eq!(truncate_utf8("a€b", 3), "a…");
        assert_eq!(truncate_utf8("a€b", 4), "a€…");
    }

    #[test]
    fn truncate_utf8_euro_boundary_ported_from_go() {
        // Ported from Go's TestRunnerRejectsTruncatedToolArguments: 500 bytes,
        // the 200-byte cut lands inside the 63rd euro sign, so it must back up
        // to the previous UTF-8 boundary.
        let mut input = String::from("{\"command\":\"");
        input.push_str(&"€".repeat(162));
        input.push('é');
        let mut want = String::from("{\"command\":\"");
        want.push_str(&"€".repeat(62));
        want.push('…');
        assert_eq!(truncate_utf8(&input, 200), want);
    }

    #[test]
    fn go_quote_non_printable_unicode_matches_go() {
        // Goldens captured from Go 1.26.6 `strconv.Quote`. Proves go_quote now
        // matches Go's unicode.IsPrint exactly for Cf/Co/Cn/Zs runes (bidi RLO,
        // zero-width, soft hyphen, BOM, private-use, unassigned, NBSP) while
        // keeping ASCII space and printable Unicode literal. Also re-pins the
        // quote/backslash/control behavior so this test is self-contained.
        let cases: &[(&str, &str)] = &[
            ("a\u{202e}b", "\"a\\u202eb\""), // rlo_u202e
            ("a\u{200b}b", "\"a\\u200bb\""), // zwsp_u200b
            ("a\u{ad}b", "\"a\\u00adb\""),   // shy_u00ad
            ("a\u{feff}b", "\"a\\ufeffb\""), // bom_ufeff
            ("a\u{e000}b", "\"a\\ue000b\""), // private_use_ue000
            ("a\u{378}b", "\"a\\u0378b\""),  // unassigned_u0378
            ("a b c", "\"a b c\""),          // ascii_space
            ("a\u{a0}b", "\"a\\u00a0b\""),   // nbsp_u00a0
            (
                "caf\u{e9} \u{20ac} \u{4f60}\u{597d}",
                "\"caf\u{e9} \u{20ac} \u{4f60}\u{597d}\"",
            ), // printable_letter
            ("\"", "\"\\\"\""),              // double_quote
            ("\\", "\"\\\\\""),              // backslash
            ("a\nb", "\"a\\nb\""),           // newline
            ("a\tb", "\"a\\tb\""),           // tab
            ("a\0b", "\"a\\x00b\""),         // nul
            ("\u{7}\u{8}\u{c}\u{b}\r", "\"\\a\\b\\f\\v\\r\""), // ctrl_escapes
            ("a\u{7f}b", "\"a\\x7fb\""),     // del
            ("a\u{85}b", "\"a\\u0085b\""),   // latin1_ctrl
            (
                "run \u{202e} echo \u{200b} done",
                "\"run \\u202e echo \\u200b done\"",
            ), // mixed_rlo
        ];
        for &(input, want) in cases {
            let got = go_quote(input);
            assert_eq!(got.as_str(), want, "input={input:?}");
        }
    }
}
