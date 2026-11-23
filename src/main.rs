use aswap::{
    Error, Result, Safe, now,
    provider::{self, Provider},
    runner,
    store::{self, Store, StoreView},
    usage,
};
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{self, IsTerminal, Write},
    time::Duration,
};

#[derive(Parser)]
#[command(
    name = "aswap",
    version,
    about = "Private account profiles for Claude Code, Codex CLI, and Antigravity CLI",
    after_long_help = "To reuse a separate Antigravity store, set [stores.agy] in the aswap config.toml. Set root to the absolute account store path, including its final /aswap component when migrating a standalone store. Set private_root to its private state directory (defaults to ~/.local/state/agy-private). ASWAP_AGY_HOME and ASWAP_AGY_PRIVATE_ROOT override those paths. Optional program pins the absolute native agy binary for that store."
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
    /// Preview retention of finished runtime views.
    Gc {
        /// Remove eligible views and preserve their transcripts.
        #[arg(long, conflicts_with = "dry_run")]
        apply: bool,
        /// Preview even if the configuration enables automatic application.
        #[arg(long)]
        dry_run: bool,
        /// Verify an old label's original home (provider:label=/absolute/path).
        #[arg(long, value_name = "PROVIDER:LABEL=PATH")]
        legacy_home: Vec<String>,
    },
    /// Show the actual account used by each locally running session.
    Sessions { provider: Option<Provider> },
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
    /// Install a backed-up live login (live-backups) as an existing
    /// account's login; it must be the same account.
    Restore {
        provider: Provider,
        label: String,
        /// The backup directory aswap printed when it saved the login.
        #[arg(long)]
        from: std::path::PathBuf,
        /// Replace the account's login even if it changed since the backup
        /// or has a pending rotation (both are backed up first).
        #[arg(long)]
        force: bool,
    },
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
    #[command(args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
    Auto {
        #[command(subcommand)]
        global: Option<AutoGlobal>,
        #[arg(required = true)]
        provider: Option<Provider>,
        #[arg(value_enum, required = true)]
        action: Option<AutoAction>,
        #[arg(
            help = "Percentage for threshold; text or off for resume-prompt; headroom or weekly-reset for prefer; off, dry-run or on for reset (Codex only); account label for reserve or unreserve"
        )]
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
        /// With --serve: stream a packet now and after every auth change.
        #[arg(long, hide = true, requires = "serve")]
        watch: bool,
    },
    /// Stream Waybar custom/aswap JSON lines, at a relaxed cadence.
    Waybar {
        #[arg(long)]
        once: bool,
        #[arg(long, default_value_t=30, value_parser=clap::value_parser!(u64).range(30..=3600))]
        interval: u64,
    },
}
/// Settings shared by every provider, given without one.
#[derive(Subcommand)]
enum AutoGlobal {
    /// Show or set the minimum seconds between account switches of all
    /// supervised sessions on this machine (every provider); 0 turns it off.
    SwitchInterval {
        #[arg(value_parser = clap::value_parser!(u64).range(0..=store::MAX_SWITCH_INTERVAL))]
        seconds: Option<u64>,
    },
}
#[derive(Clone, Copy, ValueEnum)]
enum AutoAction {
    ResumePrompt,
    On,
    Off,
    Threshold,
    Prefer,
    /// Codex: redeem a usage-limit reset credit when every account is out of
    /// weekly usage (off, dry-run or on; default off).
    Reset,
    /// Keep this account available for runs pinned to it.
    Reserve,
    /// Make this account available to automatic selection again.
    Unreserve,
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
struct SnapshotRow<'a> {
    store: &'a Store,
    account: store::Account,
    cached: usage::Cached,
    duplicates: Vec<String>,
    active: bool,
    reserved: bool,
}

