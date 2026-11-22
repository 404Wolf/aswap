//! A Linux PTY keeps the provider's interactive UI intact during supervision.
use crate::{Error, Result, Safe};
use std::{
    fs::File,
    io::{self, IsTerminal, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::process::CommandExt,
    },
    process::{Child, Command},
};

struct Interrupt {
    flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    registrations: Vec<signal_hook::SigId>,
}
impl Interrupt {
    fn new() -> Result<Self> {
        let mut interrupt = Self {
            flag: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            registrations: Vec::new(),
        };
        for signal in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT, libc::SIGQUIT] {
            interrupt.registrations.push(
                signal_hook::flag::register(signal, interrupt.flag.clone())
                    .safe("cannot register terminal cleanup")?,
            );
        }
        Ok(interrupt)
    }
}
impl Drop for Interrupt {
    fn drop(&mut self) {
        for registration in self.registrations.drain(..) {
            signal_hook::low_level::unregister(registration);
        }
    }
}
pub struct Terminal {
    interrupt: Interrupt,
    master: File,
    slave: File,
    original: Option<libc::termios>,
    input_open: bool,
    started: bool,
    childless: bool,
    diagnostics: Diagnostics,
    /// A Codex terminal matches only Codex's own limit notices (see
    /// `diagnostic_line`); Claude's rules would misread its tool output.
    pub codex: bool,
    /// An agy terminal matches only agy's quota notices.
    pub agy: bool,
    last_input: Option<std::time::Instant>,
    pub auth_expired: bool,
    pub model_unsupported: bool,
    /// While set, login errors in the output are held in `held_auth` (with
    /// the time each was read) rather than counted: the provider may be
    /// redrawing a resumed conversation, not reporting a live failure.
    pub quiet_auth: bool,
    pub held_auth: Vec<std::time::Instant>,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct InputActivity {
    pub(crate) last_forwarded: Option<std::time::Instant>,
    pub(crate) waiting: bool,
}

/// macOS declares the request narrower than `ioctl` takes it.
#[allow(clippy::unnecessary_cast)]
const TIOCSCTTY: libc::c_ulong = libc::TIOCSCTTY as libc::c_ulong;

impl Terminal {
    pub fn open() -> Result<Self> {
        let interrupt = Interrupt::new()?;
        let mut master = -1;
        let mut slave = -1;
        // SAFETY: openpty initializes both descriptors; optional arguments are null.
        if unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                // Linux takes these as const and macOS as mut pointers.
                std::ptr::null_mut::<libc::termios>(),
                std::ptr::null_mut::<libc::winsize>(),
            )
        } != 0
        {
            return Err(Error("cannot create provider terminal"));
        }
        // SAFETY: these are distinct, newly opened descriptors owned by this object.
        let master = unsafe { File::from_raw_fd(master) };
        let slave = unsafe { File::from_raw_fd(slave) };
        let original = if io::stdin().is_terminal() {
            let mut attrs = std::mem::MaybeUninit::<libc::termios>::uninit();
            // SAFETY: tcgetattr writes the termios struct on success.
            if unsafe { libc::tcgetattr(0, attrs.as_mut_ptr()) } != 0 {
                return Err(Error("cannot read terminal mode"));
            }
            // SAFETY: the successful tcgetattr initialized attrs.
            let original = unsafe { attrs.assume_init() };
            let mut raw = original;
            // SAFETY: raw is a valid initialized termios value.
            unsafe {
                libc::cfmakeraw(&mut raw);
            }
            // SAFETY: valid termios and the current stdin terminal descriptor.
            if unsafe { libc::tcsetattr(0, libc::TCSANOW, &raw) } != 0 {
                return Err(Error("cannot configure terminal mode"));
            }
            Some(original)
        } else {
            None
        };
        Ok(Self {
            interrupt,
            master,
            slave,
            original,
            input_open: true,
            started: false,
            childless: true,
            diagnostics: Diagnostics::default(),
            codex: false,
            agy: false,
            last_input: None,
            auth_expired: false,
            model_unsupported: false,
            quiet_auth: false,
            held_auth: Vec::new(),
        })
    }
    pub fn spawn(&mut self, command: &mut Command) -> Result<Child> {
        for file in [&self.master, &self.slave] {
            // SAFETY: descriptors are open; prevents retaining the master in exec.
            unsafe {
                libc::fcntl(file.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
            }
        }
        command
            .stdin(self.slave.try_clone().safe("cannot clone terminal")?)
            .stdout(self.slave.try_clone().safe("cannot clone terminal")?)
            .stderr(self.slave.try_clone().safe("cannot clone terminal")?);
        // SAFETY: only async-signal-safe syscalls run between fork and exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, TIOCSCTTY, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        self.resize();
        if self.started {
            // Preparation/commit can take time after the last wait pump.
            // Drain outer input at the relaunch boundary, including a TTY,
            // and clear bytes forwarded before the previous exit was seen.
            self.childless = true;
            self.wait_for_restart(0)?;
            while self.input_open {
                let mut input = libc::pollfd {
                    fd: 0,
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: a single initialized descriptor, with no wait.
                let ready = unsafe { libc::poll(&mut input, 1, 0) };
                if ready < 0 {
                    if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(Error("cannot drain terminal input before resume"));
                }
                if ready == 0 {
                    break;
                }
                self.wait_for_restart(0)?;
            }
            // SAFETY: the private slave remains open while no provider runs.
            if unsafe { libc::tcflush(self.slave.as_raw_fd(), libc::TCIFLUSH) } != 0 {
                return Err(Error("cannot clear provider input before resume"));
            }
        }
        self.diagnostics = Diagnostics {
            codex: self.codex,
            agy: self.agy,
            ..Diagnostics::default()
        };
        self.last_input = None;
        self.auth_expired = false;
        self.model_unsupported = false;
        self.quiet_auth = false;
        self.held_auth.clear();
        let child = command.spawn().safe("cannot launch provider in terminal")?;
        self.started = true;
        self.childless = false;
        Ok(child)
    }
    pub fn discard_piped_input(&mut self) {
        if !io::stdin().is_terminal() {
            self.input_open = false;
        }
    }
    fn resize(&self) {
        let mut size = std::mem::MaybeUninit::<libc::winsize>::uninit();
        // SAFETY: ioctl initializes size only on success; the subsequent call
        // reads it only in that branch. No signal handler or global state needed.
        unsafe {
            if libc::ioctl(1, libc::TIOCGWINSZ, size.as_mut_ptr()) == 0 {
                libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, size.as_ptr());
            }
        }
    }
    /// Forward terminal I/O and inspect a bounded trailing slice for limit errors.
    pub fn pump(&mut self, wait_ms: i32) -> Result<bool> {
        self.pump_inner(wait_ms, !self.childless, None)
            .map(|(limits, _)| limits.any())
    }
    /// Recheck liveness after polling and before forwarding newly read input:
    /// the child can exit while poll waits for a keystroke.
    pub fn pump_child(&mut self, wait_ms: i32, child: &mut Child) -> Result<bool> {
        self.pump_child_signals(wait_ms, child)
            .map(ScreenLimits::any)
    }
    pub(crate) fn pump_child_signals(
        &mut self,
        wait_ms: i32,
        child: &mut Child,
    ) -> Result<ScreenLimits> {
        self.pump_inner(wait_ms, !self.childless, Some(child))
            .map(|(limits, _)| limits)
    }
    pub fn child_exited(&mut self) -> Result<bool> {
        self.child_exit_signals().map(ScreenLimits::any)
    }
    pub(crate) fn child_exit_signals(&mut self) -> Result<ScreenLimits> {
        self.childless = true;
        let mut limits = ScreenLimits::default();
        // The slave stays open for reuse. Drain queued output before treating
        // the final unterminated row as complete. Bound the drain in case a
        // descendant still holds the slave and keeps writing after child exit.
        for _ in 0..16 {
            let (observed, more_output) = self.pump_inner(0, false, None)?;
            limits.merge(observed);
            if !more_output {
                let signals = self.diagnostics.finish();
                limits.merge(self.apply_signals(signals));
                break;
            }
        }
        Ok(limits)
    }
    /// Keep cancellation responsive while no provider exists. Typed input
    /// must not queue up in the PTY and replay when the session resumes.
    pub fn wait_for_restart(&mut self, wait_ms: i32) -> Result<()> {
        self.childless = true;
        self.pump_inner(wait_ms, false, None).map(|_| ())
    }

    pub(crate) fn input_activity(&self) -> InputActivity {
        let mut input = libc::pollfd {
            fd: if self.input_open { 0 } else { -1 },
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one initialized pollfd and no wait.
        let ready = unsafe { libc::poll(&mut input, 1, 0) };
        InputActivity {
            last_forwarded: self.last_input,
            waiting: ready < 0 || input.revents & libc::POLLIN != 0,
        }
    }

    // The second result indicates that the PTY may still have output to read.
    fn pump_inner(
        &mut self,
        wait_ms: i32,
        mut forward_input: bool,
        child: Option<&mut Child>,
    ) -> Result<(ScreenLimits, bool)> {
        if self
            .interrupt
            .flag
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return Err(Error("provider session interrupted"));
        }
        self.resize();
        let mut fds = [
            libc::pollfd {
                fd: self.master.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: if self.input_open { 0 } else { -1 },
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: fds points to two initialized pollfd structs for this call.
        let count = unsafe { libc::poll(fds.as_mut_ptr(), 2, wait_ms) };
        if count < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                return Ok((ScreenLimits::default(), true));
            }
            return Err(Error("provider terminal polling failed"));
        }
        let mut buffer = [0u8; 8192];
        if fds[1].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            let n = io::stdin()
                .read(&mut buffer)
                .safe("cannot read terminal input")?;
            if let Some(child) = child
                && child.try_wait().safe("cannot wait for provider")?.is_some()
            {
                self.childless = true;
                forward_input = false;
            }
            if n == 0 {
                self.input_open = false;
            } else if forward_input {
                self.master
                    .write_all(&buffer[..n])
                    .safe("cannot forward terminal input")?;
                self.last_input = Some(std::time::Instant::now());
            } else if buffer[..n].iter().any(|byte| matches!(byte, 3 | 4)) {
                return Err(Error("provider session interrupted"));
            }
        }
        let mut limited = ScreenLimits::default();
        let mut more_output = false;
        if fds[0].revents & libc::POLLIN != 0 {
            match self.master.read(&mut buffer) {
                Ok(n) => {
                    let mut stdout = io::stdout().lock();
                    stdout
                        .write_all(&buffer[..n])
                        .safe("cannot forward provider output")?;
                    stdout.flush().safe("cannot flush provider output")?;
                    let signals = self.diagnostics.feed(&buffer[..n]);
                    limited = self.apply_signals(signals);
                    more_output = n != 0;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => more_output = true,
                Err(e) if e.raw_os_error() == Some(libc::EIO) => {}
                Err(_) => return Err(Error("cannot read provider terminal")),
            }
        }
        Ok((limited, more_output))
    }

    fn apply_signals(&mut self, (limits, auth, model): (ScreenLimits, bool, bool)) -> ScreenLimits {
        if auth && self.quiet_auth {
            self.held_auth.push(std::time::Instant::now());
        } else {
            self.auth_expired |= auth;
        }
        self.model_unsupported |= model;
        limits
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        if let Some(original) = self.original {
            // SAFETY: original was captured from this stdin before raw mode.
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, &original);
            }
        }
    }
}
// Match provider diagnostic lines, never arbitrary prose in a restored
// conversation. Escape state survives PTY reads (including split CSI codes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ScreenLimits {
    pub(crate) notice: bool,
    pub(crate) claude_compaction: bool,
    pub(crate) claude_menu: bool,
}
impl ScreenLimits {
    pub(crate) fn any(self) -> bool {
        self.notice || self.claude_compaction || self.claude_menu
    }

