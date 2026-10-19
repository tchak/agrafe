//! The template filters: timestamps, sizes, ages, the head of an error.
//! In scope as `filters` for every template of the crate.

use std::fmt::Display;

use jiff::Timestamp;

/// `2026-10-19 12:33:03Z`: readable, sortable, no local time zone.
#[askama::filter_fn]
pub fn ts(value: &Timestamp, _: &dyn askama::Values) -> askama::Result<String> {
    Ok(format_ts(value))
}

/// `ts`, empty for `None`.
#[askama::filter_fn]
pub fn opt_ts(value: &Option<Timestamp>, _: &dyn askama::Values) -> askama::Result<String> {
    Ok(value.as_ref().map(format_ts).unwrap_or_default())
}

fn format_ts(value: &Timestamp) -> String {
    value.strftime("%Y-%m-%d %H:%M:%SZ").to_string()
}

/// Bytes with a binary unit, one decimal: `1.5 MiB`.
#[askama::filter_fn]
pub fn size(value: &Option<i64>, _: &dyn askama::Values) -> askama::Result<String> {
    Ok(value.map(format_size).unwrap_or_default())
}

pub fn format_size(bytes: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// A duration in seconds as `3m 12s`, `2h 05m`, `3d 4h`.
#[askama::filter_fn]
pub fn age(value: &Option<f64>, _: &dyn askama::Values) -> askama::Result<String> {
    Ok(value.map(format_age).unwrap_or_default())
}

pub fn format_age(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else if s < 86_400 {
        format!("{}h {:02}m", s / 3600, (s % 3600) / 60)
    } else {
        format!("{}d {}h", s / 86_400, (s % 86_400) / 3600)
    }
}

/// The first line of a message, for a listing; the detail shows it whole.
#[askama::filter_fn]
pub fn first_line(value: &Option<String>, _: &dyn askama::Values) -> askama::Result<String> {
    Ok(value
        .as_deref()
        .and_then(|s| s.lines().next())
        .unwrap_or_default()
        .to_string())
}

/// A state as a coloured badge: the colour says at a glance whether the
/// thing is waiting, under way, done or wrong. The value is escaped here,
/// since the template takes the result as `safe`.
#[askama::filter_fn]
pub fn badge(value: &dyn Display, _: &dyn askama::Values) -> askama::Result<String> {
    let state = askama::filters::escape(value, askama::filters::Html)?.to_string();
    Ok(badge_html(&state))
}

/// The markup of a badge around `state`, already escaped.
fn badge_html(state: &str) -> String {
    let colour = match state {
        "ready" | "clean" => "badge-success",
        "due" | "uploaded" => "badge-info",
        "running" | "uploading" | "created" => "badge-primary",
        "scheduled" | "pending" => "badge-warning",
        "dead" | "infected" | "failed" => "badge-error",
        _ => "badge-ghost",
    };
    format!(r#"<span class="badge badge-soft badge-sm {colour}">{state}</span>"#)
}

/// `None` as nothing, `Some` as itself.
#[askama::filter_fn]
pub fn opt<T: Display>(value: &Option<T>, _: &dyn askama::Values) -> askama::Result<String> {
    Ok(value.as_ref().map(ToString::to_string).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_and_ages_read_well() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1536), "1.5 KiB");
        assert_eq!(format_size(5 << 30), "5.0 GiB");
        assert_eq!(format_age(-3.0), "0s");
        assert_eq!(format_age(59.9), "59s");
        assert_eq!(format_age(192.0), "3m 12s");
        assert_eq!(format_age(7500.0), "2h 05m");
        assert_eq!(format_age(100_000.0), "1d 3h");
    }

    #[test]
    fn a_badge_escapes_its_value() {
        let html = askama::filters::escape("<b>", askama::filters::Html)
            .map(|s| badge_html(&s.to_string()))
            .unwrap();
        assert!(html.contains(">&#60;b&#62;<"), "{html}");
        assert!(html.contains("badge-ghost"), "{html}");
    }

    #[test]
    fn timestamps_are_utc_to_the_second() {
        let t: Timestamp = "2026-10-19T12:33:03.123Z".parse().unwrap();
        assert_eq!(format_ts(&t), "2026-10-19 12:33:03Z");
    }
}
