/// Format a `Duration` exactly like Go's `time.Duration.String`.
///
/// Go stores a duration as a signed nanosecond count; Rust's `Duration` is
/// non-negative, so leading `-` handling is never needed. Units, fractional
/// seconds and the `0s` zero are a direct port of Go's `Duration.String` and
/// its `fmtFrac` / `fmtInt` helpers.
pub fn format_go_duration(d: std::time::Duration) -> String {
    const SECOND: u64 = 1_000_000_000;
    const MILLI: u64 = 1_000_000;
    const MICRO: u64 = 1_000;

    let u: u64 = d.as_nanos() as u64;

    // Go's switch has `case u == 0: return "0s"`.
    if u == 0 {
        return "0s".to_string();
    }

    let mut buf = [0u8; 64];
    let mut w: usize = buf.len();

    if u < SECOND {
        let (prec, unit): (usize, &[u8]) = if u < MICRO {
            (0, b"ns")
        } else if u < MILLI {
            (3, "µs".as_bytes())
        } else {
            (6, b"ms")
        };
        w -= unit.len();
        buf[w..w + unit.len()].copy_from_slice(unit);
        let (nw, v) = fmt_frac(&mut buf, w, u, prec);
        w = fmt_int(&mut buf, nw, v);
    } else {
        w -= 1;
        buf[w] = b's';
        let (nw, mut v) = fmt_frac(&mut buf, w, u, 9);
        w = fmt_int(&mut buf, nw, v % 60);
        v /= 60;
        if v > 0 {
            w -= 1;
            buf[w] = b'm';
            w = fmt_int(&mut buf, w, v % 60);
            v /= 60;
            if v > 0 {
                w -= 1;
                buf[w] = b'h';
                w = fmt_int(&mut buf, w, v);
            }
        }
    }

    String::from_utf8(buf[w..].to_vec()).unwrap()
}

/// Port of Go's `fmtFrac`: write the fractional part (trailing zeros omitted,
/// and the decimal point omitted when the fraction is zero), then return the
/// output start index and the integer part `v / 10^prec`.
fn fmt_frac(buf: &mut [u8], mut w: usize, mut v: u64, prec: usize) -> (usize, u64) {
    let mut print = false;
    for _ in 0..prec {
        let digit = v % 10;
        if digit != 0 {
            print = true;
        }
        if print {
            w -= 1;
            buf[w] = digit as u8 + b'0';
        }
        v /= 10;
    }
    if print {
        w -= 1;
        buf[w] = b'.';
    }
    (w, v)
}

/// Port of Go's `fmtInt`: write `v` in decimal at the tail of `buf`.
fn fmt_int(buf: &mut [u8], mut w: usize, mut v: u64) -> usize {
    if v == 0 {
        w -= 1;
        buf[w] = b'0';
    } else {
        while v > 0 {
            w -= 1;
            buf[w] = (v % 10) as u8 + b'0';
            v /= 10;
        }
    }
    w
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn brief_cases() {
        assert_eq!(format_go_duration(Duration::from_millis(500)), "500ms");
        assert_eq!(format_go_duration(Duration::from_secs(60)), "1m0s");
        assert_eq!(format_go_duration(Duration::from_secs(3600)), "1h0m0s");
    }

    #[test]
    fn go_duration_string_goldens() {
        // Goldens captured from Go 1.27 `time.Duration(ns).String()`.
        let cases: &[(u64, &str)] = &[
            (0, "0s"),
            (1, "1ns"),
            (999, "999ns"),
            (1000, "1µs"),
            (1500, "1.5µs"),
            (999999, "999.999µs"),
            (1000000, "1ms"),
            (1500000, "1.5ms"),
            (999999999, "999.999999ms"),
            (1000000000, "1s"),
            (1500000000, "1.5s"),
            (59999999999, "59.999999999s"),
            (60000000000, "1m0s"),
            (61500000000, "1m1.5s"),
            (3600000000000, "1h0m0s"),
            (3661000000000, "1h1m1s"),
            (3661000000001, "1h1m1.000000001s"),
            (500000000, "500ms"),
            (90061001000000, "25h1m1.001s"),
            (123456789000, "2m3.456789s"),
            (2500000000, "2.5s"),
        ];
        for &(ns, want) in cases {
            let got = format_go_duration(Duration::from_nanos(ns));
            assert_eq!(got, want, "ns={ns}");
        }
    }
}