    pub(crate) fn merge(&mut self, next: Self) {
        self.notice |= next.notice;
        self.claude_compaction |= next.claude_compaction;
        self.claude_menu |= next.claude_menu;
    }
}

#[derive(Default)]
struct Diagnostics {
    line: Vec<u8>,
    escape: u8,
    truncated: bool,
    menu_header: bool,
    codex: bool,
    agy: bool,
}
impl Diagnostics {
    fn feed(&mut self, bytes: &[u8]) -> (ScreenLimits, bool, bool) {
        let mut signals = (ScreenLimits::default(), false, false);
        for &byte in bytes {
            match self.escape {
                1 => {
                    self.escape = match byte {
                        b'[' => 2,
                        b']' => 3,
                        _ => 0,
                    };
                    continue;
                }
                2 => {
                    if (0x40..=0x7e).contains(&byte) {
                        self.escape = 0;
                    }
                    continue;
                }
                3 => {
                    if byte == 7 {
                        self.escape = 0;
                    } else if byte == 0x1b {
                        self.escape = 4;
                    }
                    continue;
                }
                4 => {
                    self.escape = if byte == b'\\' { 0 } else { 3 };
                    continue;
                }
                _ => {}
            }
            match byte {
                0x1b => self.escape = 1,
                b'\n' | b'\r' => {
                    self.finish_row(&mut signals);
                }
                _ => {
                    // Preserve UTF-8 bytes until the diagnostic is inspected.
                    if self.line.len() < 4096 {
                        self.line.push(byte);
                    } else {
                        self.truncated = true;
                    }
                }
            }
        }
        signals
    }

