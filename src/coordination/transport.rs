//! The SSH exchange with the owner and the allowlisted stderr classification.
use super::*;

// Only exact, application-owned messages survive the first-line filter. This is
// deliberately not token-pattern replacement: an unknown diagnostic could contain
// an email, path, token, terminal controls or an entire credential JSON object.
pub(super) fn remote_line(bytes: &[u8]) -> &'static str {
    let Ok(line) = std::str::from_utf8(bytes) else {
        return "redacted-unrecognized";
    };
    let line = line.trim_end_matches(['\r', '\n']);
    match line {
        "aswap: sync server requires the configured refresh owner"
        | "aswap: sync: owner-not-configured; run sync --owner on the server using its service store" => {
            "owner-not-configured"
        }
        "aswap: sync: version-mismatch" => "version-mismatch",
        "aswap: invalid sync request" | "aswap: sync: invalid-request" => "invalid-request",
        "aswap: sync transport is restricted to an SSH pipe" => "transport-restricted",
        "aswap: owner profile identity mismatch; align profile labels first"
        | "aswap: sync: identity-conflict; same label has a different account; align labels first" => {
            "identity-conflict"
        }
        "aswap: duplicate account in owner packet"
        | "aswap: sync: label-conflict; duplicate provider/label in owner packet" => {
            "label-conflict"
        }
        "aswap: cannot read private file"
        | "aswap: invalid private JSON; contents redacted"
        | "aswap: cannot encode private state"
        | "aswap: cannot secure directory"
        | "aswap: cannot create private directory"
        | "aswap: cannot inspect private path"
        | "aswap: private file exceeds size limit"
        | "aswap: expected a regular private file"
        | "aswap: sync: owner-store-error" => "owner-store-error",
        "aswap: refusing a symlink or managed file" => "owner-managed-path",
        "aswap: unsupported or missing credentials; use a file-based provider login first"
        | "aswap: invalid credential JSON; contents redacted" => "owner-auth-invalid",
        "aswap: sync: timeout" => "timeout",
        "aswap: sync: bad-response" => "bad-response",
        "aswap: cannot send owner packet" => "owner-write-failed",
        "aswap: sync request too large" => "request-too-large",
        "aswap: cannot read sync request" => "owner-read-failed",
        _ => "redacted-unrecognized",
    }
}
pub(super) fn remote_error(status: std::process::ExitStatus, first: &'static str) -> Error {
    if let Some(code) = status.code() {
        eprintln!("aswap sync: remote-exit-{code}; remote-stderr-first-line: {first}");
    } else {
        eprintln!("aswap sync: remote-signaled; remote-stderr-first-line: {first}");
    }
    Error(match first {
        "owner-not-configured" => {
            "sync: owner-not-configured; check the SSH user's role/store; run sync --owner there"
        }
        "version-mismatch" => "sync: version-mismatch",
        "identity-conflict" => "sync: identity-conflict",
        "label-conflict" => "sync: label-conflict",
        "invalid-request" => "sync: invalid-request",
        "owner-store-error" => "sync: owner-store-error",
        "owner-managed-path" => "sync: owner-managed-path",
        "owner-auth-invalid" => "sync: owner-auth-invalid",
        "transport-restricted" => "sync: transport-restricted",
        "timeout" => "sync: timeout",
        "bad-response" => "sync: bad-response",
        _ => "sync: remote-exit; see classified remote stderr above",
    })
}
pub(super) fn exchange(host: &str, request: &Request) -> Result<Packet> {
    exchange_command(
        ssh(host, "aswap sync --serve")?,
        request,
        Duration::from_secs(90),
    )
}
pub(super) fn exchange_command(
    mut command: Command,
    request: &Request,
    timeout: Duration,
) -> Result<Packet> {
    let mut child = command
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .safe("sync: ssh-start-failed")?;
    let result = (|| {
        let mut input = child
            .stdin
            .take()
            .ok_or(Error("sync: request-write-failed"))?;
        let sent =
            serde_json::to_writer(&mut input, request).is_ok() && input.write_all(b"\n").is_ok();
        drop(input);
        let output = child.stdout.take().ok_or(Error("sync: bad-response"))?;
        let err_rx = stderr_filter(child.stderr.take().ok_or(Error("sync: bad-response"))?);
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = output.take(4 * 1024 * 1024 + 1).read_to_end(&mut bytes);
            let _ = tx.send((result.is_ok(), bytes));
        });
        let deadline = Instant::now() + timeout;
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
                // Judge the response first: a peer that exits without reading
                // stdin races the request write (EPIPE).
                let (ok, bytes) = rx
                    .recv_timeout(Duration::from_secs(1))
                    .safe("sync: bad-response; incomplete SSH output")?;
                if !ok || bytes.len() > 4 * 1024 * 1024 {
                    return Err(Error("sync: bad-response; invalid response size"));
                }
                // A complete packet is authoritative (protocol checks follow);
                // a failed write only explains a peer that said nothing.
                if bytes.is_empty() && !sent {
                    return Err(Error("sync: request-write-failed"));
                }
                return serde_json::from_slice(&bytes)
                    .safe("sync: bad-response; invalid protocol JSON");
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        Err(Error("sync: timeout"))
    })();
    if result.is_err() || child.try_wait().ok().flatten().is_none() {
        // SAFETY: this child was created as the owner of a private process group.
        // Also stop surviving descendants holding pipes after the SSH child exits.
        unsafe {
            libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
        }
    }
    let _ = child.wait();
    result
}
pub(super) fn ssh(host: &str, remote: &str) -> Result<Command> {
    if !valid_host(host) {
        return Err(Error("invalid sync host alias"));
    }
    let mut command = Command::new("ssh");
    command.args([
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=5",
        "-o",
        "ServerAliveInterval=10",
        "-o",
        "ServerAliveCountMax=2",
        "--",
        host,
        remote,
    ]);
    Ok(command)
}
/// Classify the first stderr line on a helper thread and drain the rest, so
/// verbose SSH/provider errors cannot deadlock the pipe.
pub(super) fn stderr_filter(
    stderr: std::process::ChildStderr,
) -> std::sync::mpsc::Receiver<&'static str> {
    let (err_tx, err_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut first = Vec::new();
        let ok = reader
            .by_ref()
            .take(4097)
            .read_until(b'\n', &mut first)
            .is_ok();
        let filtered = if !ok || first.len() > 4096 {
            "redacted-unrecognized"
        } else if first.is_empty() {
            "empty"
        } else {
            remote_line(&first)
        };
        let _ = err_tx.send(filtered);
        let _ = std::io::copy(&mut reader, &mut std::io::sink());
    });
    err_rx
}
