use aswap::{
    provider::Provider,
    store::{self, Account, Store},
    usage::{Cached, Usage, Window},
};
use serde_json::json;
use std::{
    fs,
    io::Read,
    os::unix::fs::PermissionsExt,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Children(Vec<Child>);
impl Drop for Children {
    fn drop(&mut self) {
        for child in &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn scenario(provider: Provider, recovered: &str, exited: bool, count: usize) {
    review_scenario(provider, recovered, exited, count, "ordinary");
}

fn review_scenario(provider: Provider, recovered: &str, exited: bool, count: usize, mode: &str) {
    let interrupted = recovered == "interrupt";
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("data/aswap")).unwrap();
    for home in ["claude", "codex"] {
        fs::create_dir(temp.path().join(home)).unwrap();
    }
    let log = temp.path().join("launches");
    fs::create_dir(&log).unwrap();
    let reset = aswap::now() + 4;
    let mut state = store.load().unwrap();
    for (label, used) in [("current", 20.0), ("other", 100.0)] {
        let profile = store.profile(provider, label).unwrap();
        let auth = if provider == Provider::Claude {
            json!({"claudeAiOauth":{"accessToken":format!("synthetic-{label}")}})
        } else {
            json!({"tokens":{"access_token":format!("synthetic-{label}")}})
        };
        store::json_write(&profile.join(provider.auth_file()), &auth).unwrap();
        store::json_write(
            &profile.join("usage-cache.json"),
            &Cached {
                fetched_at: aswap::now(),
                // Fixtures install the next observation directly. Suppress
                // real requests even if the test writer is descheduled.
                last_attempt: reset + 60,
                next_poll_at: reset + 3600,
                usage: Usage {
                    windows: vec![Window {
                        name: "weekly".into(),
                        used_percent: used,
                        resets_at: Some(if label == recovered {
                            reset
                        } else {
                            if mode == "model" {
                                reset + 10
                            } else {
                                reset + 60
                            }
                        }),
                        duration_minutes: Some(10080),
                    }],
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
        state.accounts.push(Account {
            provider,
            label: label.into(),
        });
    }
    state.active.insert(provider, "current".into());
    store.save(&state).unwrap();

    let python = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|path| path.join("python3"))
        .find(|path| path.is_file())
        .unwrap();
    fs::create_dir(temp.path().join("bin")).unwrap();
    let fake = temp.path().join("bin").join(provider.to_string());
    fs::write(
        &fake,
        format!(
            "#!{}\n{}",
            python.display(),
            include_str!("fixtures/wake_provider.py")
        ),
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
    let mut children = Children(Vec::new());
    for index in 0..count {
        let id = format!("11111111-2222-4333-8444-{index:012}");
        children.0.push(
            Command::new(env!("CARGO_BIN_EXE_aswap"))
                .args([
                    "run",
                    &provider.to_string(),
                    "--auto",
                    "--account",
                    "current",
                    "--",
                    if provider == Provider::Claude {
                        "--resume"
                    } else {
                        "resume"
                    },
                    &id,
                    "--model",
                    "synthetic-model",
                ])
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap())
                .env("HOME", temp.path())
                .env("XDG_DATA_HOME", temp.path().join("data"))
                .env("CLAUDE_CONFIG_DIR", temp.path().join("claude"))
                .env("CODEX_HOME", temp.path().join("codex"))
                .env(provider.bin_var(), &fake)
                .env("ASWAP_TEST_LOG", &log)
                .env("ASWAP_TEST_MODE", mode)
                .env("XDG_CONFIG_HOME", temp.path().join("config"))
                .env("ASWAP_TEST_RECOVERED", recovered)
                .env("ASWAP_TEST_EXITED", if exited { "yes" } else { "no" })
                .stdin(if exited {
                    Stdio::piped()
                } else {
                    Stdio::null()
                })
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut sent = false;
    while aswap::now() < reset {
        assert!(Instant::now() < deadline);
        if exited
            && !sent
            && (if mode == "input" {
                fs::read_dir(&log).unwrap().count() == count
            } else {
                aswap::now() >= reset - 1
            })
        {
            if mode == "input" {
                std::thread::sleep(Duration::from_millis(60));
            }
            for child in &mut children.0 {
                use std::io::Write;
                child
                    .stdin
                    .take()
                    .unwrap()
                    .write_all(b"synthetic input while waiting\n")
                    .unwrap();
            }
            sent = true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if fs::read_dir(&log).unwrap().count() != count {
        for child in &mut children.0 {
            if let Some(status) = child.try_wait().unwrap() {
                let mut stderr = String::new();
                std::io::Read::read_to_string(child.stderr.as_mut().unwrap(), &mut stderr).unwrap();
                let mut stdout = String::new();
                std::io::Read::read_to_string(child.stdout.as_mut().unwrap(), &mut stdout).unwrap();
                panic!("provider exited before barrier: {status}: {stderr} {stdout}");
            }
        }
        panic!("providers did not reach launch barrier");
    }
    if interrupted {
        for child in &children.0 {
            // SAFETY: this is our unreaped supervisor child, never a reused PID.
            assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
        }
    } else {
        let cache_path = store
            .profile(provider, recovered)
            .unwrap()
            .join("usage-cache.json");
        let mut cache: Cached = store::json_read(&cache_path).unwrap();
        cache.error = None;
        cache.fetched_at = aswap::now();
        cache.usage.windows[0].used_percent = 2.0;
        cache.usage.windows[0].resets_at = Some(reset + 3600);
        store::json_write(&cache_path, &cache).unwrap();
    }
    if mode == "model" || mode == "race" {
        while if mode == "model" {
            aswap::now() < reset + 10
        } else {
            !log.join("revoked").exists()
        } {
            assert!(Instant::now() < deadline, "review barrier timed out");
            for child in &mut children.0 {
                if let Some(status) = child.try_wait().unwrap() {
                    let mut stderr = String::new();
                    std::io::Read::read_to_string(child.stderr.as_mut().unwrap(), &mut stderr)
                        .unwrap();
                    panic!("supervisor abandoned quota recovery: {status}: {stderr}");
                }
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        if mode == "race" {
            std::thread::sleep(Duration::from_millis(750));
        }
        for child in &mut children.0 {
            if let Some(status) = child.try_wait().unwrap() {
                let mut stderr = String::new();
                std::io::Read::read_to_string(child.stderr.as_mut().unwrap(), &mut stderr).unwrap();
                panic!("supervisor launched without verified quota: {status}: {stderr}");
            }
        }
        let label = if mode == "model" { "current" } else { "other" };
        let path = store
            .profile(provider, label)
            .unwrap()
            .join("usage-cache.json");
        let mut cache: Cached = store::json_read(&path).unwrap();
        cache.error = None;
        cache.fetched_at = aswap::now();
        cache.usage.windows[0].used_percent = 2.0;
        cache.usage.windows[0].resets_at = Some(reset + 3600);
        store::json_write(&path, &cache).unwrap();
    }
    for child in &mut children.0 {
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                let cache: Cached = store::json_read(
                    &store
                        .profile(provider, recovered)
                        .unwrap()
                        .join("usage-cache.json"),
                )
                .unwrap();
                let mut diagnostics =
                    format!("supervisor never woke after reset; cache={cache:?}\n");
                let _ = child.kill();
                let _ = child.wait();
                let mut stderr = String::new();
                child
                    .stderr
                    .as_mut()
                    .unwrap()
                    .read_to_string(&mut stderr)
                    .unwrap();
                diagnostics.push_str(&stderr);
                panic!("{diagnostics}");
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let mut stderr = String::new();
        std::io::Read::read_to_string(child.stderr.as_mut().unwrap(), &mut stderr).unwrap();
        let status = child.wait().unwrap();
        if interrupted {
            assert!(!status.success());
            assert!(stderr.contains("provider session interrupted"), "{stderr}");
            continue;
        }
        if !status.success() {
            let mut stdout = String::new();
            std::io::Read::read_to_string(child.stdout.as_mut().unwrap(), &mut stdout).unwrap();
            panic!("{stderr}\n{stdout}");
        }
        if mode == "race" {
            assert!(
                stderr.contains("lost eligibility before commit; waiting"),
                "{stderr}"
            );
            assert!(!stderr.contains("resuming on claude:current"), "{stderr}");
        }
        assert!(stderr.contains("watching all accounts"), "{stderr}");
        assert!(
            stderr.contains(&format!(
                "quota available on {provider}:{}",
                if mode == "model" {
                    "current"
                } else {
                    recovered
                }
            )),
            "{stderr}"
        );
        if recovered == "current" {
            assert!(stderr.contains("on the current account"), "{stderr}");
        }
    }
    assert_eq!(
        fs::read_dir(log).unwrap().count(),
        count * if interrupted { 1 } else { 2 } + usize::from(mode == "race")
    );
}

#[test]
fn claude_limit_dialog_wakes_on_current_or_other_account() {
    for recovered in ["current", "other"] {
        scenario(Provider::Claude, recovered, false, 1);
    }
}
#[test]
fn codex_limit_dialog_wakes_on_current_or_other_account() {
    for recovered in ["current", "other"] {
        scenario(Provider::Codex, recovered, false, 1);
    }
}

#[test]
fn waiting_after_provider_exit_can_be_cancelled_without_a_restart() {
    for provider in [Provider::Claude, Provider::Codex] {
        scenario(provider, "interrupt", true, 1);
    }
}

// Adapted from the independent reviewer reproductions in ~/handoff.
#[test]
fn review_input_queued_after_exit_claude() {
    review_scenario(Provider::Claude, "other", true, 1, "input");
}
#[test]
fn review_input_queued_after_exit_codex() {
    review_scenario(Provider::Codex, "other", true, 1, "input");
}
#[test]
fn review_incompatible_other_does_not_abort_current_recovery() {
    review_scenario(Provider::Codex, "other", false, 1, "model");
}
#[test]
fn review_quota_revoked_between_selection_and_commit() {
    review_scenario(Provider::Claude, "other", false, 1, "race");
}