    fn finish(&mut self) -> (ScreenLimits, bool, bool) {
        let mut signals = (ScreenLimits::default(), false, false);
        self.finish_row(&mut signals);
        signals
    }

    fn finish_row(&mut self, signals: &mut (ScreenLimits, bool, bool)) {
        if self.truncated {
            self.menu_header = false;
            self.line.clear();
            self.truncated = false;
            return;
        }
        let decoded = normalize(&self.line);
        self.line.clear();
        let line = decoded.as_str();
        if self.agy {
            // agy's own notices only; Claude's and Codex's wording would
            // misread its tool output.
            signals.0.notice |= crate::agy::limit_notice(line);
            return;
        }
        let (limit, auth, model) = diagnostic_line(line, self.codex);
        signals.0.notice |= limit;
        signals.1 |= auth;
        signals.2 |= model;
        if self.codex {
            return;
        }
        signals.0.claude_compaction |= claude_compaction_limit(line);
        const HEADER: &str = "what do you want to do?";
        let option = line
            .strip_prefix(HEADER)
            .or_else(|| self.menu_header.then_some(line));
        signals.0.claude_menu |= option.is_some_and(claude_limit_option);
        // A CRLF generates an empty row after the CR. Keep the header until
        // the next visible row, including when the option arrives later.
        if !line.is_empty() {
            self.menu_header = line == HEADER;
        }
    }
}
/// Box-drawing and quote characters Claude's dialogs and notices may wrap a
/// diagnostic in; they carry no meaning for the match.
fn normalize(line: &[u8]) -> String {
    let decoded = String::from_utf8_lossy(line)
        .to_lowercase()
        .replace(['\u{2019}', '\u{2018}'], "'");
    let frame =
        |c: char| c.is_whitespace() || c == '\u{23bf}' || ('\u{2500}'..='\u{257f}').contains(&c);
    decoded.trim_matches(frame).to_owned()
}
const CODEX_LIMIT: &str = "you've hit your usage limit";

