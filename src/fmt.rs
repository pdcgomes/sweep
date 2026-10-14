use std::time::Duration;

/// Decimal units, as Finder shows them.
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "kB", "MB", "GB", "TB", "PB"];
    if n < 1000 {
        return format!("{n} B");
    }
    let mut v = n as f64;
    let mut u = 0;
    while v >= 999.5 && u < UNITS.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    if v < 9.95 { format!("{v:.1} {}", UNITS[u]) } else { format!("{v:.0} {}", UNITS[u]) }
}

pub fn opt_bytes(n: Option<u64>) -> String {
    n.map(bytes).unwrap_or_else(|| "?".into())
}

/// "3 min", "5 h", "4 days", "3 months".
pub fn age(d: Duration) -> String {
    let s = d.as_secs();
    let plural = |n: u64, unit: &str| if n == 1 { format!("1 {unit}") } else { format!("{n} {unit}s") };
    match s {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min", s / 60),
        3600..=172_799 => format!("{} h", s / 3600),
        172_800..=5_183_999 => plural(s / 86_400, "day"),
        5_184_000..=63_071_999 => plural(s / 2_592_000, "month"),
        _ => plural(s / 31_536_000, "year"),
    }
}

pub fn ago(d: Duration) -> String {
    let a = age(d);
    if a == "just now" { a } else { format!("{a} ago") }
}

/// "22,882".
pub fn num(n: usize) -> String {
    let digits = n.to_string();
    let mut grouped = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(c);
    }
    grouped
}

pub fn count(n: usize, one: &str, many: &str) -> String {
    format!("{} {}", num(n), if n == 1 { one } else { many })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(999), "999 B");
        assert_eq!(bytes(1_500), "1.5 kB");
        assert_eq!(bytes(132_000_000_000), "132 GB");
        assert_eq!(bytes(2_340_000_000), "2.3 GB");
        assert_eq!(bytes(999_600_000), "1.0 GB");
    }

    #[test]
    fn ages() {
        assert_eq!(age(Duration::from_secs(30)), "just now");
        assert_eq!(age(Duration::from_secs(5 * 3600)), "5 h");
        assert_eq!(age(Duration::from_secs(47 * 3600)), "47 h");
        assert_eq!(age(Duration::from_secs(3 * 86_400)), "3 days");
        assert_eq!(age(Duration::from_secs(100 * 86_400)), "3 months");
        assert_eq!(ago(Duration::from_secs(90)), "1 min ago");
    }

    #[test]
    fn counts() {
        assert_eq!(count(1, "item", "items"), "1 item");
        assert_eq!(count(22_882, "item", "items"), "22,882 items");
    }
}