impl SnapshotRow<'_> {
    fn json(&self, at: u64) -> Value {
        let Self {
            account,
            cached: c,
            duplicates,
            active,
            reserved,
            ..
        } = self;
        let used = c.used(at);
        // Agy observations are per family. Keep their individual windows
        // without publishing a percentage for the whole account.
        let remaining_percent = if account.provider == Provider::Agy {
            None
        } else {
            used.map(|u| 100.0 - u)
        };
        let usage_status = if used.is_some() {
            "ok"
        } else if c.error.is_some() {
            "backoff"
        } else {
            "unknown"
        };
        json!({
            "provider":account.provider,"label":account.label,"active":active,"reserved":reserved,
            "remaining_percent":remaining_percent,"usage":c.usage,"fetched_at":c.fetched_at,
            "next_poll_at":c.next_poll_at,"duplicates":duplicates,"usage_error":c.diagnostic(),
            "http_status":c.http_status,"retry_after_seconds":c.retry_after,"last_attempt":c.last_attempt,
            "usage_status":usage_status,"usage_reason":c.reason(at),"age_seconds":c.age(at)
        })
    }
}

struct Snapshot<'a> {
    metadata: &'a Store,
    rows: Vec<SnapshotRow<'a>>,
    auto: BTreeMap<Provider, store::Auto>,
    switch_interval: u64,
    at: u64,
}

impl Snapshot<'_> {
    fn json(&self) -> Result<Value> {
        let metadata = self.metadata;
        let coordination = aswap::coordination::settings(metadata)?;
        // A corrupt reset log should still leave accounts visible.
        let usage_resets = {
            let _lock = metadata.lock()?;
            aswap::reset::load(metadata)
                .map(|log| log.summary())
                .unwrap_or_else(|err| json!({"error": err.to_string()}))
        };
        let auto: serde_json::Map<_, _> = self
            .auto
            .iter()
            .map(|(p, a)| (p.as_str().to_owned(), a.public_json()))
            .collect();
        let accounts: Vec<_> = self.rows.iter().map(|row| row.json(self.at)).collect();
        Ok(json!({
            "schema_version":1,"accounts":accounts,"auto":auto,
            "codex_usage_resets":usage_resets,"switch_interval_seconds":self.switch_interval,
            "credential_mode":if coordination.owner {"refresh-owner"}else if coordination.host.is_some(){"access-only-follower"}else{"refresh-disabled"},
            "last_sync":coordination.last_sync,"sync":aswap::coordination::status(metadata)?
        }))
    }
}

fn snapshot(stores: &[Store], filter: Option<Provider>, refresh: bool) -> Result<Snapshot<'_>> {
    let mut result = Snapshot {
        metadata: &stores[0],
        rows: Vec::new(),
        auto: BTreeMap::new(),
        switch_interval: 0,
        at: now(),
    };
    for (index, store) in stores.iter().enumerate() {
        let state = {
            let _lock = store.lock()?;
            store.load()?
        };
        // StoreView orders the legacy root first when aggregating. Its
        // settings continue to supply the existing scalar metadata fields.
        if index == 0 {
            result.switch_interval = state.switch_interval_seconds;
        }
        for account in state
            .accounts
            .into_iter()
            .filter(|a| filter.is_none_or(|p| p == a.provider))
        {
            let cached = if refresh {
                usage::cached(store, &account)?
            } else {
                let _lock = store.lock()?;
                let path = store
                    .profile(account.provider, &account.label)?
                    .join("usage-cache.json");
                if path.exists() {
                    store::json_read(&path)?
                } else {
                    usage::Cached::default()
                }
            };
            result.rows.push(SnapshotRow {
                store,
                active: state.active.get(&account.provider) == Some(&account.label),
                reserved: state
                    .auto
                    .get(&account.provider)
                    .is_some_and(|s| !s.allows(&account.label, None)),
                duplicates: duplicate_labels(store, &account)?,
                account,
                cached,
            });
        }
        result.auto.extend(state.auto);
    }
    result.at = now();
    Ok(result)
}