/// One normalized row: (usage limit, login failure, unsupported model).
/// On a Codex terminal (`on_codex`) only Codex's own notices count as a limit:
/// its "you've hit your usage limit" line and the bare error codes. Claude's
/// rules ("error: 429 ...", "you've hit your weekly limit", ...) would also
/// match tool output Codex draws, such as `└ Error: 429 Too Many Requests`
/// from curl, and switch a healthy account away.
fn diagnostic_line(line: &str, on_codex: bool) -> (bool, bool, bool) {
    // The square is Codex's diagnostic marker. Strip it only for the exact
    // usage error, so other output with a square keeps its existing meaning.
    let marked = line.starts_with('■');
    let codex = line.strip_prefix('■').unwrap_or(line).trim_start();
    let codex = codex
        .strip_prefix("error running remote compact task:")
        .map_or(codex, str::trim_start);
    // A narrow pane wraps Codex's longer wordings: the marked first row
    // starts with the notice and breaks off inside its second sentence
    // (no sentence ends on that row; a finished one is checked whole).
    let wrapped = marked
        && codex
            .strip_prefix(CODEX_LIMIT)
            .and_then(|rest| rest.strip_prefix(". "))
            .is_some_and(|rest| !rest.is_empty() && !rest.contains(". ") && !rest.ends_with('.'));
    if wrapped || codex_limit_notice(codex) {
        return (true, false, false);
    }
    let api = line
        .strip_prefix("api error:")
        .or_else(|| line.strip_prefix("error:"));
    let notice = !line.starts_with(CODEX_LIMIT) && hit_limit_notice(line);
    let standalone = matches!(line, "usage_limit_reached" | "rate_limit_exceeded")
        || (line.starts_with("the ") && unsupported_model_message(line))
        || line.starts_with("oauth token revoked \u{b7} please run /login")
        || line.starts_with("login expired \u{b7} please run /login")
        || notice
        || line.starts_with("you're out of extra usage");
    let Some(text) = api.or(standalone.then_some(line)) else {
        return (false, false, false);
    };
    let limit = if on_codex {
        matches!(line, "usage_limit_reached" | "rate_limit_exceeded")
    } else {
        limit_message(text)
            || text.trim_start().starts_with("429 ")
            || notice
            || line.contains("out of extra usage")
    };
    (
        limit,
        auth_message(text) || text.trim_start().starts_with("401 "),
        unsupported_model_message(text),
    )
}

/// Codex's usage-limit notice in each wording its plans get, as normalize()
/// leaves it: "you've hit your usage limit", then optionally " · resets ..."
/// or one sentence that ends in a retry time ("try again later", "at 4:31
/// pm", "in 2 hours 15 minutes"), led by an upgrade, credits or admin offer.
/// Free plans get only the upgrade ("upgrade to plus to continue using
/// codex ..."). Anything else after the notice is prose, not a notice.
fn codex_limit_notice(line: &str) -> bool {
    let Some(suffix) = line.strip_prefix(CODEX_LIMIT) else {
        return false;
    };
    if matches!(suffix, "" | ".") || suffix.starts_with(" · resets ") {
        return true;
    }
    let Some(sentence) = suffix.strip_prefix(". ") else {
        return false;
    };
    if sentence.starts_with("upgrade to plus to continue using codex") && sentence.ends_with('.') {
        return true;
    }
    const OFFERS: [&str; 4] = [
        "visit https://chatgpt.com/settings/usage to purchase more credits or ",
        "visit https://chatgpt.com/codex/settings/usage to purchase more credits or ",
        "to get more access now, send a request to your admin or ",
        "",
    ];
    // "upgrade to pro (<link>) or ...", "upgrade to pro (<link>), visit ...".
    let offer = match sentence.strip_prefix("upgrade to ") {
        Some(upgrade) => {
            let Some((_, rest)) = upgrade.split_once(')') else {
                return false;
            };
            let rest = rest.strip_prefix(',').unwrap_or(rest).trim_start();
            rest.strip_prefix("or ").unwrap_or(rest)
        }
        None => sentence,
    };
    let Some(retry) = OFFERS.iter().find_map(|o| {
        offer
            .strip_prefix(o)
            .and_then(|r| r.strip_prefix("try again "))
    }) else {
        return false;
    };
    retry_time(retry)
}

