//! Human-friendly number formatting.

pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 7] = ["B", "KB", "MB", "GB", "TB", "PB", "EB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if v >= 100.0 {
        format!("{v:.0} {}", UNITS[u])
    } else if v >= 10.0 {
        format!("{v:.1} {}", UNITS[u])
    } else {
        format!("{v:.2} {}", UNITS[u])
    }
}

pub fn count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// "1 item", "2,048 items".
pub fn items(n: u64) -> String {
    if n == 1 {
        "1 item".into()
    } else {
        format!("{} items", count(n))
    }
}

pub fn percent(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "—".into();
    }
    let p = part as f64 * 100.0 / whole as f64;
    if p >= 10.0 {
        format!("{p:.0}%")
    } else if p >= 0.1 {
        format!("{p:.1}%")
    } else if part > 0 {
        "<0.1%".into()
    } else {
        "0%".into()
    }
}

pub fn duration(d: std::time::Duration) -> String {
    let s = d.as_secs_f64();
    if s < 1.0 {
        format!("{:.0} ms", s * 1000.0)
    } else if s < 60.0 {
        format!("{s:.1} s")
    } else {
        format!("{}m {:02}s", (s / 60.0) as u64, (s % 60.0) as u64)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn formats() {
        assert_eq!(super::bytes(512), "512 B");
        assert_eq!(super::bytes(1536), "1.50 KB");
        assert_eq!(super::bytes(200 * 1024 * 1024), "200 MB");
        assert_eq!(super::count(1234567), "1,234,567");
        assert_eq!(super::count(12), "12");
        assert_eq!(super::percent(1, 4), "25%");
    }
}
