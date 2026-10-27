use aswap::{
    Error, Result, Safe, now,
    provider::{self, Provider},
    runner,
    store::{self, Store},
    usage,
};
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};
use std::{
    io::{self, IsTerminal, Write},
    time::Duration,
};

#[derive(Parser)]
#[command(
    name = "aswap",
    version,
    about = "Private account profiles for Claude Code and Codex CLI"
)]
struct Cli {
    #[arg(
        long,
        global = true,
        help = "Emit structured JSON (run inherits provider output)"
    )]
    json: bool,
    #[command(subcommand)]
    command: Action,
}
#[derive(Subcommand)]
enum Action {
    /// Capture the current file-based login under a private label.
    Add { provider: Provider, label: String },
    /// List profiles, local identity hints and cached quota; no network requests.
    List { provider: Option<Provider> },
    /// Select the account used by subsequent aswap runs.
    Switch {
        provider: Provider,
        label: String,
        #[arg(
            long,
            help = "Also swap the provider's current default credential file; refuses symlinks"
        )]
        activate_live: bool,
    },
    /// Remove a stored profile (refuses a running account).
    Remove { provider: Provider, label: String },
    /// Rename a stored label, its usage cache and retained sessions.
    Rename {
        provider: Provider,
        old: String,
        new: String,
    },
    /// Read usage through the shared cache, respecting backoff.
    Usage {
        provider: Option<Provider>,
        /// Bypass cached quota/backoff for this explicit request.
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        account: Option<String>,
    },
    /// Configure or monitor automatic account selection.
    Auto {
        provider: Provider,
        #[arg(value_enum)]
        action: AutoAction,
        #[arg(help = "Percentage for threshold; text or off for resume-prompt")]
        value: Option<String>,
        #[arg(
            long,
            help = "Also activate the live default credential file when switching"
        )]
        activate_live: bool,
    },
    /// Launch with shared state and private account auth; arguments follow --.
    Run {
        provider: Provider,
        #[arg(long)]
        account: Option<String>,
        #[arg(
            long,
            help = "Supervise quota/limit switching, preserving options and resuming the exact session"
        )]
        auto: bool,
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Show all profiles and quota windows, refreshing through cache/backoff.
    Status { provider: Option<Provider> },
    /// Configure one refresh owner, or pull access-only profiles/cache over SSH.
    Sync {
        host: Option<String>,
        #[arg(long, conflicts_with_all=["host","serve","off"])]
        owner: bool,
        #[arg(long,conflicts_with_all=["host","serve","owner"])]
        off: bool,
        #[arg(long, hide = true, conflicts_with = "host")]
        serve: bool,
    },
    /// Stream Waybar custom/aswap JSON lines, at a relaxed cadence.
    Waybar {
        #[arg(long)]
        once: bool,
        #[arg(long, default_value_t=30, value_parser=clap::value_parser!(u64).range(30..=3600))]
        interval: u64,
    },
}
#[derive(Clone, Copy, ValueEnum)]
enum AutoAction {
    ResumePrompt,
    On,
    Off,
    Threshold,
    Once,
    Watch,
}
fn output(value: &Value, structured: bool, pretty: String) -> Result<()> {
    let text = if structured {
        serde_json::to_string_pretty(value).safe("cannot encode output")?
    } else {
        pretty
    };
    let mut out = io::stdout().lock();
    writeln!(out, "{text}").safe("cannot write output")
}
fn snapshot(store: &Store, filter: Option<Provider>, refresh: bool) -> Result<Value> {
    let (accounts, active, settings) = {
        let _lock = store.lock()?;
        let state = store.load()?;
        (state.accounts, state.active, state.auto)
    };
    let mut rows = Vec::new();
    for a in accounts
        .into_iter()
        .filter(|a| filter.is_none_or(|p| p == a.provider))
    {
        let c = if refresh {
            usage::cached(store, &a)?
        } else {
            let _lock = store.lock()?;
            let path = store
                .profile(a.provider, &a.label)?
                .join("usage-cache.json");
            if path.exists() {
                store::json_read(&path)?
            } else {
                usage::Cached::default()
            }
        };
        let duplicates = duplicate_labels(store, &a)?;
        rows.push(json!({"provider":a.provider,"label":a.label,"active":active.get(&a.provider)==Some(&a.label),"remaining_percent":c.used(now()).map(|u|100.0-u),"usage":c.usage,"fetched_at":c.fetched_at,"next_poll_at":c.next_poll_at,"duplicates":duplicates,"usage_error":c.diagnostic(),"http_status":c.http_status,"retry_after_seconds":c.retry_after,"last_attempt":c.last_attempt,"usage_status":if c.used(now()).is_some(){"ok"}else if c.error.is_some(){"backoff"}else{"unknown"}}));
    }
    let coordination = aswap::coordination::settings(store)?;
    Ok(
        json!({"schema_version":1,"accounts":rows,"auto":settings,"credential_mode":if coordination.owner {"refresh-owner"}else if coordination.host.is_some(){"access-only-follower"}else{"refresh-disabled"},"last_sync":coordination.last_sync}),
    )
}
fn duplicate_labels(store: &Store, account: &store::Account) -> Result<Vec<String>> {
    let _lock = store.lock()?;
    Ok(aswap::identity::duplicates(
        store,
        account,
        &store.load()?.accounts,
    ))
}
fn terminal_text(store: &Store, value: &Value, compact: bool) -> Result<String> {
    let accounts = value["accounts"].as_array().unwrap();
    let mut identities = Vec::new();
    let mut caches = Vec::new();
    let mut duplicates = Vec::new();
    for row in accounts {
        let provider: Provider =
            serde_json::from_value(row["provider"].clone()).safe("invalid provider")?;
        let _lock = store.lock()?;
        identities.push(
            aswap::identity::read(
                provider,
                &store.profile(provider, row["label"].as_str().unwrap())?,
            )
            .unwrap_or_default(),
        );
        caches.push(usage::Cached {
            usage: serde_json::from_value(row["usage"].clone()).safe("invalid usage")?,
            fetched_at: row["fetched_at"].as_u64().unwrap_or(0),
            error: row["usage_error"]["class"].as_str().map(|class| {
                match class {
                    "expired-or-invalid-token" => "http-401",
                    "forbidden" => "http-403",
                    "rate-limited" => "http-429",
                    other => other,
                }
                .to_owned()
            }),
            http_status: row["http_status"]
                .as_u64()
                .and_then(|n| u16::try_from(n).ok()),
            retry_after: row["retry_after_seconds"].as_u64(),
            next_poll_at: row["next_poll_at"].as_u64().unwrap_or(0),
            ..Default::default()
        });
        duplicates.push(
            serde_json::from_value::<Vec<String>>(row["duplicates"].clone()).unwrap_or_default(),
        );
    }
    let rows: Vec<_> = accounts
        .iter()
        .enumerate()
        .map(|(i, r)| aswap::display::Row {
            provider: r["provider"].as_str().unwrap(),
            label: r["label"].as_str().unwrap(),
            active: r["active"] == true,
            identity: &identities[i],
            cached: &caches[i],
            duplicates: &duplicates[i],
        })
        .collect();
    let mut size = std::mem::MaybeUninit::<libc::winsize>::uninit();
    // SAFETY: ioctl writes size on success; only then do we read it.
    let detected = unsafe {
        if libc::ioctl(1, libc::TIOCGWINSZ, size.as_mut_ptr()) == 0 {
            Some(size.assume_init().ws_col as usize)
        } else {
            None
        }
    };
    let width = detected
        .filter(|w| *w > 0)
        .or_else(|| std::env::var("COLUMNS").ok()?.parse().ok())
        .unwrap_or(80);
    Ok(aswap::display::render(
        &rows,
        compact,
        width,
        io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
        now(),
    ))
}
fn rows_text(value: &Value) -> String {
    let rows = value["accounts"].as_array().unwrap();
    if rows.is_empty() {
        return "No managed accounts. Use aswap add claude|codex LABEL.".into();
    }
    rows.iter()
        .map(|row| {
            let quota = row["remaining_percent"]
                .as_f64()
                .map(|p| format!("{p:.1}% remaining"))
                .unwrap_or("quota unknown".into());
            format!(
                "{} {} {} — {}",
                if row["active"] == true { "*" } else { " " },
                row["provider"].as_str().unwrap(),
                row["label"].as_str().unwrap(),
                quota
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}
fn waybar(value: &Value) -> Value {
    let text = value["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["active"] == true)
        .map(|a| {
            let remaining = a["remaining_percent"]
                .as_f64()
                .map(|p| format!("{p:.0}%"))
                .unwrap_or("?".into());
            format!(
                "{}:{} {}",
                a["provider"].as_str().unwrap(),
                a["label"].as_str().unwrap(),
                remaining
            )
        })
        .collect::<Vec<_>>()
        .join(" | ");
    json!({"text":if text.is_empty(){"aswap: no accounts"}else{&text},"tooltip":rows_text(value),"class":"aswap"})
}
fn execute(cli: Cli) -> Result<i32> {
    let store = Store::from_env()?;
    match cli.command {
        Action::Add { provider, label } => {
            let a = provider::add(&store, provider, &label)?;
            let duplicates = duplicate_labels(&store, &a)?;
            let warning = if duplicates.is_empty() {
                String::new()
            } else {
                format!(
                    "\nWarning: duplicate login of {}. add captures the current provider login.",
                    duplicates.join(", ")
                )
            };
            output(
                &json!({"added":a,"duplicates":duplicates}),
                cli.json,
                format!("Added {provider}:{label}{warning}"),
            )?;
        }
        Action::List { provider } => {
            let v = snapshot(&store, provider, false)?;
            let pretty = if cli.json {
                String::new()
            } else {
                terminal_text(&store, &v, true)?
            };
            output(&v, cli.json, pretty)?;
        }
        Action::Status { provider } => {
            let v = snapshot(&store, provider, true)?;
            let pretty = if cli.json {
                String::new()
            } else {
                terminal_text(&store, &v, false)?
            };
            output(&v, cli.json, pretty)?;
        }
        Action::Switch {
            provider,
            label,
            activate_live,
        } => {
            let a = provider::switch(&store, provider, &label, activate_live)?;
            output(
                &json!({"active":a,"live_activated":activate_live}),
                cli.json,
                format!("Selected {provider}:{label}"),
            )?;
        }
        Action::Remove { provider, label } => {
            let duplicates = provider::remove(&store, provider, &label)?;
            let detail = if duplicates.is_empty() {
                String::new()
            } else {
                format!(" (duplicate login of {})", duplicates.join(", "))
            };
            output(
                &json!({"removed":true,"provider":provider,"label":label,"duplicates":duplicates}),
                cli.json,
                format!("Removed {provider}:{label}{detail}"),
            )?;
        }
        Action::Rename { provider, old, new } => {
            let account = aswap::rename::rename(&store, provider, &old, &new)?;
            output(
                &json!({"renamed":account,"old_label":old}),
                cli.json,
                format!("Renamed {provider}:{old} to {new}"),
            )?;
        }
        Action::Usage {
            provider,
            account,
            refresh,
        } => {
            let v = if let Some(provider) = provider {
                let a = {
                    let _lock = store.lock()?;
                    store.account(&store.load()?, provider, account.as_deref())?
                };
                usage::cached_force(&store, &a, refresh)?;
                let mut v = snapshot(&store, Some(provider), false)?;
                v["accounts"]
                    .as_array_mut()
                    .unwrap()
                    .retain(|r| r["label"] == a.label);
                v
            } else {
                if account.is_some() {
                    return Err(Error("--account requires a provider"));
                }
                let accounts = {
                    let _lock = store.lock()?;
                    store.load()?.accounts
                };
                for a in accounts {
                    usage::cached_force(&store, &a, refresh)?;
                }
                snapshot(&store, None, false)?
            };
            let pretty = if cli.json {
                String::new()
            } else {
                terminal_text(&store, &v, false)?
            };
            output(&v, cli.json, pretty)?;
        }
        Action::Run {
            provider,
            account,
            args,
            auto,
        } => return runner::run(&store, provider, account.as_deref(), &args, auto),
        Action::Auto {
            provider,
            action,
            value,
            activate_live,
        } => {
            if matches!(action, AutoAction::Threshold | AutoAction::ResumePrompt) != value.is_some()
            {
                return Err(Error(
                    "threshold requires a percentage; resume-prompt requires text or off; other actions take no value",
                ));
            }
            match action {
                AutoAction::On
                | AutoAction::Off
                | AutoAction::Threshold
                | AutoAction::ResumePrompt => {
                    let _lock = store.lock()?;
                    let mut state = store.load()?;
                    let settings = state.auto.entry(provider).or_default();
                    match action {
                        AutoAction::On => settings.enabled = true,
                        AutoAction::Off => settings.enabled = false,
                        AutoAction::Threshold => {
                            let v = value
                                .as_deref()
                                .unwrap()
                                .parse::<f64>()
                                .safe("invalid threshold percentage")?;
                            store::threshold(v)?;
                            settings.threshold = v;
                        }
                        AutoAction::ResumePrompt => {
                            let prompt = value.unwrap();
                            if prompt.is_empty() || prompt.len() > 16384 || prompt.contains('\0') {
                                return Err(Error(
                                    "resume prompt must be 1–16384 bytes without NUL",
                                ));
                            }
                            settings.resume_prompt = (prompt != "off").then_some(prompt);
                        }
                        _ => unreachable!(),
                    }
                    let v = json!({"provider":provider,"auto":settings});
                    store.save(&state)?;
                    output(
                        &v,
                        cli.json,
                        format!(
                            "{provider} auto: {} (threshold {}%)",
                            v["auto"]["enabled"], v["auto"]["threshold"]
                        ),
                    )?;
                }
                AutoAction::Once | AutoAction::Watch => loop {
                    let selected = runner::tick(&store, provider, activate_live)?;
                    output(
                        &json!({"provider":provider,"selected":selected}),
                        cli.json,
                        selected
                            .map(|s| format!("Selected {provider}:{s}"))
                            .unwrap_or("No switch".into()),
                    )?;
                    if matches!(action, AutoAction::Once) {
                        break;
                    }
                    std::thread::sleep(Duration::from_secs(usage::TTL));
                },
            }
        }
        Action::Sync {
            host,
            owner,
            off,
            serve,
        } => {
            if serve {
                aswap::coordination::serve(&store)?;
            } else if off {
                aswap::coordination::off(&store)?;
                output(
                    &json!({"refresh_owner":false,"role":"refresh-disabled"}),
                    cli.json,
                    "Background OAuth refresh disabled; profiles retained".into(),
                )?;
            } else if owner {
                aswap::coordination::owner(&store)?;
                output(
                    &json!({"refresh_owner":true}),
                    cli.json,
                    "Configured this machine as refresh owner".into(),
                )?;
            } else {
                if host.is_none() && aswap::coordination::settings(&store)?.host.is_none() {
                    return Err(Error(
                        "no refresh owner configured; use sync SSH_ALIAS or sync --owner",
                    ));
                }
                let count = if let Some(host) = host {
                    aswap::coordination::configure(&store, &host)?
                } else {
                    aswap::coordination::poll(&store, None, true)?
                };
                output(
                    &json!({"synced":true,"profiles":count,"role":"follower"}),
                    cli.json,
                    format!("Synced {count} profiles from refresh owner (access-only credentials)"),
                )?;
            }
        }
        Action::Waybar { once, interval } => loop {
            let v = waybar(&snapshot(&store, None, true)?);
            let mut out = io::stdout().lock();
            if let Err(e) = writeln!(out, "{v}").and_then(|_| out.flush()) {
                if e.kind() == io::ErrorKind::BrokenPipe {
                    return Ok(0);
                }
                return Err(Error("cannot write Waybar stream"));
            }
            drop(out);
            if once {
                break;
            }
            std::thread::sleep(Duration::from_secs(interval));
        },
    }
    Ok(0)
}
fn main() {
    let cli = Cli::parse();
    let structured = cli.json;
    let code = match execute(cli) {
        Ok(code) => code,
        Err(err) => {
            if structured {
                eprintln!("{}", json!({"error":err.to_string()}));
            } else {
                eprintln!("aswap: {err}");
            }
            1
        }
    };
    std::process::exit(code);
}