/// "later.", "at 4:31 pm.", "at nov 21st, 2026 4:31 pm." or "in 2 hours 15
/// minutes." (also "1 day, 3 hours and 5 minutes").
fn retry_time(retry: &str) -> bool {
    if retry == "later." {
        return true;
    }
    if let Some(at) = retry.strip_prefix("at ") {
        let at = at.strip_suffix('.').unwrap_or(at);
        let mut parts = at.rsplitn(3, ' ');
        let (Some(period), Some(time)) = (parts.next(), parts.next()) else {
            return false;
        };
        return matches!(period, "am" | "pm")
            && chrono::NaiveTime::parse_from_str(&format!("{time} {period}"), "%I:%M %P").is_ok();
    }
    let Some(span) = retry.strip_prefix("in ").and_then(|s| s.strip_suffix('.')) else {
        return false;
    };
    let words: Vec<&str> = span
        .split([' ', ','])
        .filter(|w| !w.is_empty() && *w != "and")
        .collect();
    !words.is_empty()
        && words.len().is_multiple_of(2)
        && words.chunks(2).all(|pair| {
            pair[0].parse::<u64>().is_ok()
                && matches!(
                    pair[1],
                    "day" | "days" | "hour" | "hours" | "minute" | "minutes" | "second" | "seconds"
                )
        })
}

pub(crate) fn claude_compaction_limit(line: &str) -> bool {
    let Some(notice) = line.strip_prefix("prompt is too long · automatic compaction failed: ")
    else {
        return false;
    };
    notice == "you've hit your weekly limit"
        || notice
            .strip_prefix("you've hit your weekly limit · resets ")
            .is_some_and(|reset| !reset.is_empty())
}

fn claude_limit_option(text: &str) -> bool {
    let option = text.trim_start().trim_start_matches('❯').trim_start();
    let Some(rest) = option.strip_prefix("1. stop and wait for limit to reset") else {
        return false;
    };
    matches!(rest.trim(), "" | "2. upgrade")
}
/// Claude's usage-limit notice, in every wording it has used: "You've hit
/// your limit", "… your session limit", "… your 5-hour limit", "… your weekly
/// limit", and model limits such as "… your Opus limit" or "… your Fable
/// weekly limit", each optionally followed by " · resets …". Only the notice
/// itself, at the start of the (normalized) line, never prose that quotes it.
fn hit_limit_notice(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("you've hit your ") else {
        return false;
    };
    let head = rest.split('\u{b7}').next().unwrap_or("").trim();
    let words: Vec<_> = head.split_whitespace().collect();
    !words.is_empty()
        && words.len() <= 4
        && words
            .last()
            .is_some_and(|w| w.trim_end_matches(|c: char| c.is_ascii_punctuation()) == "limit")
}
pub fn limit_message(text: &str) -> bool {
    let text = text.to_lowercase().replace(['\u{2019}', '\u{2018}'], "'");
    [
        "http 429",
        "status 429",
        "429 too many requests",
        "usage_limit_reached",
        "rate_limit_exceeded",
        "rate limit reached",
        "usage limit reached",
    ]
    .iter()
    .any(|needle| text.contains(needle))
        || text
            .match_indices("you've hit your ")
            .any(|(at, _)| hit_limit_notice(&text[at..]))
}
#[cfg(test)]
mod tests {
    fn codex_feed(text: &str) -> super::ScreenLimits {
        let mut detector = super::Diagnostics {
            codex: true,
            ..Default::default()
        };
        let mut seen = detector.feed(text.as_bytes()).0;
        seen.merge(detector.finish().0);
        seen
    }