fn output_snapshot(snapshot: &Snapshot<'_>, structured: bool, compact: bool) -> Result<()> {
    let value = snapshot.json()?;
    let pretty = if structured {
        String::new()
    } else {
        terminal_text(snapshot, compact)?
    };
    output(&value, structured, pretty)
}

fn duplicate_labels(store: &Store, account: &store::Account) -> Result<Vec<String>> {
    let _lock = store.lock()?;
    Ok(aswap::identity::duplicates(
        store,
        account,
        &store.load()?.accounts,
    ))
}
fn terminal_text(snapshot: &Snapshot<'_>, compact: bool) -> Result<String> {
    let identities = snapshot
        .rows
        .iter()
        .map(|row| {
            let account = &row.account;
            let store = row.store;
            let _lock = store.lock()?;
            Ok(aswap::identity::read(
                account.provider,
                &store.profile(account.provider, &account.label)?,
            )
            .unwrap_or_default())
        })
        .collect::<Result<Vec<_>>>()?;
    let rows: Vec<_> = snapshot
        .rows
        .iter()
        .zip(&identities)
        .map(|(row, identity)| aswap::display::Row {
            provider: row.account.provider.as_str(),
            label: &row.account.label,
            active: row.active,
            reserved: row.reserved,
            identity,
            cached: &row.cached,
            duplicates: &row.duplicates,
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
    let default = store::Auto::default();
    let has_agy = snapshot
        .rows
        .iter()
        .any(|r| r.account.provider == Provider::Agy);
    let policies: Vec<_> = [Provider::Claude, Provider::Codex, Provider::Agy]
        .into_iter()
        .filter(|p| *p != Provider::Agy || has_agy)
        .map(|provider| (provider, snapshot.auto.get(&provider).unwrap_or(&default)))
        .map(|(provider, a)| aswap::display::Policy {
            provider: provider.as_str(),
            enabled: a.enabled,
            threshold: a.threshold,
            prefer: a.prefer,
            auto_reset: a.auto_reset,
        })
        .collect();
    let color = io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    let mut text =
        aswap::display::render_with(&rows, &policies, compact, width, color, snapshot.at);
    if !rows.is_empty() {
        text.push_str(if compact { "\n" } else { "\n\n" });
        text.push_str(
            &aswap::display::switch_interval_lines(snapshot.switch_interval, width, color)
                .join("\n"),
        );
    }
    Ok(text)
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
    if let Action::Gc {
        apply,
        dry_run,
        legacy_home,
    } = cli.command
    {
        use aswap::retention::{self, Mode};
        let mode = match (apply, dry_run) {
            (true, _) => Mode::Apply,
            (_, true) => Mode::Preview,
            _ => Mode::Configured,
        };
        let report = retention::run(mode, &legacy_home)?;
        let value = serde_json::to_value(&report).safe("cannot encode retention report")?;
        output(&value, cli.json, report.text())?;
        return Ok(i32::from(report.incomplete()));
    }
    let view = StoreView::from_env()?;
    match cli.command {
        Action::Gc { .. } => unreachable!(),
        Action::Sessions { provider } => {
            let mut rows = Vec::new();
            for store in view.stores(provider)? {
                rows.extend(
                    aswap::live::sessions(&store)?
                        .into_iter()
                        .filter(|r| provider.is_none_or(|p| r["provider"] == p.as_str())),
                );
            }
            let pretty = if rows.is_empty() {
                "No local running sessions".into()
            } else {
                rows.iter()
                    .map(|r| {
                        format!(
                            "{}:{} · terminal {} · supervisor PID {} · child PID {}",
                            r["provider"].as_str().unwrap(),
                            r["account"].as_str().unwrap(),
                            r["terminal"]
                                .as_str()
                                .unwrap_or("unknown (started before session tracking)"),
                            r["supervisor_pid"],
                            r["child_pid"]
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            output(&json!({"sessions": rows}), cli.json, pretty)?;
        }
        Action::Add { provider, label } => {
            let store = view.for_provider(provider)?;
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
        Action::List { provider } | Action::Status { provider } => {
            let compact = matches!(cli.command, Action::List { .. });
            let stores = view.stores(provider)?;
            let snapshot = snapshot(&stores, provider, !compact)?;
            output_snapshot(&snapshot, cli.json, compact)?;
        }
        Action::Switch {
            provider,
            label,
            activate_live,
        } => {
            let store = view.for_provider(provider)?;
            let a = provider::switch(&store, provider, &label, activate_live)?;
            output(
                &json!({"active":a,"live_activated":activate_live}),
                cli.json,
                format!("Selected {provider}:{label}"),
            )?;
        }
        Action::Remove { provider, label } => {
            let store = view.for_provider(provider)?;
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
        Action::Restore {
            provider,
            label,
            from,
            force,
        } => {
            let store = view.for_provider(provider)?;
            let (account, replaced) =
                aswap::provider::restore(&store, provider, &label, &from, force)?;
            output(
                &json!({"restored":account,"replaced_login_backup":replaced}),
                cli.json,
                match &replaced {
                    None => {
                        format!("{provider}:{label} already holds that login; nothing to restore")
                    }
                    Some(replaced) => format!(
                        "Restored {provider}:{label} from {}; the replaced login is in {}",
                        from.display(),
                        replaced.display()
                    ),
                },
            )?;
        }
        Action::Rename { provider, old, new } => {
            let store = view.for_provider(provider)?;
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
            if provider.is_none() && account.is_some() {
                return Err(Error("--account requires a provider"));
            }
            let stores = view.stores(provider)?;
            let selected = if let Some(provider) = provider {
                let store = &stores[0];
                let a = {
                    let _lock = store.lock()?;
                    store.account(&store.load()?, provider, account.as_deref())?
                };
                usage::cached_force(store, &a, refresh)?;
                Some(a.label)
            } else {
                for store in &stores {
                    let accounts = {
                        let _lock = store.lock()?;
                        store.load()?.accounts
                    };
                    for a in accounts {
                        usage::cached_force(store, &a, refresh)?;
                    }
                }
                None
            };
            let mut snapshot = snapshot(&stores, provider, false)?;
            if let Some(label) = selected {
                snapshot.rows.retain(|row| row.account.label == label);
            }
            output_snapshot(&snapshot, cli.json, false)?;
        }
        Action::Run {
            provider,
            account,
            args,
            auto,
        } => {
            let store = view.for_provider(provider)?;
            return runner::run(&store, provider, account.as_deref(), &args, auto);
        }
        Action::Auto {
            global: Some(AutoGlobal::SwitchInterval { seconds }),
            ..
        } => {
            let store = view.legacy()?;
            let _lock = store.lock()?;
            let mut state = store.load()?;
            if let Some(seconds) = seconds {
                state.switch_interval_seconds = seconds;
                store.save(&state)?;
            }
            let seconds = state.switch_interval_seconds;
            output(
                &json!({"switch_interval_seconds":seconds}),
                cli.json,
                aswap::display::switch_interval_text(seconds),
            )?;
        }
        Action::Auto {
            provider,
            action,
            value,
            activate_live,
            ..
        } => {
            let (Some(provider), Some(action)) = (provider, action) else {
                unreachable!("clap requires a provider and an action without a subcommand")
            };
            let store = view.for_provider(provider)?;
            if matches!(
                action,
                AutoAction::Threshold
                    | AutoAction::ResumePrompt
                    | AutoAction::Prefer
                    | AutoAction::Reset
                    | AutoAction::Reserve
                    | AutoAction::Unreserve
            ) != value.is_some()
            {
                return Err(Error(
                    "threshold requires a percentage; resume-prompt requires text or off; prefer requires headroom or weekly-reset; reset requires off, dry-run or on; reserve and unreserve require an account label; other actions take no value",
                ));
            }
            match action {
                AutoAction::Reserve | AutoAction::Unreserve => {
                    let label = value.unwrap();
                    let _lock = store.lock()?;
                    let mut state = store.load()?;
                    store.account(&state, provider, Some(&label))?;
                    let settings = state.auto.entry(provider).or_default();
                    let reserved = matches!(action, AutoAction::Reserve);
                    if reserved {
                        if settings.allows(&label, None) {
                            settings.reserved.push(label.clone());
                        }
                    } else {
                        settings.reserved.retain(|item| item != &label);
                    }
                    let v = json!({"provider":provider,"label":label,"reserved":reserved,"auto":settings.public_json()});
                    store.save(&state)?;
                    let action = if reserved { "Reserved" } else { "Unreserved" };
                    output(&v, cli.json, format!("{action} {provider}:{label}"))?;
                }
                AutoAction::On
                | AutoAction::Off
                | AutoAction::Threshold
                | AutoAction::Prefer
                | AutoAction::Reset
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
                        AutoAction::Prefer => {
                            settings.prefer =
                                store::Prefer::from_str(value.as_deref().unwrap(), false).map_err(
                                    |_| Error("prefer must be headroom or weekly-reset"),
                                )?;
                        }
                        AutoAction::Reset => {
                            let mode = store::AutoReset::from_str(value.as_deref().unwrap(), false)
                                .map_err(|_| Error("reset must be off, dry-run or on"))?;
                            aswap::reset::set_mode(&store, provider, settings, mode)?;
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
                    let v = json!({"provider":provider,"auto":settings.public_json()});
                    let pretty = format!(
                        "{provider}  {}",
                        aswap::display::policy_text(&aswap::display::Policy {
                            provider: provider.as_str(),
                            enabled: settings.enabled,
                            threshold: settings.threshold,
                            prefer: settings.prefer,
                            auto_reset: settings.auto_reset,
                        })
                    );
                    store.save(&state)?;
                    output(&v, cli.json, pretty)?;
                }
                AutoAction::Once | AutoAction::Watch => {
                    if matches!(action, AutoAction::Watch) {
                        // The monitor keeps the owner's push stream open and
                        // polls every 30s while no stream delivers, whatever
                        // its usage tick and auto policy.
                        aswap::coordination::spawn_sync(&store);
                    }
                    loop {
                        // A reset first, so this tick's selection can use it.
                        let reset = match aswap::reset::tick(&store, provider) {
                            Ok(step) => step,
                            Err(err) if matches!(action, AutoAction::Watch) => {
                                eprintln!(
                                    "aswap: usage reset check failed ({err}); retrying next tick"
                                );
                                None
                            }
                            Err(err) => return Err(err),
                        };
                        if let Some(message) = reset.as_ref().and_then(|s| s.message(now())) {
                            eprintln!("{message}");
                        }
                        let selected = match runner::tick(&store, provider, activate_live) {
                            Ok(selected) => selected,
                            // The monitor outlives store trouble (a full disk, a
                            // busy lock): report it and try again next tick.
                            Err(err) if matches!(action, AutoAction::Watch) => {
                                eprintln!("aswap: monitor tick failed ({err}); retrying");
                                std::thread::sleep(Duration::from_secs(usage::TTL.min(60)));
                                continue;
                            }
                            Err(err) => return Err(err),
                        };
                        let mut report = json!({"provider":provider,"selected":selected});
                        if let Some(step) = &reset {
                            report["usage_reset"] = json!(step);
                        }
                        output(
                            &report,
                            cli.json,
                            selected
                                .map(|s| format!("Selected {provider}:{s}"))
                                .unwrap_or("No switch".into()),
                        )?;
                        if matches!(action, AutoAction::Once) {
                            break;
                        }
                        std::thread::sleep(Duration::from_secs(usage::TTL));
                    }
                }
            }
        }
        Action::Sync {
            host,
            owner,
            off,
            serve,
            watch,
        } => {
            let store = view.legacy()?;
            if serve {
                aswap::coordination::serve(&store, watch)?;
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
            let stores = view.stores(None)?;
            let v = waybar(&snapshot(&stores, None, true)?.json()?);
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
