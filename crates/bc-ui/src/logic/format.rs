//! Number/time formatting (port of `lib/format.ts`).

pub fn format_duration_ms(ms: Option<f64>) -> String {
    let Some(ms) = ms.filter(|m| m.is_finite() && *m >= 0.0) else { return "--:--".into() };
    let total = (ms / 1000.0).floor() as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

pub fn format_duration_s(s: f64) -> String {
    format_duration_ms(Some(s * 1000.0))
}

pub fn format_long_duration(ms: f64) -> String {
    let total = (ms / 1000.0).floor() as u64;
    let (d, h, m) = (total / 86400, (total % 86400) / 3600, (total % 3600) / 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

pub fn format_playtime(ms: f64) -> String {
    let total = (ms.max(0.0) / 1000.0).floor() as u64;
    let (d, h, m) = (total / 86400, (total % 86400) / 3600, (total % 3600) / 60);
    let mut parts = vec![];
    if d > 0 {
        parts.push(format!("{d}d"));
    }
    if d > 0 || h > 0 {
        parts.push(format!("{h}h"));
    }
    parts.push(format!("{m}m"));
    parts.join(" ")
}

pub fn format_bytes(bytes: f64) -> String {
    if bytes <= 0.0 {
        return "0 B".into();
    }
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let i = ((bytes.ln() / 1024f64.ln()).floor() as usize).min(U.len() - 1);
    let v = bytes / 1024f64.powi(i as i32);
    if v >= 100.0 || i == 0 { format!("{:.0} {}", v, U[i]) } else { format!("{:.1} {}", v, U[i]) }
}

/// 1234567 -> "1,234,567" (locale-independent so tests are stable).
pub fn format_count(n: i64) -> String {
    let s = n.abs().to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

pub fn format_bpm(bpm: Option<f64>) -> String {
    bpm.map(|b| if (b - b.round()).abs() < 0.05 { format!("{:.0}", b) } else { format!("{:.1}", b) }).unwrap_or_default()
}

/// Seconds-ago -> "3m", "2h", "5d", "3mo".
pub fn format_ago(secs: i64) -> String {
    let s = secs.max(0);
    match s {
        0..=59 => "now".into(),
        60..=3599 => format!("{}m", s / 60),
        3600..=86399 => format!("{}h", s / 3600),
        86400..=2_592_000 => format!("{}d", s / 86400),
        _ => format!("{}mo", s / 2_592_000),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn durations() {
        assert_eq!(format_duration_ms(Some(0.0)), "0:00");
        assert_eq!(format_duration_ms(None), "--:--");
        assert_eq!(format_duration_ms(Some(-5.0)), "--:--");
        assert_eq!(format_duration_ms(Some(61_000.0)), "1:01");
        assert_eq!(format_duration_ms(Some(3_661_000.0)), "1:01:01");
    }
    #[test]
    fn long_and_playtime() {
        assert_eq!(format_long_duration(90_000_000.0), "1d 1h");
        assert_eq!(format_long_duration(3_600_000.0 * 6.2), "6h 12m");
        assert_eq!(format_playtime(1_000.0 * (86400.0 * 17.0 + 3.0 * 3600.0 + 42.0 * 60.0)), "17d 3h 42m");
        assert_eq!(format_playtime(45.0 * 60_000.0), "45m");
    }
    #[test]
    fn bytes_and_counts() {
        assert_eq!(format_bytes(0.0), "0 B");
        assert_eq!(format_bytes(1536.0), "1.5 KB");
        assert_eq!(format_bytes(200.0 * 1024.0 * 1024.0), "200 MB");
        assert_eq!(format_count(1234567), "1,234,567");
        assert_eq!(format_count(12), "12");
        assert_eq!(format_bpm(Some(128.0)), "128");
        assert_eq!(format_bpm(Some(127.5)), "127.5");
        assert_eq!(format_ago(30), "now");
        assert_eq!(format_ago(7200), "2h");
    }
}