    #[test]
    fn a_codex_terminal_counts_only_codex_limit_notices() {
        for notice in [
            "\u{25a0} You've hit your usage limit. Try again at Nov 21st, 2026 4:31 PM.\r\n",
            "\u{25a0} You've hit your usage limit\r\n",
            "\u{25a0} Error running remote compact task: You've hit your usage limit. Try again later.\r\n",
            "usage_limit_reached\r\n",
            // Each plan's wording.
            "\u{25a0} You've hit your usage limit. Upgrade to Pro (https://openai.com/chatgpt/pricing) or try again in 2 hours 15 minutes.\r\n",
            "\u{25a0} You've hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro), visit https://chatgpt.com/codex/settings/usage to purchase more credits or try again at 4:31 PM.\r\n",
            "\u{25a0} You've hit your usage limit. Upgrade to Plus to continue using Codex (https://openai.com/chatgpt/pricing).\r\n",
            "\u{25a0} You've hit your usage limit. To get more access now, send a request to your admin or try again in 1 day, 3 hours and 5 minutes.\r\n",
            "\u{25a0} You've hit your usage limit. Try again in 5 hours.\r\n",
            "\u{25a0} You've hit your usage limit. Try again in 1 minute.\r\n",
            // Wrapped onto two rows by a narrow pane.
            "\u{25a0} You've hit your usage limit. Upgrade to Pro (https://openai.com/chatgpt/pricing), visit\r\nhttps://chatgpt.com/codex/settings/usage to purchase more credits or try again at 4:31 PM.\r\n",
        ] {
            assert!(codex_feed(notice).notice, "{notice:?}");
        }
        for prose in [
            "You've hit your usage limit. Try again in a bit, I guess.\r\n",
            "You've hit your usage limit. Upgrade to Pro, they said.\r\n",
            "You've hit your usage limit. Try again in 5 hours\r\n",
            "You've hit your usage limit, apparently. Try again later.\r\n",
        ] {
            assert_eq!(codex_feed(prose), Default::default(), "{prose:?}");
        }
        // Tool output and Claude's wording, as Codex's TUI draws them.
        for other in [
            "  \u{2514} Error: 429 Too Many Requests\r\n",
            "\u{2502} error: http 429 from the test server\r\n",
            "  \u{2514} API Error: 429 {\"error\":{\"type\":\"rate_limit_error\"}}\r\n",
            "Error: rate limit reached for requests\r\n",
            "You've hit your weekly limit \u{b7} resets Nov 20, 3am\r\n",
            "\u{23bf} Prompt is too long \u{b7} automatic compaction failed: You've hit your weekly limit \u{b7} resets tomorrow\r\n",
            "What do you want to do?\r\n1. Stop and wait for limit to reset\r\n",
            "You're out of extra usage\r\n",
        ] {
            assert_eq!(codex_feed(other), Default::default(), "{other:?}");
        }
        // Login and model failures still count on Codex.
        assert!(super::diagnostic_line("error: 401 unauthorized", true).1);
        assert!(super::diagnostic_line("error: model_not_found", true).2);
    }

    /// `diagnostic_line` as it was before Codex terminals were scoped
    /// (main 48195be), for Claude's unchanged behaviour.
    fn before(line: &str) -> (bool, bool, bool) {
        use super::*;
        let codex = line.strip_prefix('\u{25a0}').unwrap_or(line).trim_start();
        let codex = codex
            .strip_prefix("error running remote compact task:")
            .map_or(codex, str::trim_start);
        if codex_limit_notice(codex) {
            return (true, false, false);
        }
        let api = line
            .strip_prefix("api error:")
            .or_else(|| line.strip_prefix("error:"));
        let notice = !line.starts_with(CODEX_LIMIT) && hit_limit_notice(line);
        let standalone = matches!(line, "usage_limit_reached" | "rate_limit_exceeded")
            || (line.starts_with("the ") && unsupported_model_message(line))
            || line.starts_with("oauth token revoked \u{b7} please run /login")
            || line.starts_with("login expired \u{b7} please run /login")
            || notice
            || line.starts_with("you're out of extra usage");
        let Some(text) = api.or(standalone.then_some(line)) else {
            return (false, false, false);
        };
        (
            limit_message(text)
                || text.trim_start().starts_with("429 ")
                || notice
                || line.contains("out of extra usage"),
            auth_message(text) || text.trim_start().starts_with("401 "),
            unsupported_model_message(text),
        )
    }

    #[test]
    fn claude_detection_is_unchanged() {
        let source = include_str!("terminal.rs");
        let lines: Vec<String> = source
            .split('"')
            .skip(1)
            .step_by(2)
            .map(|q| {
                q.replace("\\u{b7}", "\u{b7}")
                    .replace("\\u{2019}", "\u{2019}")
            })
            .collect();
        assert!(lines.len() > 50);
        for line in &lines {
            let line = super::normalize(line.as_bytes());
            assert_eq!(
                super::diagnostic_line(&line, false),
                before(&line),
                "{line}"
            );
        }
    }
    #[test]
    fn limit_signals_do_not_match_arbitrary_numbers() {
        assert!(!super::limit_message("processed 429 items"));
        assert!(super::limit_message("error: http 429"));
        assert!(super::limit_message("usage_limit_reached"));
        assert!(super::limit_message(
            "You've hit your 5-hour limit · resets 8:30pm"
        ));
        assert!(super::limit_message("You've hit your weekly limit"));
        assert!(super::limit_message("You\u{2019}ve hit your session limit"));
        assert!(!super::limit_message("5-hour limit: 90% used"));
        assert!(!super::limit_message(
            "you've hit your stride; no limit in sight today"
        ));
    }

