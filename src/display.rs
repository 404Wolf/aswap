//! Terminal-only rendering: identity hints never enter JSON, Waybar or caches.
use crate::{identity::Identity, usage::Cached};
pub struct Row<'a> {
    pub provider: &'a str,
    pub label: &'a str,
    pub active: bool,
    pub identity: &'a Identity,
    pub duplicates: &'a [String],
    pub cached: &'a Cached,
}
fn clean(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}
fn reset(at: Option<u64>, now: u64) -> String {
    match at {
        None => "reset unknown".into(),
        Some(at) if at <= now => "reset passed; refresh needed".into(),
        Some(at) => {
            let minutes = (at - now).div_ceil(60);
            if minutes >= 1440 {
                format!("resets in {}d{}h", minutes / 1440, minutes % 1440 / 60)
            } else {
                format!("resets in {}h{}m", minutes / 60, minutes % 60)
            }
        }
    }
}
fn window_name(name: &str, duration: Option<u64>) -> String {
    match (name, duration) {
        ("five_hour", _) | (_, Some(300)) => "SESSION (5h)".into(),
        ("seven_day", _) => "WEEKLY".into(),
        ("seven_day_opus", _) => "WEEKLY OPUS".into(),
        ("seven_day_sonnet", _) => "WEEKLY SONNET".into(),
        (_, Some(10080)) => "WEEKLY".into(),
        _ => clean(name).to_uppercase(),
    }
}
fn wrap(line: &str, width: usize) -> Vec<String> {
    if line.is_empty() {
        return vec![String::new()];
    }
    let chars: Vec<_> = line.chars().collect();
    chars
        .chunks(width.max(1))
        .map(|c| c.iter().collect::<String>().trim_end().to_owned())
        .collect()
}
pub fn render(rows: &[Row<'_>], compact: bool, width: usize, color: bool, now: u64) -> String {
    let width = width.max(1);
    if rows.is_empty() {
        return wrap(
            "No managed accounts. Use aswap add claude|codex LABEL.",
            width,
        )
        .join("\n");
    }
    let mut lines = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        if index > 0 && !compact {
            lines.push(String::new());
        }
        let identity = format!(
            "{} · {}",
            row.identity.email.as_deref().unwrap_or("email unknown"),
            row.identity.plan.as_deref().unwrap_or("plan unknown")
        );
        let heading = format!(
            "{} {}:{}{}",
            if row.active { "*" } else { " " },
            row.provider,
            row.label,
            if row.active { " [active]" } else { "" }
        );
        lines.extend(wrap(&clean(&format!("{heading}  {identity}")), width));
        if !row.duplicates.is_empty() {
            lines.extend(wrap(
                &format!("  ! duplicate login: {}", clean(&row.duplicates.join(", "))),
                width,
            ));
        }
        let c = row.cached;
        let state = if c.error.is_some() {
            "unavailable (backoff)"
        } else if c.used(now).is_none() {
            "stale/unknown"
        } else {
            "cached"
        };
        let age = if c.fetched_at == 0 {
            "never fetched".into()
        } else {
            format!("observed {}m ago", now.saturating_sub(c.fetched_at) / 60)
        };
        lines.extend(wrap(&format!("  quota: {state} · {age}"), width));
        for w in &c.usage.windows {
            let name = window_name(&w.name, w.duration_minutes);
            let valid = c.error.is_none()
                && now.saturating_sub(c.fetched_at) <= 600
                && w.resets_at.is_none_or(|t| t > now);
            let percent = if valid {
                format!("{:.1}% used", w.used_percent)
            } else {
                format!("{:.1}% last observed ({state})", w.used_percent)
            };
            let detail = format!("  {name} {percent} · {}", reset(w.resets_at, now));
            lines.extend(wrap(&detail, width));
            if !compact && width >= 5 {
                let size = width.saturating_sub(4).clamp(1, 24);
                let filled = if valid {
                    (w.used_percent.clamp(0.0, 100.0) / 100.0 * size as f64).round() as usize
                } else {
                    0
                };
                let bar = if valid {
                    format!("{}{}", "━".repeat(filled), "░".repeat(size - filled))
                } else {
                    "?".repeat(size)
                };
                let bar = format!("[{bar}]");
                let bar = if color && valid {
                    format!(
                        "\x1b[{}m{bar}\x1b[0m",
                        if w.used_percent >= 90.0 {
                            31
                        } else if w.used_percent >= 70.0 {
                            33
                        } else {
                            32
                        }
                    )
                } else {
                    bar
                };
                lines.push(format!("  {bar}"));
            }
        }
        for (name, duration) in [("SESSION (5h)", 300), ("WEEKLY", 10080)] {
            if !c
                .usage
                .windows
                .iter()
                .any(|w| w.duration_minutes == Some(duration))
            {
                lines.extend(wrap(&format!("  {name} unknown · reset unknown"), width));
            }
        }
    }
    lines.join("\n")
}
