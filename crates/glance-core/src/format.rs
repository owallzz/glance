//! Small formatting helpers shared by the UI.

/// "1.4 MB", "820 KB", "12 B". Decimal units, like file managers.
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if n < 1000 {
        return format!("{n} B");
    }
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1000.0 && unit < UNITS.len() - 1 {
        v /= 1000.0;
        unit += 1;
    }
    if v < 10.0 {
        format!("{v:.1} {}", UNITS[unit])
    } else {
        format!("{v:.0} {}", UNITS[unit])
    }
}

/// "0:42", "12:05", "1:02:03".
pub fn duration(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Seconds left for a transfer, if the rate is known.
pub fn eta_secs(remaining: u64, bytes_per_sec: u64) -> Option<u64> {
    (bytes_per_sec > 0).then(|| remaining.div_ceil(bytes_per_sec))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes() {
        assert_eq!(human_bytes(12), "12 B");
        assert_eq!(human_bytes(1_400_000), "1.4 MB");
        assert_eq!(human_bytes(820_000), "820 KB");
        assert_eq!(human_bytes(3_000_000_000_000_000), "3000 TB");
    }

    #[test]
    fn durations() {
        assert_eq!(duration(42), "0:42");
        assert_eq!(duration(725), "12:05");
        assert_eq!(duration(3723), "1:02:03");
    }

    #[test]
    fn eta() {
        assert_eq!(eta_secs(1000, 300), Some(4));
        assert_eq!(eta_secs(1000, 0), None);
    }
}