    /// Every wording Claude Code has printed for a usage limit, as recorded
    /// in real transcripts (2.1.3xx) and its changelog, plain and as drawn in
    /// its limit dialog (a bordered box, curly apostrophe).
    #[test]
    fn every_claude_limit_notice_is_recognized() {
        for notice in [
            "You've hit your weekly limit \u{b7} resets Nov 20, 3am (America/New_York)",
            "You've hit your session limit \u{b7} resets 4am (America/New_York)",
            "You've hit your 5-hour limit \u{b7} resets 8:30pm (America/New_York)",
            "You've hit your limit \u{b7} resets tomorrow",
            "You've hit your Fable limit",
            "You've hit your Opus weekly limit \u{b7} resets Nov 21, 9pm",
            "You\u{2019}ve hit your weekly limit \u{b7} resets Nov 20, 3am",
        ] {
            for line in [
                format!("  \u{23bf}  {notice}\r\n"),
                format!("{notice}\n"),
                format!("\u{2502} {notice}   \u{2502}\r\n"),
                format!("\x1b[1m\u{2502}\x1b[0m  {notice}\x1b[2m  \u{2502}\x1b[0m\r\n"),
            ] {
                let mut detector = super::Diagnostics::default();
                assert!(detector.feed(line.as_bytes()).0.notice, "{line:?}");
            }
        }
        for prose in [
            "\u{25cf} You've hit your weekly limit, so I stopped here.\n",
            "\u{276f} why did you've hit your weekly limit show up?\n",
            "  - you've hit your weekly limit when the account is at 100%\n",
            "You've hit your stride, and the session limit is far away\n",
        ] {
            let mut detector = super::Diagnostics::default();
            assert_eq!(
                detector.feed(prose.as_bytes()),
                Default::default(),
                "{prose:?}"
            );
        }
    }
}

/// Explicit provider authentication failures; ordinary mentions of 401 do not match.
pub fn auth_message(text: &str) -> bool {
    [
        "login expired",
        "oauth token revoked",
        "token has expired",
        "token is expired",
        "authentication token expired",
        "invalid authentication token",
        "refresh_token_reused",
        "refresh token has expired",
        "http 401",
        "status 401",
        "401 unauthorized",
    ]
    .iter()
    .any(|needle| text.to_ascii_lowercase().contains(needle))
}
#[cfg(test)]
mod auth_tests {
    #[test]
    fn recognizes_login_failures_without_matching_numbers_or_quota() {
        for text in [
            "Login expired · Please run /login",
            "HTTP 401",
            "refresh_token_reused",
        ] {
            assert!(super::auth_message(text));
        }
        for text in [
            "processed 401 files",
            "usage_limit_reached",
            "login successful",
        ] {
            assert!(!super::auth_message(text));
        }
    }
}

/// The provider rejected a model, rather than exhausting this account's quota.
pub fn unsupported_model_message(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    text.contains("model is not supported when using codex with")
        || text.contains("model_not_found")
        || text.contains("does not have access to model")
}

#[cfg(test)]
mod diagnostic_tests {
    use super::{Diagnostics, ScreenLimits};

    #[test]
    fn recalled_reports_and_prompts_are_not_live_failures() {
        let mut detector = Diagnostics::default();
        for report in [
            "❯ <hc from=\"t7 worker\">All logins failed: login expired (401), HTTP 401.</hc>\r\n",
            "● The resume failed with \"aswap: login expired for claude:synthetic\".\n",
            "  - Profiles: synthetic has expired logins; HTTP 401.\n",
            "❯ You were interrupted by a usage-limit account switch. Continue exactly where you left off.\n",
            "● Discuss API Error: 429 usage_limit_reached and token has expired.\n",
            "❯ aswap: login expired for claude:synthetic; selecting another account\n",
            "❯ ■ You've hit your usage limit. Try again at Nov 21st, 2026 4:31 PM.\n",
            "■ You've hit your usage limit, as someone said earlier.\n",
            "> Error running remote compact task: You've hit your usage limit.\n",
            "● Prompt is too long · automatic compaction failed: You've hit your weekly limit · resets tomorrow\n",
            "❯ What do you want to do? 1. Stop and wait for limit to reset\n",
            "What do you want to do? Here are some ideas.\n1. Stop and wait for limit to reset\n",
            "\"You've hit your usage limit. Try again at Nov 21st, 2026 4:31 PM.\"\n",
        ] {
            for chunk in report.as_bytes().chunks(7) {
                assert_eq!(detector.feed(chunk), Default::default(), "{report}");
            }
        }
    }

    #[test]
    fn raw_exact_diagnostic_replay_is_indistinguishable_from_live_output() {
        // Terminal text carries no provenance. Keep this limitation explicit;
        // anchored shapes reject surrounding prose, not identical replay bytes.
        let bytes = b"API Error: 401 synthetic authentication failure\n";
        let expected = (ScreenLimits::default(), true, false);
        assert_eq!(Diagnostics::default().feed(bytes), expected);
        assert_eq!(Diagnostics::default().feed(bytes), expected);
    }

