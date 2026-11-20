//! Retention of finished private launch views. Reports contain paths and counts only.
mod archive;
mod codex_index;
mod files;
mod process;

use crate::{
    Error, Result, Safe, live,
    provider::Provider,
    shared,
    store::{self, Store},
};
use files::{Dir, Home, Inventory, Plan};
use serde::{Deserialize, Serialize};
use std::{
    ffi::{OsStr, OsString},
    fmt::Write as _,
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Read},
    os::unix::{
        ffi::OsStringExt,
        fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

#[derive(Deserialize)]
struct Config {
    #[serde(default)]
    retention: Settings,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Settings {
    enabled: bool,
    min_age: u64,
    archive_dir: Option<PathBuf>,
    dry_run: bool,
    io_pressure_limit: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            min_age: 2 * 24 * 3600,
            archive_dir: None,
            dry_run: true,
            io_pressure_limit: 30.0,
        }
    }
}

impl Settings {
    fn load(path: &Path) -> Result<Self> {
        // Home Manager may install this declarative file as a symlink.
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Self::default()),
            Err(_) => return Err(Error("cannot read aswap configuration")),
        };
        if !file
            .metadata()
            .safe("cannot inspect aswap configuration")?
            .is_file()
        {
            return Err(Error("aswap configuration must be a regular TOML file"));
        }
        const MAX_CONFIG: u64 = 1024 * 1024;
        let mut text = String::new();
        file.take(MAX_CONFIG + 1)
            .read_to_string(&mut text)
            .safe("cannot read aswap configuration")?;
        if text.len() as u64 > MAX_CONFIG {
            return Err(Error("aswap configuration exceeds size limit"));
        }
        let config: Config = toml::from_str(&text).safe("invalid aswap retention configuration")?;
        Ok(config.retention)
    }
}

struct Pressure<'a> {
    read: &'a mut dyn FnMut() -> Result<f64>,
    limit: f64,
    next: Instant,
    stopped: Option<&'static str>,
}

impl Pressure<'_> {
    fn check(&mut self, force: bool) -> Result<()> {
        if let Some(reason) = self.stopped {
            return Err(Error(reason));
        }
        if !force && Instant::now() < self.next {
            return Ok(());
        }
        self.next = Instant::now() + Duration::from_secs(1);
        let reason = match (self.read)() {
            Ok(value) if !(0.0..=100.0).contains(&value) => "I/O pressure unavailable",
            Ok(value) if value > self.limit => "I/O pressure is above the configured limit",
            Ok(_) => return Ok(()),
            Err(_) => "I/O pressure unavailable",
        };
        self.stopped = Some(reason);
        Err(Error(reason))
    }
}

#[derive(Clone, Copy)]
pub enum Mode {
    Configured,
    Preview,
    Apply,
}

#[derive(Serialize)]
pub struct Report {
    enabled: bool,
    dry_run: bool,
    stopped: Option<&'static str>,
    warnings: Vec<String>,
    totals: Totals,
    groups: Vec<Group>,
}

#[derive(Serialize)]
struct Group {
    provider: Provider,
    label: String,
    totals: Totals,
    views: Vec<ViewResult>,
}

#[derive(Default, Serialize)]
struct Totals {
    examined: u64,
    kept: u64,
    finished: u64,
    removed: u64,
    bytes_reclaimed: i128,
    bytes_moved: u64,
    bytes_archived: u64,
}

#[derive(Serialize)]
struct ViewResult {
    path: String,
    verdict: Verdict,
    reason: &'static str,
    bytes_reclaimed: i128,
    bytes_moved: u64,
    bytes_archived: u64,
    archive: Option<String>,
    codex_index_rows_rewritten: u64,
    codex_index_backups: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
enum Verdict {
    Kept,
    Finished,
    Removed,
}

impl Totals {
    fn add(&mut self, view: &ViewResult) {
        self.examined += 1;
        match view.verdict {
            Verdict::Kept => self.kept += 1,
            Verdict::Finished => self.finished += 1,
            Verdict::Removed => self.removed += 1,
        }
        self.bytes_reclaimed += view.bytes_reclaimed;
        self.bytes_moved += view.bytes_moved;
        self.bytes_archived += view.bytes_archived;
    }

