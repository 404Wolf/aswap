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
    tail: String,
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
            tail: String::new(),
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
        self.tail.clear();
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
                    self.tail
                        .push_str(&String::from_utf8_lossy(&buffer[..n]).to_ascii_lowercase());
                    limited = limit_message(&self.tail);
                    // UTF-8 safe and bounded; this is never logged or persisted.
                    self.tail = self
                        .tail
                        .chars()
                        .rev()
                        .take(512)
                        .collect::<String>()
                        .chars()
                        .rev()
                        .collect();
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
pub fn limit_message(text: &str) -> bool {
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
}
#[cfg(test)]
mod tests {
    #[test]
    fn limit_signals_do_not_match_arbitrary_numbers() {
        assert!(!super::limit_message("processed 429 items"));
        assert!(super::limit_message("error: http 429"));
        assert!(super::limit_message("usage_limit_reached"));
    }
}
