//! Terminal-only rendering: identity hints never enter JSON, Waybar or caches.
use crate::{
    identity::Identity,
    store::Prefer,
    usage::{Cached, Window},
};
pub struct Row<'a> {
    pub provider: &'a str,
    pub label: &'a str,
    pub active: bool,
    pub identity: &'a Identity,
    pub duplicates: &'a [String],
    pub cached: &'a Cached,
}
/// The provider-wide auto policy shown above its accounts.
pub struct Policy<'a> {
    pub provider: &'a str,
    pub enabled: bool,
    pub threshold: f64,
    pub prefer: Prefer,
}
/// Observations older than this are shown dimmed with their age.
const FRESH: u64 = 600;
/// Errored observations older than this collapse to a one-line summary.
const ANCIENT: u64 = 86_400;

fn clean(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}
fn credit_balance(balance: Option<&str>) -> String {
    let Some(value) = balance
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|n| n.is_finite() && *n >= 0.0)
    else {
        return "unknown".into();
    };
    let rounded = format!("{:.0}", value.round());
    let mut grouped = String::new();
    for (i, c) in rounded.chars().enumerate() {
        if i > 0 && (rounded.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(c);
    }
    grouped
}
/// A short human duration: 45s, 12m, 3h49m, 6d21h.
pub fn duration(seconds: u64) -> String {
    let minutes = seconds.div_ceil(60);
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{minutes}m"),
        3600..86_400 => format!("{}h{:02}m", minutes / 60, minutes % 60),
        _ => format!("{}d{}h", minutes / 1440, minutes % 1440 / 60),
    }
}
fn ago(at: u64, now: u64) -> String {
    match now.saturating_sub(at) {
        0..60 => "just now".into(),
        s => format!("{} ago", duration(s)),
    }
}
fn reset(w: &Window, now: u64) -> String {
    match w.resets_at {
        // Claude reports no reset for a window nobody has started yet.
        None if w.used_percent == 0.0 => "not started".into(),
        None => "reset time unknown".into(),
        Some(at) if at <= now => "reset passed, refreshing".into(),
        Some(at) => format!("resets in {}", duration(at - now)),
    }
}
fn window_name(name: &str, duration: Option<u64>) -> String {
    match (name, duration) {
        ("five_hour", _) | (_, Some(300)) => "session".into(),
        ("seven_day", _) => "weekly".into(),
        ("seven_day_opus", _) => "weekly opus".into(),
        ("seven_day_sonnet", _) => "weekly sonnet".into(),
        (_, Some(10080)) => "weekly".into(),
        ("primary" | "secondary", None) => "quota".into(),
        (_, Some(n)) if n % 1440 == 0 => format!("{}d window", n / 1440),
        (_, Some(n)) if n % 60 == 0 => format!("{}h window", n / 60),
        (_, Some(n)) => format!("{n}m window"),
        _ => clean(name).to_lowercase(),
    }
}
fn percent(p: f64) -> String {
    if p > 0.0 && p < 1.0 {
        "<1%".into()
    } else {
        format!("{:.0}%", p.clamp(0.0, 100.0).floor())
    }
}
/// What went wrong, and what to do about it when the user must act.
fn problem(row: &Row<'_>, now: u64) -> Option<(String, Option<String>)> {
    let c = row.cached;
    let reason = c.diagnostic()?;
    let class = reason["class"].as_str().unwrap_or("usage-unavailable");
    let detail = match reason["http_status"].as_u64() {
        Some(n) => format!("HTTP {n} {class}"),
        None => class.to_owned(),
    };
    let retry = match c.next_poll_at.saturating_sub(now) {
        0 => "retrying now".into(),
        s => format!("retrying in {}", duration(s)),
    };
    let after = c
        .retry_after
        .map(|s| format!(", Retry-After {s}s"))
        .unwrap_or_default();
    let readd = format!(
        "sign in to {} again, then: aswap remove {p} {l} && aswap add {p} {l}",
        clean(row.provider),
        p = clean(row.provider),
        l = clean(row.label)
    );
    Some(match class {
        "refresh-invalid-grant" | "expired-or-invalid-token" => {
            (format!("login expired ({detail})"), Some(readd))
        }
        "forbidden" => (format!("usage forbidden ({detail}) · {retry}"), None),
        "rate-limited" => (
            format!("usage API rate-limited ({detail}{after}) · {retry}"),
            None,
        ),
        "network" => (format!("network error · {retry}"), None),
        "sync-unavailable" => (
            "refresh owner unreachable over SSH; showing the last synced data".into(),
            None,
        ),
        "owner-profile-missing" => (
            "the refresh owner has no profile with this login".into(),
            Some(format!(
                "on the owner: aswap add {} <label>",
                clean(row.provider)
            )),
        ),
        "refresh-owner-required" => (
            "no machine refreshes logins".into(),
            Some("on one machine: aswap sync --owner".into()),
        ),
        "provider-limit" => (format!("hit the provider usage limit · {retry}"), None),
        _ => (
            format!("usage check failed ({detail}{after}) · {retry}"),
            None,
        ),
    })
}
/// Word-wraps at spaces, keeping the line's indent on continuations; words
/// longer than the width are split.
fn wrap(line: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    if line.chars().count() <= width {
        return vec![line.trim_end().to_owned()];
    }
    let indent: String = line.chars().take_while(|c| *c == ' ').collect();
    let indent = if indent.len() * 2 < width {
        indent
    } else {
        String::new()
    };
    let mut out = Vec::new();
    let mut current = String::new();
    for word in line.split_whitespace() {
        let candidate = if current.is_empty() {
            format!("{indent}{word}")
        } else {
            format!("{current} {word}")
        };
        if candidate.chars().count() <= width {
            current = candidate;
            continue;
        }
        if !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
        let mut rest: Vec<char> = format!("{indent}{word}").chars().collect();
        while rest.len() > width {
            out.push(
                rest[..width]
                    .iter()
                    .collect::<String>()
                    .trim_end()
                    .to_owned(),
            );
            rest = rest[width..].to_vec();
        }
        current = rest.into_iter().collect();
    }
    if !current.trim().is_empty() {
        out.push(current);
    }
    out
}
struct Paint(bool);
impl Paint {
    fn wrap(&self, code: &str, s: &str) -> String {
        if self.0 && !s.is_empty() && !code.is_empty() {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_owned()
        }
    }
    /// Wraps plain text to the width first, then colors each piece, so escapes
    /// never count against the width.
    fn lines(&self, code: &str, plain: &str, width: usize) -> Vec<String> {
        wrap(plain, width)
            .into_iter()
            .map(|l| self.wrap(code, &l))
            .collect()
    }
}
fn level(p: f64) -> &'static str {
    if p >= 90.0 {
        "31"
    } else if p >= 70.0 {
        "33"
    } else {
        "32"
    }
}
fn credits_line(c: &Cached) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(credits) = &c.usage.credits {
        parts.push(if credits.unlimited == Some(true) {
            "unlimited credits".into()
        } else {
            format!("{} credits", credit_balance(credits.balance.as_deref()))
        });
        match credits.automatic_reload {
            Some(true) => parts.push("auto-reload on".into()),
            Some(false) => parts.push("auto-reload off".into()),
            None => {}
        }
    }
    match c.usage.available_usage_resets {
        Some(1) => parts.push("1 limit reset available".into()),
        Some(n) if n > 1 => parts.push(format!("{n} limit resets available")),
        _ => {}
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}
pub fn policy_text(p: &Policy<'_>) -> String {
    let prefer = match p.prefer {
        Prefer::Headroom => "most headroom",
        Prefer::WeeklyReset => "soonest weekly reset",
    };
    format!(
        "auto-select {} · switch at {}% used · prefer {prefer}",
        if p.enabled { "on" } else { "off" },
        p.threshold
    )
}