    fn text(&self, dry_run: bool) -> String {
        format!(
            "{} examined, {} kept, {} finished, {} removed, {:.3} GB ({} bytes) {}, {} bytes {}, {} bytes {}",
            self.examined,
            self.kept,
            self.finished,
            self.removed,
            self.bytes_reclaimed as f64 / 1e9,
            self.bytes_reclaimed,
            if dry_run { "would regain" } else { "reclaimed" },
            self.bytes_moved,
            if dry_run { "to move" } else { "moved" },
            self.bytes_archived,
            if dry_run { "to archive" } else { "archived" },
        )
    }
}

impl Report {
    pub fn text(&self) -> String {
        if !self.enabled {
            return "Runtime retention is disabled".into();
        }
        let mut text = String::new();
        if self.dry_run {
            text.push_str("Dry run. Reclaim estimates exclude bytes sent to the archive.\n");
        }
        for warning in &self.warnings {
            let _ = writeln!(text, "Warning — {warning}");
        }
        for group in &self.groups {
            let _ = writeln!(
                text,
                "{}:{} — {}",
                group.provider,
                group.label,
                group.totals.text(self.dry_run)
            );
            for view in &group.views {
                let verdict = match view.verdict {
                    Verdict::Kept => "kept",
                    Verdict::Finished => "finished",
                    Verdict::Removed => "removed",
                };
                let _ = writeln!(
                    text,
                    "  {:?} — {verdict} ({}), {} bytes {}, {} {}, {} {}",
                    view.path,
                    view.reason,
                    view.bytes_reclaimed,
                    if self.dry_run {
                        "to reclaim"
                    } else {
                        "reclaimed"
                    },
                    view.bytes_moved,
                    if self.dry_run { "to move" } else { "moved" },
                    view.bytes_archived,
                    if self.dry_run {
                        "to archive"
                    } else {
                        "archived"
                    },
                );
                if let Some(path) = &view.archive {
                    let _ = writeln!(text, "    Archive {path:?}");
                }
                for backup in &view.codex_index_backups {
                    let _ = writeln!(text, "    Codex index backup {backup:?}");
                }
                if view.codex_index_rows_rewritten != 0 {
                    let _ = writeln!(
                        text,
                        "    Rewrote {} Codex index rows",
                        view.codex_index_rows_rewritten
                    );
                }
            }
        }
        if let Some(reason) = self.stopped {
            let _ = writeln!(text, "Stopped — {reason}");
        }
        let _ = write!(text, "Total — {}", self.totals.text(self.dry_run));
        text
    }
}

pub fn run(mode: Mode) -> Result<Report> {
    let settings = Settings::load(&Store::config_dir()?.join("config.toml"))?;
    let root = Store::root_from_env()?;
    if settings.enabled && Dir::open(&root.join("runtimes")).is_ok() {
        process::lower_priority()?;
    }
    let home = |provider: Provider| shared::shared_home(&root, provider, provider.current_home()?);
    let mut in_use = |view: &Path| Ok(process::uses_view(view)? || live::view_in_use(&root, view)?);
    Retention {
        root: &root,
        at: SystemTime::now(),
        home: &home,
        in_use: &mut in_use,
        pressure: Pressure {
            read: &mut process::io_pressure,
            limit: settings.io_pressure_limit,
            next: Instant::now(),
            stopped: None,
        },
    }
    .run(settings, mode)
}

struct Retention<'a> {
    root: &'a Path,
    at: SystemTime,
    home: &'a dyn Fn(Provider) -> Result<PathBuf>,
    in_use: &'a mut dyn FnMut(&Path) -> Result<bool>,
    pressure: Pressure<'a>,
}

