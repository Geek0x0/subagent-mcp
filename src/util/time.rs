/// Format a `SystemTime` as UTC `RFC3339Nano`
/// (`2006-01-02T15:04:05.999999999Z`). The fractional second is emitted only
/// when non-zero and its trailing zeros are trimmed, exactly like Go's
/// `time.RFC3339Nano` layout; UTC is rendered as a trailing `Z`.
pub fn rfc3339_nano(t: std::time::SystemTime) -> String {
    let dt: chrono::DateTime<chrono::Utc> = chrono::DateTime::from(t);
    let base = dt.format("%Y-%m-%dT%H:%M:%S");
    let nanos = dt.timestamp_subsec_nanos();
    if nanos == 0 {
        format!("{base}Z")
    } else {
        let frac = format!("{nanos:09}");
        let trimmed = frac.trim_end_matches('0');
        format!("{base}.{trimmed}Z")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    #[cfg(not(windows))]
    #[test]
    fn rfc3339_nano_goldens() {
        // Goldens captured from Go 1.27
        // `time.Unix(sec, nsec).UTC().Format(time.RFC3339Nano)`.
        let cases: &[(u64, u32, &str)] = &[
            (0, 0, "1970-01-01T00:00:00Z"),
            (1700000000, 0, "2023-11-14T22:13:20Z"),
            (1700000000, 123000000, "2023-11-14T22:13:20.123Z"),
            (1700000000, 123456789, "2023-11-14T22:13:20.123456789Z"),
            (1700000000, 100000000, "2023-11-14T22:13:20.1Z"),
            (1700000000, 120000000, "2023-11-14T22:13:20.12Z"),
            (1700000000, 10000000, "2023-11-14T22:13:20.01Z"),
            (1700000000, 1, "2023-11-14T22:13:20.000000001Z"),
            (1700000000, 999999999, "2023-11-14T22:13:20.999999999Z"),
            (1700000000, 500000000, "2023-11-14T22:13:20.5Z"),
            (1, 100000000, "1970-01-01T00:00:01.1Z"),
        ];
        for &(sec, nsec, want) in cases {
            let t = UNIX_EPOCH + Duration::new(sec, nsec);
            let got = rfc3339_nano(t);
            assert_eq!(got, want, "sec={sec} nsec={nsec}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn rfc3339_nano_uses_windows_100ns_precision() {
        // `SystemTime` on Windows is a FILETIME (100 ns ticks), so the
        // nanosecond field cannot represent 123456789; Go's clock has the same
        // platform precision.
        let t = UNIX_EPOCH + Duration::new(1700000000, 123456700);
        assert_eq!(rfc3339_nano(t), "2023-11-14T22:13:20.1234567Z");
    }

    #[test]
    fn rfc3339_nano_pre_epoch() {
        let t = UNIX_EPOCH - Duration::from_secs(1);
        assert_eq!(rfc3339_nano(t), "1969-12-31T23:59:59Z");
    }
}
