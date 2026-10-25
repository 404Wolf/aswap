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
    io::{self, Write},
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
    /// List labels without reading credentials or making network requests.
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
    /// Read usage through the shared cache, respecting backoff.
    Usage {
        provider: Provider,
        #[arg(long)]
        account: Option<String>,
    },
    /// Configure or monitor automatic account selection.
    Auto {
        provider: Provider,
        #[arg(value_enum)]
        action: AutoAction,
        #[arg(help = "Percentage, required with threshold")]
        value: Option<f64>,
        #[arg(
            long,
            help = "Also activate the live default credential file when switching"
        )]
        activate_live: bool,
    },
    /// Launch an isolated provider session; arguments follow --.
    Run {
        provider: Provider,
        #[arg(long)]
        account: Option<String>,
        #[arg(
            long,
            help = "Supervise an interactive session and resume on quota/limit switches; accepts no provider arguments"
        )]
        auto: bool,
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Show active labels and cached quota; makes no network requests.
    Status { provider: Option<Provider> },
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
        rows.push(json!({"provider":a.provider,"label":a.label,"active":active.get(&a.provider)==Some(&a.label),"remaining_percent":c.used(now()).map(|u|100.0-u),"usage":c.usage,"fetched_at":c.fetched_at,"next_poll_at":c.next_poll_at,"usage_status":if c.used(now()).is_some(){"ok"}else if c.error.is_some(){"backoff"}else{"unknown"}}));
    }
    Ok(json!({"schema_version":1,"accounts":rows,"auto":settings}))
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
            output(
                &json!({"added":a}),
                cli.json,
                format!("Added {provider}:{label}"),
            )?;
        }
        Action::List { provider } | Action::Status { provider } => {
            let v = snapshot(&store, provider, false)?;
            output(&v, cli.json, rows_text(&v))?;
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
            provider::remove(&store, provider, &label)?;
            output(
                &json!({"removed":true}),
                cli.json,
                format!("Removed {provider}:{label}"),
            )?;
        }
        Action::Usage { provider, account } => {
            let a = {
                let _lock = store.lock()?;
                store.account(&store.load()?, provider, account.as_deref())?
            };
            usage::cached(&store, &a)?;
            let mut v = snapshot(&store, Some(provider), false)?;
            v["accounts"]
                .as_array_mut()
                .unwrap()
                .retain(|r| r["label"] == a.label);
            output(&v, cli.json, rows_text(&v))?;
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
            if matches!(action, AutoAction::Threshold) != value.is_some() {
                return Err(Error("only threshold takes a percentage argument"));
            }
            match action {
                AutoAction::On | AutoAction::Off | AutoAction::Threshold => {
                    let _lock = store.lock()?;
                    let mut state = store.load()?;
                    let settings = state.auto.entry(provider).or_default();
                    match action {
                        AutoAction::On => settings.enabled = true,
                        AutoAction::Off => settings.enabled = false,
                        AutoAction::Threshold => {
                            let v = value.unwrap();
                            store::threshold(v)?;
                            settings.threshold = v;
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