pub fn render(rows: &[Row<'_>], compact: bool, width: usize, color: bool, now: u64) -> String {
    render_with(rows, &[], compact, width, color, now)
}
pub fn render_with(
    rows: &[Row<'_>],
    policies: &[Policy<'_>],
    compact: bool,
    width: usize,
    color: bool,
    now: u64,
) -> String {
    let width = width.max(1);
    let paint = Paint(color);
    if rows.is_empty() {
        return wrap(
            "No managed accounts. Use aswap add claude|codex LABEL.",
            width,
        )
        .join("\n");
    }
    let mut providers: Vec<&str> = Vec::new();
    for row in rows {
        if !providers.contains(&row.provider) {
            providers.push(row.provider);
        }
    }
    // One window-name column across every account, so bars line up.
    let name_width = rows
        .iter()
        .flat_map(|r| &r.cached.usage.windows)
        .map(|w| window_name(&w.name, w.duration_minutes).chars().count())
        .max()
        .unwrap_or(0);
    let mut lines = Vec::new();
    for (pi, provider) in providers.iter().enumerate() {
        if pi > 0 && !compact {
            lines.push(String::new());
        }
        let group: Vec<_> = rows.iter().filter(|r| r.provider == *provider).collect();
        let mut header = paint.lines("1", &clean(provider), width);
        if let Some(p) = policies.iter().find(|p| p.provider == *provider) {
            let text = format!("{}  {}", clean(provider), policy_text(p));
            header = if text.chars().count() <= width {
                vec![format!(
                    "{}  {}",
                    paint.wrap("1", &clean(provider)),
                    paint.wrap("2", &policy_text(p))
                )]
            } else {
                paint.lines("1", &text, width)
            };
        }
        lines.extend(header);
        let selected = group.iter().find(|r| r.active).map(|r| clean(r.label));
        let selection = match selected {
            Some(label) => format!("  Default: {label} · running agents may use other accounts"),
            None => "  No default account selected".into(),
        };
        lines.extend(paint.lines("2", &selection, width));
        let label_width = group
            .iter()
            .map(|r| clean(r.label).chars().count())
            .max()
            .unwrap_or(0);
        for row in group {
            render_row(
                &mut lines,
                row,
                (label_width, name_width),
                compact,
                width,
                &paint,
                now,
            );
        }
    }
    lines.join("\n")
}
fn render_row(
    lines: &mut Vec<String>,
    row: &Row<'_>,
    (label_width, name_width): (usize, usize),
    compact: bool,
    width: usize,
    paint: &Paint,
    now: u64,
) {
    let c = row.cached;
    let age = now.saturating_sub(c.fetched_at);
    let errored = c.error.is_some();
    let collapsed = c.fetched_at == 0 || (errored && age > ANCIENT);
    let marker = if row.active { "●" } else { "○" };
    let label = format!("{:<label_width$}", clean(row.label));
    let mut facts = Vec::new();
    match (&row.identity.email, &row.identity.plan) {
        (Some(e), Some(p)) => facts.push(format!("{} · {}", clean(e), clean(p))),
        (Some(e), None) => facts.push(clean(e)),
        (None, Some(p)) => facts.push(clean(p)),
        (None, None) => {}
    }
    if !row.duplicates.is_empty() {
        facts.push(format!(
            "same login as {}",
            clean(&row.duplicates.join(", "))
        ));
    }
    if compact && let Some(credits) = credits_line(c) {
        facts.push(credits);
    }
    if c.fetched_at == 0 {
        facts.push("never fetched".into());
    } else {
        facts.push(format!("checked {}", ago(c.fetched_at, now)));
        if errored || age > FRESH || c.usage.used(now).is_none() {
            facts.push("last known quota; availability unknown".into());
        }
    }
    let summary = if compact && !collapsed {
        window_summary(c, now)
    } else {
        String::new()
    };
    let facts = facts.join(" · ");
    // List rows lead with quota; identity facts follow when they fit, else
    // they get their own line.
    let (lead, trail) = match (summary.is_empty(), facts.is_empty()) {
        (true, _) => (facts.clone(), String::new()),
        (false, true) => (summary.clone(), String::new()),
        (false, false)
            if format!("{marker} {label}  {summary} · {facts}")
                .chars()
                .count()
                <= width =>
        {
            (format!("{summary} · {facts}"), String::new())
        }
        (false, false) => (summary.clone(), facts.clone()),
    };
    let head_plain = format!("{marker} {label}  {lead}");
    if head_plain.chars().count() <= width {
        let styled_label = if row.active {
            paint.wrap("1;32", &label)
        } else {
            paint.wrap("1", &label)
        };
        let styled_marker = if row.active {
            paint.wrap("32", marker)
        } else {
            paint.wrap("2", marker)
        };
        let styled_lead = if summary.is_empty() {
            paint.wrap("2", &lead)
        } else {
            lead.clone()
        };
        lines.push(
            format!("{styled_marker} {styled_label}  {styled_lead}")
                .trim_end()
                .to_owned(),
        );
    } else {
        // Too long: the label stands alone and everything else moves below.
        let label_line = format!("{marker} {}", label.trim_end());
        lines.extend(paint.lines(if row.active { "1;32" } else { "1" }, &label_line, width));
        lines.extend(paint.lines(
            if summary.is_empty() { "2" } else { "" },
            &format!("    {lead}"),
            width,
        ));
    }
    if !trail.is_empty() {
        lines.extend(paint.lines("2", &format!("    {trail}"), width));
    }
    if let Some((problem, hint)) = problem(row, now) {
        lines.extend(paint.lines("31", &format!("    ! {problem}"), width));
        if let Some(hint) = hint {
            lines.extend(paint.lines("2", &format!("      {hint}"), width));
        }
    }
    if compact {
        return;
    }
    if collapsed {
        if c.fetched_at != 0 && !c.usage.windows.is_empty() {
            lines.extend(paint.lines(
                "2",
                &format!("    last known: {}", window_summary(c, now)),
                width,
            ));
        }
        return;
    }
    let windows: Vec<_> = c.usage.windows.iter().collect();
    if windows.is_empty() {
        lines.extend(paint.lines("2", "    quota unknown", width));
    }
    for w in windows {
        let name = format!("{:<name_width$}", window_name(&w.name, w.duration_minutes));
        let pct = format!("{:>4} used", percent(w.used_percent));
        let when = reset(w, now);
        let limit = if w.used_percent >= 100.0 && w.resets_at.is_none_or(|r| r > now) {
            "  limit reached"
        } else {
            ""
        };
        let valid = !errored && age <= FRESH && w.resets_at.is_none_or(|t| t > now);
        // indent + name + bar + percent + reset, with two-space gutters.
        // A fixed reset column keeps every bar the same length.
        let fixed = 4 + name_width + 2 + 1 + 9 + 2 + "resets in 10d23h".len();
        let size = width.saturating_sub(fixed).min(24);
        let plain_bar = if size >= 6 {
            let filled = (w.used_percent.clamp(0.0, 100.0) / 100.0 * size as f64).round() as usize;
            format!("{}{} ", "█".repeat(filled), "░".repeat(size - filled))
        } else {
            String::new()
        };
        let mut plain = format!("    {name}  {plain_bar}{pct}  {when}{limit}");
        let limit = if plain.chars().count() > width && !limit.is_empty() {
            plain = format!("    {name}  {plain_bar}{pct}  {when}");
            ""
        } else {
            limit
        };
        if plain.chars().count() > width {
            lines.extend(paint.lines(if valid { "" } else { "2" }, &plain, width));
            continue;
        }
        let code = if valid { level(w.used_percent) } else { "2" };
        lines.push(format!(
            "    {name}  {}{}  {}{}",
            paint.wrap(code, &plain_bar),
            paint.wrap(code, &pct),
            paint.wrap("2", &when),
            paint.wrap("31", limit)
        ));
    }
    if let Some(credits) = credits_line(c) {
        lines.extend(paint.lines("2", &format!("    {credits}"), width));
    }
}
/// One-line window summary for list rows and long-broken logins.
fn window_summary(c: &Cached, now: u64) -> String {
    let parts: Vec<String> = c
        .usage
        .windows
        .iter()
        .map(|w| {
            let mut s = format!(
                "{} {} used",
                window_name(&w.name, w.duration_minutes),
                percent(w.used_percent)
            );
            if w.duration_minutes == Some(10080)
                && let Some(at) = w.resets_at.filter(|at| *at > now)
            {
                s.push_str(&format!(" (resets in {})", duration(at - now)));
            }
            s
        })
        .collect();
    parts.join(" · ")
}