impl Retention<'_> {
    fn run(mut self, settings: Settings, mode: Mode) -> Result<Report> {
        let dry_run = match mode {
            Mode::Configured => settings.dry_run,
            Mode::Preview => true,
            Mode::Apply => false,
        };
        let mut report = Report {
            enabled: settings.enabled,
            dry_run,
            stopped: None,
            warnings: Vec::new(),
            totals: Totals::default(),
            groups: Vec::new(),
        };
        if !settings.enabled {
            return Ok(report);
        }
        if !(0.0..=100.0).contains(&settings.io_pressure_limit) {
            return Err(Error(
                "retention io_pressure_limit must be between 0 and 100",
            ));
        }
        let runtimes = self.root.join("runtimes");
        if self.root.ancestors().any(|p| p.join(".git").exists()) {
            return Err(Error("account stores must live outside a Git checkout"));
        }
        let archive = settings
            .archive_dir
            .unwrap_or_else(|| self.root.join("archive"));
        if !archive.is_absolute()
            || archive.starts_with(&runtimes)
            || archive
                .components()
                .any(|c| c == std::path::Component::ParentDir)
            || archive.ancestors().any(|p| p.join(".git").exists())
        {
            return Err(Error(
                "archive_dir must be absolute and outside runtimes and Git checkouts",
            ));
        }
        let root = match Dir::open(&runtimes) {
            Ok(root) => root,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(report),
            Err(_) => return Err(Error("cannot safely open runtimes directory")),
        };
        // Serializes concurrent GC, including old views without a lock file.
        // Flocking the existing directory leaves dry-run entirely read-only.
        fs2::FileExt::try_lock_exclusive(&root.0)
            .map_err(|_| Error("cannot lock runtimes directory (another GC may be active)"))?;
        let mut backups = codex_index::Backups::default();
        'providers: for provider in [Provider::Claude, Provider::Codex] {
            let provider_name = OsStr::new(provider.as_str());
            let labels = match root
                .child(provider_name)
                .and_then(|dir| Ok((dir.names()?, dir)))
            {
                Ok(result) => result,
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(_) => {
                    report
                        .warnings
                        .push(format!("cannot inspect {provider} runtime labels"));
                    continue;
                }
            };
            let (names, provider_dir) = labels;
            for name in names {
                let Some(label) = name.to_str().filter(|label| store::label(label).is_ok()) else {
                    report
                        .warnings
                        .push(format!("unrecognized {provider} runtime label"));
                    continue;
                };
                let (runs, label_dir) = match provider_dir
                    .child(&name)
                    .and_then(|dir| Ok((dir.names()?, dir)))
                {
                    Ok(result) => result,
                    Err(_) => {
                        report.warnings.push(format!(
                            "cannot inspect {provider}:{label} runtime directory"
                        ));
                        continue;
                    }
                };
                let mut group = Group {
                    provider,
                    label: label.into(),
                    totals: Totals::default(),
                    views: Vec::new(),
                };
                for run in runs {
                    if !run
                        .to_str()
                        .and_then(|s| s.strip_prefix("run-"))
                        .is_some_and(|s| store::label(s).is_ok())
                    {
                        continue;
                    }
                    if self.pressure.check(true).is_err() {
                        break;
                    }
                    let path = runtimes.join(provider_name).join(label).join(&run);
                    let mut result = ViewResult {
                        path: path.to_string_lossy().into(),
                        verdict: Verdict::Kept,
                        reason: "eligibility uncertain",
                        bytes_reclaimed: 0,
                        bytes_moved: 0,
                        bytes_archived: 0,
                        archive: None,
                        codex_index_rows_rewritten: 0,
                        codex_index_backups: Vec::new(),
                    };
                    let candidate = Candidate {
                        path: &path,
                        provider,
                        label_dir: &label_dir,
                        run: &run,
                        archive: &archive.join(provider_name).join(label).join(&run),
                        archive_root: &archive,
                        min_age: Duration::from_secs(settings.min_age),
                        dry_run,
                    };
                    if let Err(error) = self.view(candidate, &mut backups, &mut result) {
                        result.reason = error.0;
                    }
                    group.totals.add(&result);
                    report.totals.add(&result);
                    group.views.push(result);
                    if self.pressure.stopped.is_some() {
                        break;
                    }
                }
                if !group.views.is_empty() {
                    report.groups.push(group);
                }
                if self.pressure.stopped.is_some() {
                    break 'providers;
                }
            }
        }
        report.stopped = self.pressure.stopped;
        Ok(report)
    }

    fn view(
        &mut self,
        candidate: Candidate<'_>,
        backups: &mut codex_index::Backups,
        result: &mut ViewResult,
    ) -> Result<()> {
        let Candidate {
            path,
            provider,
            label_dir,
            run,
            archive,
            archive_root,
            min_age,
            dry_run,
        } = candidate;
        let view = label_dir
            .child(run)
            .safe("unsafe or unreadable view directory")?;
        let metadata = view.0.metadata().safe("cannot inspect view directory")?;
        let modified = metadata
            .modified()
            .safe("view modification time unavailable")?;
        let changed = SystemTime::UNIX_EPOCH
            .checked_add(Duration::new(
                metadata
                    .ctime()
                    .try_into()
                    .safe("invalid view change time")?,
                metadata.ctime_nsec() as u32,
            ))
            .ok_or(Error("invalid view change time"))?;
        if !self
            .at
            .duration_since(modified.max(changed))
            .is_ok_and(|age| age > min_age)
        {
            return Err(Error("younger than minimum age"));
        }

        let sibling = shared::view_lock_path(Path::new(run));
        let _auth = try_lock(label_dir, sibling.as_os_str())?;
        let lifetime = try_lock(&view, OsStr::new(shared::VIEW_LIFETIME_LOCK))?;
        if (self.in_use)(path)? {
            return Err(Error("used by a live process or supervisor"));
        }

        let inventory = Inventory::scan(&view, provider, &mut self.pressure)?;
        let home = self.shared_home(provider, &inventory, lifetime.as_ref())?;
        let mut plan = Plan::classify(
            &inventory,
            &view,
            home.as_ref().map(|home| &home.dir),
            &mut self.pressure,
        )?;
        let archive_path = plan
            .has_conflicts()
            .then(|| archive.to_string_lossy().into_owned());
        if dry_run {
            if provider == Provider::Codex
                && let Some(home) = &home
            {
                codex_index::check(
                    home,
                    path,
                    None,
                    |relative, pressure| {
                        inventory.verify_rollout(relative, &view, home, true, pressure)
                    },
                    &mut self.pressure,
                )?;
            }
            result.verdict = Verdict::Finished;
            result.reason = "eligible for removal";
            result.bytes_reclaimed = i128::from(inventory.bytes)
                - i128::from(plan.move_bytes)
                - i128::from(plan.archive_bytes);
            result.bytes_moved = plan.move_bytes;
            result.bytes_archived = plan.archive_bytes;
            result.archive = archive_path;
            return Ok(());
        }

        if let Some(home) = &home {
            let published = plan.publish(
                &inventory,
                &view,
                &home.dir,
                &mut result.bytes_moved,
                &mut self.pressure,
            );
            result.bytes_reclaimed -= i128::from(result.bytes_moved);
            published?;
        }
        if plan.has_conflicts() {
            let archive_added = archive::preserve(
                archive,
                &view,
                plan.conflicts(&inventory),
                &mut self.pressure,
            )?;
            result.bytes_reclaimed -= i128::from(archive_added);
            result.bytes_archived = plan.archive_bytes;
            result.archive = archive_path;
        }

        if provider == Provider::Codex
            && let Some(home) = &home
        {
            let (rows, bytes) = codex_index::check(
                home,
                path,
                Some((backups, archive_root, &mut result.codex_index_backups)),
                |relative, pressure| {
                    inventory.verify_rollout(relative, &view, home, false, pressure)
                },
                &mut self.pressure,
            )?;
            result.codex_index_rows_rewritten = rows;
            result.bytes_reclaimed -= i128::from(bytes);
        }

        // Recheck both the sources and published copies before deleting anything.
        if let Some(home) = &home {
            plan.verify_shared(&inventory, &home.dir, &mut self.pressure)?;
            let current = Dir::open(&home.path)
                .safe("cannot recheck shared provider home")?
                .0
                .metadata()
                .safe("cannot inspect shared provider home")?;
            let opened = home
                .dir
                .0
                .metadata()
                .safe("cannot inspect shared provider home")?;
            if (current.dev(), current.ino()) != (opened.dev(), opened.ino()) {
                return Err(Error("shared provider home changed"));
            }
        }
        if (self.in_use)(path)? {
            return Err(Error("used by a live process or supervisor"));
        }
        inventory.verify(&view, &mut self.pressure)?;
        self.pressure.check(true)?;
        let current = label_dir
            .child(run)
            .safe("cannot recheck view directory")?
            .0
            .metadata()
            .safe("cannot inspect view directory")?;
        if (current.dev(), current.ino()) != (metadata.dev(), metadata.ino()) {
            return Err(Error("view directory changed"));
        }
        // Linux remove_dir_all uses descriptor-relative traversal and unlinks
        // symlinks without entering their targets.
        fs::remove_dir_all(label_dir.path().join(run)).safe("cannot remove view directory")?;
        result.verdict = Verdict::Removed;
        result.reason = "finished and transcripts preserved";
        result.bytes_reclaimed += i128::from(inventory.bytes);
        Ok(())
    }

    fn shared_home(
        &self,
        provider: Provider,
        inventory: &Inventory,
        lifetime: Option<&File>,
    ) -> Result<Option<Home>> {
        let recorded = lifetime
            .map(|file| {
                let mut bytes = Vec::new();
                file.take(4097)
                    .read_to_end(&mut bytes)
                    .safe("cannot read view's shared home")?;
                if bytes.is_empty() || bytes.len() > 4096 {
                    return Err(Error("invalid recorded shared home"));
                }
                Ok(PathBuf::from(OsString::from_vec(bytes)))
            })
            .transpose()?;
        let mut hints = inventory
            .shared_links
            .iter()
            .filter_map(|(_, target)| target.parent());
        let hinted = hints.next();
        if let Some(home) = recorded.as_deref().or(hinted)
            && (!home.is_absolute()
                || home.starts_with(self.root)
                || hinted.is_some_and(|hint| hint != home)
                || hints.any(|hint| hint != home))
        {
            return Err(Error("cannot establish view's shared home"));
        }
        if inventory.transcripts.is_empty() && provider != Provider::Codex {
            return Ok(None);
        }
        let home = match recorded.or_else(|| hinted.map(Path::to_owned)) {
            Some(home) => home,
            None => (self.home)(provider)?,
        };
        if home.starts_with(self.root) || home.ancestors().any(|p| p.join(".git").exists()) {
            return Err(Error(
                "shared home must be outside the store and Git checkouts",
            ));
        }
        match Dir::open(&home) {
            Ok(dir) => Ok(Some(Home { path: home, dir })),
            Err(error)
                if inventory.transcripts.is_empty() && error.kind() == ErrorKind::NotFound =>
            {
                Ok(None)
            }
            Err(_) => Err(Error("cannot safely open shared provider home")),
        }
    }
}

struct Candidate<'a> {
    path: &'a Path,
    provider: Provider,
    label_dir: &'a Dir,
    run: &'a OsStr,
    archive: &'a Path,
    archive_root: &'a Path,
    min_age: Duration,
    dry_run: bool,
}

fn try_lock(dir: &Dir, name: &OsStr) -> Result<Option<File>> {
    match dir.file(name) {
        Ok((file, _)) => {
            if let Err(error) = fs2::FileExt::try_lock_exclusive(&file) {
                if error.kind() == ErrorKind::WouldBlock {
                    return Err(Error("view lock held"));
                }
                return Err(Error("cannot establish view lock status"));
            }
            Ok(Some(file))
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(_) => Err(Error("unsafe or unreadable view lock")),
    }
}

#[cfg(test)]
mod tests;
