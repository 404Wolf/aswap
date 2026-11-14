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
    diagnostics: Diagnostics,
    pub auth_expired: bool,
    pub model_unsupported: bool,
    /// While set, login errors in the output are not counted: the provider
    /// is redrawing a resumed conversation, not reporting a live failure.
    pub quiet_auth: bool,
}
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
                std::ptr::null(),
                std::ptr::null(),
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
            diagnostics: Diagnostics::default(),
            auth_expired: false,
            model_unsupported: false,
            quiet_auth: false,
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
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        self.diagnostics = Diagnostics::default();
        self.auth_expired = false;
        self.model_unsupported = false;
        self.quiet_auth = false;
        self.resize();
        command.spawn().safe("cannot launch provider in terminal")
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
                return Ok(false);
            }
            return Err(Error("provider terminal polling failed"));
        }
        let mut buffer = [0u8; 8192];
        if fds[1].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            let n = io::stdin()
                .read(&mut buffer)
                .safe("cannot read terminal input")?;
            if n == 0 {
                self.input_open = false;
            } else {
                self.master
                    .write_all(&buffer[..n])
                    .safe("cannot forward terminal input")?;
            }
        }
        let mut limited = false;
        if fds[0].revents & libc::POLLIN != 0 {
            match self.master.read(&mut buffer) {
                Ok(n) => {
                    let mut stdout = io::stdout().lock();
                    stdout
                        .write_all(&buffer[..n])
                        .safe("cannot forward provider output")?;
                    stdout.flush().safe("cannot flush provider output")?;
                    let signals = self.diagnostics.feed(&buffer[..n]);
                    limited = signals.0;
                    self.auth_expired |= signals.1 && !self.quiet_auth;
                    self.model_unsupported |= signals.2;
                }
                Err(e) if e.raw_os_error() == Some(libc::EIO) => {}
                Err(_) => return Err(Error("cannot read provider terminal")),
            }
        }
        Ok(limited)
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
#[derive(Default)]
struct Diagnostics {
    line: Vec<u8>,
    escape: u8,
    truncated: bool,
}
impl Diagnostics {
    fn feed(&mut self, bytes: &[u8]) -> (bool, bool, bool) {
        let mut signals = (false, false, false);
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
                    let next = if self.truncated {
                        (false, false, false)
                    } else {
                        diagnostic_line(&self.line)
                    };
                    signals.0 |= next.0;
                    signals.1 |= next.1;
                    signals.2 |= next.2;
                    self.line.clear();
                    self.truncated = false;
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
        let next = if self.truncated {
            (false, false, false)
        } else {
            diagnostic_line(&self.line)
        };
        (
            signals.0 || next.0,
            signals.1 || next.1,
            signals.2 || next.2,
        )
    }
}
fn diagnostic_line(line: &[u8]) -> (bool, bool, bool) {
    // Decode bytes accumulated across reads, then remove Claude's result marker.
    let decoded = String::from_utf8_lossy(line).to_ascii_lowercase();
    let line = decoded.trim().trim_start_matches('⎿').trim();
    let api = line
        .strip_prefix("api error:")
        .or_else(|| line.strip_prefix("error:"));
    let standalone = matches!(line, "usage_limit_reached" | "rate_limit_exceeded")
        || (line.starts_with("the ") && unsupported_model_message(line))
        || line.starts_with("oauth token revoked · please run /login")
        || line.starts_with("login expired · please run /login")
        || line.starts_with("you've hit your limit")
        || line.starts_with("you've hit your 5-hour limit")
        || line.starts_with("you've hit your weekly limit")
        || line.starts_with("you’re out of extra usage")
        || line.starts_with("you're out of extra usage");
    let Some(text) = api.or(standalone.then_some(line)) else {
        return (false, false, false);
    };
    (
        limit_message(text)
            || text.trim_start().starts_with("429 ")
            || line.starts_with("you've hit your limit")
            || line.contains("out of extra usage"),
        auth_message(text) || text.trim_start().starts_with("401 "),
        unsupported_model_message(text),
    )
}
pub fn limit_message(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    [
        "http 429",
        "status 429",
        "429 too many requests",
        "usage_limit_reached",
        "rate_limit_exceeded",
        "rate limit reached",
        "usage limit reached",
        "you've hit your 5-hour limit",
        "you've hit your weekly limit",
        "you've hit your limit",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}
#[cfg(test)]
mod tests {
    #[test]
    fn limit_signals_do_not_match_arbitrary_numbers() {
        assert!(!super::limit_message("processed 429 items"));
        assert!(super::limit_message("error: http 429"));
        assert!(super::limit_message("usage_limit_reached"));
        assert!(super::limit_message(
            "You've hit your 5-hour limit · resets 8:30pm"
        ));
        assert!(super::limit_message("You've hit your weekly limit"));
        assert!(!super::limit_message("5-hour limit: 90% used"));
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
    use super::Diagnostics;

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
        ] {
            for chunk in report.as_bytes().chunks(7) {
                assert_eq!(detector.feed(chunk), (false, false, false), "{report}");
            }
        }
    }

    #[test]
    fn raw_exact_diagnostic_replay_is_indistinguishable_from_live_output() {
        // Terminal text carries no provenance. Keep this limitation explicit;
        // anchored shapes reject surrounding prose, not identical replay bytes.
        let bytes = b"API Error: 401 synthetic authentication failure\n";
        assert_eq!(Diagnostics::default().feed(bytes), (false, true, false));
        assert_eq!(Diagnostics::default().feed(bytes), (false, true, false));
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
                actual.0 |= next.0;
                actual.1 |= next.1;
                actual.2 |= next.2;
            }
            assert_eq!(actual, expected, "{line}");
            assert_eq!(detector.feed(b"ordinary output\n"), (false, false, false));
        }
    }
}
