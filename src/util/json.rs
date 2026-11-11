// Go's canonicalRequestID: string -> "string:<v>"; integral float64/int/int64
// -> "int64:<decimal>"; non-integral float64 -> "float64:<FormatFloat('f',-1,64)>".
// JSON numbers decode as f64 in Go (both in the before-call hook and in
// notifications/cancelled), so mirror that by converting serde_json::Number to
// f64 first. This also matches Go's loss of precision for integers past 2^53.
#[allow(clippy::float_cmp)] // integral detection is an intentional exact compare
pub fn canonical_request_id(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(format!("string:{s}")),
        serde_json::Value::Number(n) => {
            let f = n.as_f64()?;
            let i = f as i64;
            if f == i as f64 {
                Some(format!("int64:{i}"))
            } else {
                // strconv.FormatFloat(v, 'f', -1, 64) matches Rust's f64
                // Display: the shortest fixed-point decimal that round-trips.
                Some(format!("float64:{f}"))
            }
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

/// Go `strconv.IsPrint` for the ASCII range plus an approximation for
/// non-ASCII. Go keeps categories L, M, N, P, S (and ASCII space) literal and
/// escapes the C* categories and non-ASCII separators. Rust's std has no
/// general-category table, so use "not a control and not a whitespace". This is
/// exact for every approval golden case; exotic format/private-use/unassigned
/// runes are the only approximation.
fn is_print(c: char) -> bool {
    let u = c as u32;
    if u < 0x20 || u == 0x7f {
        return false;
    }
    if u < 0x7f {
        return true;
    }
    !c.is_control() && !c.is_whitespace()
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
}
