use aswap::{
    display::{self, Row},
    identity,
    provider::Provider,
    usage::{Cached, parse_claude, parse_codex},
};
use serde_json::json;
fn fixture(compact: bool, width: usize, color: bool, stale: bool) -> String {
    let claude = identity::from_values(
        Provider::Claude,
        &json!({"claudeAiOauth":{"subscriptionType":"max"}}),
        &json!({"oauthAccount":{"emailAddress":"alice@example.com"}}),
    );
    let codex = identity::from_values(
        Provider::Codex,
        &json!({"email":"bob@example.com"}),
        &json!({}),
    );
    let unknown = identity::Identity::default();
    let claude_cache = Cached { usage: parse_claude(&json!({"five_hour":{"utilization":23.5,"resets_at":"1970-01-01T02:30:00Z"},"seven_day":{"utilization":99,"resets_at":"1970-01-03T00:00:00Z"}, "seven_day_opus":{"utilization":81,"resets_at":"1970-01-03T00:00:00Z"}})).unwrap(), fetched_at: 1000, ..Default::default() };
    let codex_cache = Cached { usage: parse_codex(&json!({"rateLimits":{"primary":{"usedPercent":10,"windowDurationMins":300,"resetsAt":9000}, "secondary":{"usedPercent":70,"windowDurationMins":10080,"resetsAt":172800}}})).unwrap(), fetched_at:1000, ..Default::default() };
    let empty = Cached::default();
    let duplicates = vec!["personal-copy".into()];
    display::render(
        &[
            Row {
                provider: "claude",
                label: "personal",
                active: true,
                identity: &claude,
                duplicates: &duplicates,
                cached: &claude_cache,
            },
            Row {
                provider: "codex",
                label: "work",
                active: false,
                identity: &codex,
                duplicates: &[],
                cached: &codex_cache,
            },
            Row {
                provider: "claude",
                label: "offline",
                active: false,
                identity: &unknown,
                duplicates: &[],
                cached: &empty,
            },
        ],
        compact,
        width,
        color,
        if stale { 9100 } else { 1000 },
    )
}
#[test]
fn rendering_snapshots() {
    for (name, compact, width, color, stale) in [
        ("status", false, 80, false, false),
        ("list", true, 80, false, false),
        ("narrow", false, 24, false, false),
        ("color", false, 80, true, false),
        ("stale", false, 80, false, true),
        ("stale-color", false, 80, true, true),
    ] {
        let rendered = fixture(compact, width, color, stale);
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("tests/snapshots/{name}.txt"));
        if std::env::var_os("ASWAP_UPDATE_SNAPSHOTS").is_some() {
            std::fs::write(&path, &rendered).unwrap();
        }
        assert_eq!(
            rendered,
            std::fs::read_to_string(path).unwrap(),
            "snapshot: {name}"
        );
        if !color {
            assert!(!rendered.contains('\x1b'));
            assert!(rendered.lines().all(|line| line.chars().count() <= width));
        }
    }
}
#[test]
fn very_narrow_terminals_and_control_characters() {
    for width in 1..80 {
        assert!(
            fixture(false, width, false, false)
                .lines()
                .all(|s| s.chars().count() <= width)
        );
    }
    let id = identity::from_values(
        Provider::Claude,
        &json!({}),
        &json!({"oauthAccount":{"emailAddress":"alice\u{1b}[31m@example.com\nforged"}}),
    );
    let c = Cached::default();
    let rendered = display::render(
        &[Row {
            provider: "claude",
            label: "safe",
            active: false,
            identity: &id,
            duplicates: &[],
            cached: &c,
        }],
        true,
        80,
        false,
        1000,
    );
    assert!(!rendered.contains('\x1b'));
    assert!(!rendered.contains("\nforged"));
}

#[test]
fn unavailable_quota_explains_reason_and_retains_dimmed_last_known_bar() {
    let id = identity::Identity::default();
    let cache = Cached {
        usage: parse_claude(&json!({"five_hour":{"utilization":80}})).unwrap(),
        error: Some("http-429".into()),
        http_status: Some(429),
        retry_after: Some(120),
        next_poll_at: 1120,
        fetched_at: 1000,
        ..Default::default()
    };
    let rendered = display::render(
        &[Row {
            provider: "claude",
            label: "synthetic",
            active: true,
            identity: &id,
            duplicates: &[],
            cached: &cache,
        }],
        false,
        80,
        true,
        1000,
    );
    assert!(rendered.contains("HTTP 429 rate-limited"));
    assert!(rendered.contains("Retry-After 120s"));
    assert!(rendered.contains("80.0% used (stale)"));
    assert!(rendered.contains("\x1b[2m"));
    assert!(!rendered.contains("????"));
}