    #[test]
    fn recognizes_ansi_chunked_provider_diagnostics() {
        for (line, expected) in [
            (
                "  ⎿  API Error: 401 {\"error\":{\"type\":\"authentication_error\"}} · Please run /login\r\n",
                (false, true, false),
            ),
            (
                "API Error: 429 {\"error\":{\"type\":\"rate_limit_error\"}}\r\n",
                (true, false, false),
            ),
            (
                "Login expired · Please run /login\r\n",
                (false, true, false),
            ),
            (
                "You've hit your limit · resets tomorrow\r\n",
                (true, false, false),
            ),
            (
                "You've hit your 5-hour limit · resets tomorrow\r\n",
                (true, false, false),
            ),
            (
                "You've hit your weekly limit · resets tomorrow\r\n",
                (true, false, false),
            ),
            ("Error: model_not_found\r\n", (false, false, true)),
            ("■ You've hit your usage limit\r\n", (true, false, false)),
            (
                "■ You've hit your usage limit. Try again later.\r\n",
                (true, false, false),
            ),
            (
                "■ You've hit your usage limit. Try again at 4:31 PM.\r\n",
                (true, false, false),
            ),
            (
                "You've hit your usage limit. Try again at Nov 21st, 2026 4:31 PM.\r\n",
                (true, false, false),
            ),
            (
                "■ You've hit your usage limit. Visit https://chatgpt.com/settings/usage to purchase more credits or try again at Nov 21st, 2026 4:31 PM.\r\n",
                (true, false, false),
            ),
            (
                "■ Error running remote compact task: You've hit your usage limit. Visit https://chatgpt.com/settings/usage to purchase more credits or try again at Nov 21st, 2026 4:31 PM.\r\n",
                (true, false, false),
            ),
            (
                "\x1b]7501;state=blocked;kind=auth\x07  ⎿ OAuth token revoked · Please run /login\r\n",
                (false, true, false),
            ),
            (
                "\x1b]7501;state=blocked;kind=auth\x1b\\  ⎿ OAuth token revoked · Please run /login\r\n",
                (false, true, false),
            ),
        ] {
            let input = format!("\x1b[31m{line}\x1b[0m");
            let mut detector = Diagnostics::default();
            let mut actual = (false, false, false);
            for byte in input.as_bytes() {
                let next = detector.feed(&[*byte]);
                actual.0 |= next.0.notice;
                actual.1 |= next.1;
                actual.2 |= next.2;
            }
            assert_eq!(actual, expected, "{line}");
            assert_eq!(detector.feed(b"ordinary output\n"), Default::default());
        }
    }

    #[test]
    fn quoted_diagnostics_do_not_match_when_reads_split_at_a_valid_prefix() {
        for (header, prefix, suffix) in [
            (
                "",
                "\x1b[31m■ You've hit your usage limit. Try again later.\x1b[0m",
                " This sentence quotes an old error.\r\n",
            ),
            (
                "What do you want to do?\n",
                "1. Stop and wait for limit to reset",
                " is a quoted example, then continue working.\n",
            ),
        ] {
            let whole = format!("{header}{prefix}{suffix}");
            assert!(!Diagnostics::default().feed(whole.as_bytes()).0.any());
            let mut split = Diagnostics::default();
            assert!(!split.feed(header.as_bytes()).0.any());
            assert!(!split.feed(prefix.as_bytes()).0.any());
            assert!(!split.feed(suffix.as_bytes()).0.any());
        }
    }

    #[test]
    fn an_unterminated_diagnostic_counts_only_after_output_is_finished() {
        let mut detector = Diagnostics::default();
        let notice = "■ You've hit your usage limit. Try again later.";
        assert!(!detector.feed(notice.as_bytes()).0.any());
        assert!(detector.finish().0.notice);
        assert!(!detector.finish().0.any());
    }

    #[test]
    fn claude_compaction_and_choice_menu_are_distinct_from_a_plain_limit() {
        let mut detector = Diagnostics::default();
        let message = "⎿ Prompt is too long · automatic compaction failed: You’ve hit your weekly limit · resets Nov 22, 9am (America/New_York) · /clear to start fresh";
        let input = format!("\x1b[31m{message}\x1b[0m\r\n");
        let mut screen = ScreenLimits::default();
        for chunk in input.as_bytes().chunks(2) {
            screen.merge(detector.feed(chunk).0);
        }
        assert!(screen.claude_compaction);
        assert!(!screen.notice && !screen.claude_menu);
        assert!(!detector.feed(b"What do you want to do?\r\n").0.any());
        assert!(
            detector
                .feed("│ ❯ 1. Stop and wait for limit to reset │\n".as_bytes())
                .0
                .claude_menu
        );
        assert!(
            !detector
                .feed(b"1. Stop and wait for limit to reset\n")
                .0
                .any()
        );
        assert!(
            detector
                .feed("│ What do you want to do?  1. Stop and wait for limit to reset  2. Upgrade │\n".as_bytes())
                .0
                .claude_menu
        );
    }
}
