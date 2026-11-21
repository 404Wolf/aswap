//! The owner push stream: the follower watcher and the hidden owner server.
use super::*;

/// Request protocol of the push stream (`sync --serve --watch`): the owner
/// answers with newline-separated protocol-1 packets, one at start, one after
/// every profile auth change and a heartbeat every `HEARTBEAT`.
pub(super) const WATCH_PROTOCOL: u8 = 2;
pub(super) const HEARTBEAT: Duration = Duration::from_secs(20);
/// A stream that stays silent for this long (three heartbeats) is dead.
pub(super) const STREAM_IDLE: Duration = Duration::from_secs(60);
pub(super) const PACKET_LIMIT: usize = 4 * 1024 * 1024;
pub(super) enum Frame {
    Line(Vec<u8>),
    End,
    Bad,
}
/// Read one owner stream. Each packet goes to `on_packet` as it arrives;
/// `stop` ends the stream early. Returns how many packets were accepted and
/// how the stream ended. The child is always killed and reaped.
pub(super) fn stream_command(
    mut command: Command,
    request: &Request,
    first: Duration,
    idle: Duration,
    mut on_packet: impl FnMut(Packet) -> Result<()>,
    mut stop: impl FnMut() -> bool,
) -> (usize, Result<()>) {
    // SAFETY: prctl is async-signal-safe. The stream outlives no watcher: if
    // this process dies, the SSH child (and its pipes) die with it. Without
    // Linux's parent-death signal (macOS), a killed watcher's SSH child ends
    // only when the owner sees its stdin close and ends the stream.
    #[cfg(target_os = "linux")]
    unsafe {
        command.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = match command
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return (0, Err(Error("sync: ssh-start-failed"))),
    };
    let mut accepted = 0;
    let result = (|| {
        // Keep stdin open: the owner ends its stream when it reaches EOF.
        let mut input = child
            .stdin
            .take()
            .ok_or(Error("sync: request-write-failed"))?;
        serde_json::to_writer(&mut input, request).safe("sync: request-write-failed")?;
        input.write_all(b"\n").safe("sync: request-write-failed")?;
        input.flush().safe("sync: request-write-failed")?;
        let output = child.stdout.take().ok_or(Error("sync: bad-response"))?;
        let err_rx = stderr_filter(child.stderr.take().ok_or(Error("sync: bad-response"))?);
        // Bounded: the reader waits for each packet to be installed.
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(output);
            loop {
                let mut line = Vec::new();
                let frame = match reader
                    .by_ref()
                    .take(PACKET_LIMIT as u64 + 2)
                    .read_until(b'\n', &mut line)
                {
                    Ok(0) => Frame::End,
                    Ok(_) if line.len() > PACKET_LIMIT + 1 || line.last() != Some(&b'\n') => {
                        Frame::Bad
                    }
                    Ok(_) => Frame::Line(line),
                    Err(_) => Frame::Bad,
                };
                let last = !matches!(frame, Frame::Line(_));
                if tx.send(frame).is_err() || last {
                    return;
                }
            }
        });
        let mut heard = Instant::now();
        loop {
            if stop() {
                return Ok(());
            }
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(Frame::Line(bytes)) => {
                    let packet: Packet = serde_json::from_slice(&bytes)
                        .safe("sync: bad-response; invalid protocol JSON")?;
                    on_packet(packet)?;
                    accepted += 1;
                    heard = Instant::now();
                }
                Ok(Frame::Bad) => return Err(Error("sync: bad-response; invalid stream frame")),
                Ok(Frame::End) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    let deadline = Instant::now() + Duration::from_secs(1);
                    while Instant::now() < deadline {
                        if let Some(status) = child.try_wait().safe("sync: ssh-wait-failed")? {
                            if !status.success() {
                                return Err(remote_error(
                                    status,
                                    err_rx
                                        .recv_timeout(Duration::from_secs(1))
                                        .unwrap_or("unavailable"),
                                ));
                            }
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(25));
                    }
                    return Err(Error("sync: stream-ended"));
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if accepted == 0 && heard.elapsed() >= first {
                        return Err(Error(NO_STREAM));
                    }
                    if heard.elapsed() >= idle {
                        return Err(Error("sync: timeout"));
                    }
                }
            }
        }
    })();
    // SAFETY: this child was created as the owner of a private process group.
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
    let _ = child.wait();
    (accepted, result)
}
/// An owner that sends nothing this long after connecting cannot stream
/// (an older release waits for stdin EOF that never comes).
/// A current owner under load can take seconds to build its first packet
/// (it reads every profile's usage cache); don't mistake that for an old one.
pub(super) const FIRST_PACKET: Duration = Duration::from_secs(30);
pub(super) const NO_STREAM: &str = "sync: no-stream; owner sent no first packet";
/// One follower connection to the owner's push stream. It has its own
/// schedule (`watch_next`, `watch_failures`) and never touches the poll's
/// `last_attempt`/`failures`, so the 30s poll fallback keeps its cadence
/// however the stream fares. Packets are installed under the store lock as
/// they arrive; the lock is never held while waiting on the network. A
/// delivered packet counts as a sync (last_sync, last_issued, poll failures).
pub(super) fn watch_session(
    store: &Store,
    command: impl FnOnce(&str) -> Result<Command>,
    first: Duration,
    idle: Duration,
) -> Result<Option<usize>> {
    let host = {
        let _lock = store.lock()?;
        let mut settings = settings(store)?;
        let Some(host) = settings.host.clone() else {
            return Ok(None);
        };
        if now() < settings.watch_next {
            return Ok(None);
        }
        // A flapping owner gets at most one new stream per poll interval.
        settings.watch_next = now().saturating_add(30);
        store::json_write(&store.root.join("coordination.json"), &settings)?;
        log(store, "stream", "connect", None, None);
        host
    };
    let moved = || settings(store).map_or(true, |s| s.host.as_deref() != Some(host.as_str()));
    let mut refused = false;
    let (accepted, result) = match command(&host) {
        Ok(command) => stream_command(
            command,
            &Request::new(WATCH_PROTOCOL, None),
            first,
            idle,
            |packet| {
                let _lock = store.lock()?;
                if moved() {
                    return Err(Error("sync: configuration changed during sync"));
                }
                accept(store, packet)
                    .map(|done| {
                        // Heartbeats every 20s would flood the log: record
                        // only packets that changed something.
                        if done.event != "unchanged" {
                            log(store, "stream", done.event, None, Some(done.count));
                        }
                    })
                    .inspect_err(|_| refused = true)
            },
            moved,
        ),
        Err(err) => (0, Err(err)),
    };
    let _lock = store.lock()?;
    if moved() {
        return Ok(Some(accepted));
    }
    let mut settings = settings(store)?;
    match result {
        Err(err) if accepted == 0 || refused => {
            settings.watch_failures = settings.watch_failures.saturating_add(1);
            let delay = if err.0 == NO_STREAM {
                600
            } else {
                (60u64 << settings.watch_failures.min(4)).min(600)
            };
            settings.watch_next = settings.watch_next.max(now().saturating_add(delay));
            store::json_write(&store.root.join("coordination.json"), &settings)?;
            log(store, "stream", "fail", Some(code(&err)), Some(accepted));
            Err(err)
        }
        result => {
            settings.watch_failures = 0;
            store::json_write(&store.root.join("coordination.json"), &settings)?;
            let code = result.err().map(|e| code(&e));
            log(store, "stream", "end", code, Some(accepted));
            Ok(Some(accepted))
        }
    }
}
/// Keep the owner's push stream open for this follower store until it stops
/// being a follower. Only one process per store streams; the others return
/// at once and rely on the 30s poll, which stays the fallback.
pub fn watch(store: &Store) -> Result<()> {
    let Some(_watching) = store.watch_lock()? else {
        return Ok(());
    };
    while follower(store)? {
        // Stay quiet: this runs beside a provider's terminal UI; the stream
        // schedules its own retries in coordination.json.
        let _ = watch_session(
            store,
            |host| ssh(host, "aswap sync --serve --watch"),
            FIRST_PACKET,
            STREAM_IDLE,
        );
        std::thread::sleep(Duration::from_secs(1));
    }
    Ok(())
}
/// Hidden SSH-only transport; refuse credential output to a terminal or a local
/// invocation. Normal `sync --json` returns counts/role, never this packet.
pub fn serve(store: &Store, watch: bool) -> Result<()> {
    if std::io::stdout().is_terminal()
        || std::io::stdin().is_terminal()
        || std::env::var_os("SSH_CONNECTION").is_none()
    {
        return Err(Error("sync transport is restricted to an SSH pipe"));
    }
    if watch {
        return serve_watch(store);
    }
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(8193)
        .read_to_end(&mut bytes)
        .safe("cannot read sync request")?;
    if bytes.len() > 8192 {
        return Err(Error("sync request too large"));
    }
    let request: Request = serde_json::from_slice(&bytes).safe("sync: invalid-request")?;
    let packet = packet(store, &request).map_err(owner_packet_error)?;
    let mut output = std::io::stdout().lock();
    serde_json::to_writer(&mut output, &packet).safe("cannot send owner packet")?;
    output.flush().safe("cannot send owner packet")
}
pub(super) fn owner_packet_error(e: Error) -> Error {
    if e.0.starts_with("sync:") {
        e
    } else {
        Error("sync: owner-store-error")
    }
}
/// What a follower's packet depends on: the account list and every
/// profile's auth file. Stats only; no store lock and no file contents.
pub(super) fn auth_print(store: &Store) -> Vec<Option<crate::shared::Stamp>> {
    let mut print = vec![crate::shared::stamp(&store.root.join("state.json"))];
    for account in store.load().map(|s| s.accounts).unwrap_or_default() {
        print.push(
            store
                .profile(account.provider, &account.label)
                .ok()
                .and_then(|p| crate::shared::stamp(&p.join(account.provider.auth_file()))),
        );
    }
    print
}
/// The push stream: one packet per line at start, within a tick of any
/// profile auth change (a refresh revokes the old access token at once), and
/// a heartbeat every `HEARTBEAT`. Ends when the follower closes stdin or the
/// pipe breaks.
pub(super) fn serve_watch(store: &Store) -> Result<()> {
    let mut input = BufReader::new(std::io::stdin());
    let mut line = Vec::new();
    input
        .by_ref()
        .take(8193)
        .read_until(b'\n', &mut line)
        .safe("cannot read sync request")?;
    if line.len() > 8192 {
        return Err(Error("sync request too large"));
    }
    let request: Request = serde_json::from_slice(&line).safe("sync: invalid-request")?;
    if request.protocol != WATCH_PROTOCOL || request.force.is_some() {
        return Err(Error("sync: version-mismatch"));
    }
    let closed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let closed = closed.clone();
        std::thread::spawn(move || {
            let _ = std::io::copy(&mut input, &mut std::io::sink());
            closed.store(true, std::sync::atomic::Ordering::SeqCst);
        });
    }
    let single = Request {
        protocol: 1,
        force: None,
        providers: request.providers,
    };
    let mut sent: Option<(Vec<Option<crate::shared::Stamp>>, Instant)> = None;
    while !closed.load(std::sync::atomic::Ordering::SeqCst) {
        // Stamp before building: a change during the build sends again.
        let print = auth_print(store);
        if sent
            .as_ref()
            .is_none_or(|(last, at)| *last != print || at.elapsed() >= HEARTBEAT)
        {
            let packet = packet(store, &single).map_err(owner_packet_error)?;
            let mut bytes = serde_json::to_vec(&packet).safe("cannot send owner packet")?;
            drop(packet);
            bytes.push(b'\n');
            let mut output = std::io::stdout().lock();
            match output.write_all(&bytes).and_then(|_| output.flush()) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => return Ok(()),
                Err(_) => return Err(Error("cannot send owner packet")),
            }
            sent = Some((print, Instant::now()));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Ok(())
}
