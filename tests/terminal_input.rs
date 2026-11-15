use aswap::terminal::Terminal;
use std::{
    fs::{self, File},
    io::{Read, Write},
    os::fd::{AsRawFd, FromRawFd},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Owned(Child);
impl Drop for Owned {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn barrier(path: &std::path::Path) {
    let end = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(
            Instant::now() < end,
            "input barrier timed out: {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn interactive_case(mode: &str) {
    let temp = tempfile::tempdir().unwrap();
    let mut master = -1;
    let mut slave = -1;
    // SAFETY: valid output pointers for newly allocated PTY descriptors.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        },
        0
    );
    // SAFETY: each descriptor is open and transferred to exactly one File.
    let (mut master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
    for file in [&master, &slave] {
        // SAFETY: both file descriptors are open; stdin's dup remains inherited.
        assert_eq!(
            unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) },
            0
        );
    }
    let mut child = Owned(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "terminal_input_child", "--nocapture"])
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("HOME", temp.path())
            .env("XDG_CONFIG_HOME", temp.path().join("config"))
            .env("ASWAP_TEST_DIR", temp.path())
            .env("ASWAP_INPUT_CASE", mode)
            .stdin(Stdio::from(slave))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    barrier(&temp.path().join("exit-gap"));
    if mode == "exit" {
        master.write_all(b"synthetic input after exit\n").unwrap();
    }
    fs::write(temp.path().join("exit-input"), "continue").unwrap();
    barrier(&temp.path().join("spawn-gap"));
    if mode == "spawn" {
        master.write_all(b"synthetic input before spawn\n").unwrap();
    } else if mode == "cancel" {
        master.write_all(&[3]).unwrap();
    }
    fs::write(temp.path().join("spawn-input"), "continue").unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    while child.0.try_wait().unwrap().is_none() {
        assert!(Instant::now() < end, "terminal child never finished");
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut stderr = String::new();
    child
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    let mut stdout = String::new();
    child
        .0
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    assert!(child.0.wait().unwrap().success(), "{stdout}\n{stderr}");
    assert_eq!(temp.path().join("resumed").exists(), mode != "cancel");
}

#[test]
fn interactive_input_in_exit_detection_gap_is_not_replayed() {
    interactive_case("exit");
}
#[test]
fn interactive_input_after_last_wait_pump_is_not_replayed() {
    interactive_case("spawn");
}
#[test]
fn interactive_cancel_after_last_wait_pump_prevents_relaunch() {
    interactive_case("cancel");
}

#[test]
fn terminal_input_child() {
    let Ok(mode) = std::env::var("ASWAP_INPUT_CASE") else {
        return;
    };
    let dir = std::path::PathBuf::from(std::env::var_os("ASWAP_TEST_DIR").unwrap());
    let mut terminal = Terminal::open().unwrap();
    let mut command = Command::new("python3");
    command.args(["-c", "pass"]);
    assert!(
        terminal
            .spawn(&mut command)
            .unwrap()
            .wait()
            .unwrap()
            .success()
    );
    fs::write(dir.join("exit-gap"), "provider exited").unwrap();
    barrier(&dir.join("exit-input"));
    // Exercise the pump before the supervisor detects/reaps the exit.
    terminal.pump(0).unwrap();
    terminal.wait_for_restart(0).unwrap();
    fs::write(dir.join("spawn-gap"), "last wait pump finished").unwrap();
    barrier(&dir.join("spawn-input"));
    let mut command = Command::new("python3");
    command.args([
        "-c",
        r#"
import os,pathlib,select,sys,time
assert not select.select([sys.stdin],[],[],0)[0], 'queued provider input was replayed'
time.sleep(.2)
assert not select.select([sys.stdin],[],[],0)[0], 'waiting outer input was forwarded after spawn'
(pathlib.Path(os.environ['ASWAP_TEST_DIR'])/'resumed').write_text('resumed without old input')
"#,
    ]);
    let result = terminal.spawn(&mut command);
    if mode == "cancel" {
        match result {
            Err(err) => assert_eq!(err.to_string(), "provider session interrupted"),
            Ok(mut child) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("queued Ctrl-C was ignored before spawn");
            }
        }
    } else {
        let mut child = Owned(result.unwrap());
        loop {
            terminal.pump(20).unwrap();
            if let Some(status) = child.0.try_wait().unwrap() {
                terminal.wait_for_restart(0).unwrap();
                assert!(status.success(), "resumed provider consumed old input");
                break;
            }
        }
    }
}
