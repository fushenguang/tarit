//! Host side of the vsock exec channel.
//!
//! The virtio-vsock device bridges the guest agent's outbound connection (guest
//! -> host CID 2, port 1024) to a per-VM Unix control socket. This module binds
//! that socket, accepts the guest's connection, and runs exec commands over it
//! using the same `VMM_EXEC:` / `VMM_EXEC_EXIT=` marker protocol as serial, but
//! on a dedicated framed stream so exec output never interleaves with the ttyS0
//! console and a connection dropped by a restore is transparently re-accepted.

#![cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]

use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use vmm_sys_util::eventfd::EventFd;

const EXEC_OUTPUT_CAP: usize = 16 * 1024 * 1024;
const EXEC_OUTPUT_TRUNCATED: &[u8] = b"\n[VMM exec output truncated]\n";
const EXEC_OUTPUT_PAYLOAD_CAP: usize = EXEC_OUTPUT_CAP - EXEC_OUTPUT_TRUNCATED.len();
const EXEC_ACC_TAIL_CAP: usize = 64 * 1024;

/// The guest agent reads commands line-by-line. Images built before tarit#38
/// use a fixed 4096-byte line buffer and drop longer lines silently, so an
/// oversized exec only failed after the full transport timeout with nothing to
/// show for it. Refuse such commands up front with an actionable error. Bump
/// this alongside agent images rebaked with the dynamically-grown line buffer
/// (1 MiB hard cap) — serial and vsock share the limit.
pub(crate) const GUEST_AGENT_LINE_LIMIT: usize = 4095;

/// A live exec channel over vsock. Holds the accepted guest connection (if the
/// agent has dialed) and re-accepts on reconnect.
pub struct VsockExecChannel {
    stream: Arc<Mutex<Option<UnixStream>>>,
    stop: Arc<AtomicBool>,
    pump_wake: Option<EventFd>,
    handle: Mutex<Option<JoinHandle<()>>>,
}

/// Why a vsock exec did not return a result. The split matters because exec is
/// not replay-safe: once the command line reached the guest, re-sending it on
/// another channel risks running it twice.
#[derive(Debug)]
pub enum VsockExecError {
    /// The command line was not delivered (the trailing newline never reached
    /// the guest, whose agent discards partial lines on disconnect). Retrying
    /// on serial is safe.
    NotDelivered(String),
    /// The command was (or may have been) delivered but the exchange did not
    /// complete. The guest may still run it; do not re-send.
    Ambiguous(String),
}

impl std::fmt::Display for VsockExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotDelivered(e) => write!(f, "not delivered: {e}"),
            Self::Ambiguous(e) => write!(f, "ambiguous after dispatch: {e}"),
        }
    }
}

/// Internal result of one exec exchange; tells `exec` whether the stream is
/// still framed correctly or must be dropped so the agent re-dials.
#[derive(Debug)]
enum RunExecOutcome {
    /// Clean completion up to `VMM_EXEC_EXIT=`; the stream stays usable.
    Completed((i32, String, u64)),
    /// Deadline passed. When `started`, the guest acknowledged the command and
    /// its exit marker will arrive on a stream we are abandoning.
    TimedOut {
        started: bool,
        output: String,
        duration_ms: u64,
    },
    /// The write failed, so the newline-terminated command line never fully
    /// reached the guest.
    WriteFailed(String),
    /// Read-side failure after the command was delivered.
    TransportFailed(String),
}

impl VsockExecChannel {
    /// Bind `control_socket` and spawn a thread that accepts the guest agent's
    /// connection (the device connects here when the guest dials vsock) and
    /// keeps the newest stream for exec.
    pub fn bind(control_socket: &Path) -> std::io::Result<Arc<Self>> {
        Self::bind_with_pump_wake(control_socket, None)
    }

    /// Like [`Self::bind`], but also wakes the vsock pump after host→guest
    /// writes so commands do not wait for the pump's stop/RX timeout.
    pub fn bind_with_pump_wake(
        control_socket: &Path,
        pump_wake: Option<EventFd>,
    ) -> std::io::Result<Arc<Self>> {
        let _ = std::fs::remove_file(control_socket);
        let listener = UnixListener::bind(control_socket)?;
        listener.set_nonblocking(true)?;

        let stream: Arc<Mutex<Option<UnixStream>>> = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let stream_t = stream.clone();
        let stop_t = stop.clone();

        // The accept thread blocks in poll() until the guest agent connects
        // (0 idle CPU) and wakes immediately on connect — no fixed sleep, so a
        // freshly-booted or freshly-restored guest is picked up with no accept
        // quantization latency. The 250ms timeout only bounds how often we
        // re-check the stop flag. The listener stays non-blocking so accept()
        // never blocks after a spurious wake.
        let listener_fd = listener.as_raw_fd();
        let handle = std::thread::Builder::new()
            .name("vsock-exec-accept".into())
            .spawn(move || {
                while !stop_t.load(Ordering::Relaxed) {
                    let mut pfd = libc::pollfd {
                        fd: listener_fd,
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    // SAFETY: `pfd` points to one initialized pollfd and the
                    // listener fd remains open for the lifetime of this thread.
                    if unsafe { libc::poll(&mut pfd, 1, 250) } <= 0 {
                        continue; // timeout or EINTR -> re-check the stop flag
                    }
                    match listener.accept() {
                        Ok((s, _)) => {
                            log::info!("vsock exec: guest agent connected");
                            // Blocking with a short read timeout for the exec loop.
                            let _ = s.set_nonblocking(false);
                            let _ = s.set_read_timeout(Some(Duration::from_millis(200)));
                            // Newest connection wins (guest re-dials after restore).
                            *stream_t.lock().unwrap_or_else(|e| e.into_inner()) = Some(s);
                        }
                        Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                        Err(_) => std::thread::sleep(Duration::from_millis(50)),
                    }
                }
            })?;

        Ok(Arc::new(Self {
            stream,
            stop,
            pump_wake,
            handle: Mutex::new(Some(handle)),
        }))
    }

    /// True once the guest agent has dialed and a stream is available.
    pub fn is_connected(&self) -> bool {
        self.stream
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// Run `command` over vsock. Returns `None` when no guest connection
    /// exists (caller falls back to serial); `Some(Ok(..))` on completion —
    /// including a guest-side timeout, reported as exit `-1` with partial
    /// output to match serial semantics; `Some(Err(..))` on a transport
    /// failure, with [`VsockExecError`] saying whether a serial retry is safe.
    ///
    /// Any outcome other than clean completion leaves the stream desynced
    /// (late reply bytes could corrupt the next exec), so it is dropped and
    /// the guest agent re-dials; execs in the interim use serial.
    pub fn exec(
        &self,
        command: &str,
        timeout: Duration,
    ) -> Option<Result<(i32, String, u64), VsockExecError>> {
        let mut guard = self.stream.lock().unwrap_or_else(|e| e.into_inner());
        let stream = guard.as_mut()?;
        let outcome = run_exec(stream, command, timeout, self.pump_wake.as_ref());
        let (result, stream_intact) = match outcome {
            RunExecOutcome::Completed(r) => (Ok(r), true),
            RunExecOutcome::TimedOut {
                started: true,
                output,
                duration_ms,
            } => (Ok((-1, output, duration_ms)), false),
            RunExecOutcome::TimedOut { started: false, .. } => (
                Err(VsockExecError::Ambiguous(
                    "timed out before the guest acknowledged the command".into(),
                )),
                false,
            ),
            RunExecOutcome::WriteFailed(e) => (Err(VsockExecError::NotDelivered(e)), false),
            RunExecOutcome::TransportFailed(e) => (Err(VsockExecError::Ambiguous(e)), false),
        };
        if !stream_intact {
            // Drop the connection: the agent sees EOF/EPIPE (possibly after it
            // finishes the in-flight command) and re-dials a fresh stream.
            *guard = None;
        }
        Some(result)
    }
}

impl Drop for VsockExecChannel {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = h.join();
        }
    }
}

/// Send one command and read back its output up to the `VMM_EXEC_EXIT=` marker.
fn run_exec(
    stream: &mut UnixStream,
    command: &str,
    timeout: Duration,
    pump_wake: Option<&EventFd>,
) -> RunExecOutcome {
    let start = Instant::now();
    let msg = format!("VMM_EXEC:{command}\n");
    // tarit#38: the guest agent's line buffer is bounded; a line past it never
    // reaches a shell, so sending it would just burn the exec timeout. The
    // command was not delivered — serial fallback is safe (its pre-check will
    // produce the same fast error).
    if msg.len() > GUEST_AGENT_LINE_LIMIT + 1 {
        return RunExecOutcome::WriteFailed(format!(
            "command line is {} bytes, over the guest agent's {}-byte line \
             limit (see tarit#38); rebuild the image with the updated agent \
             for commands up to 1 MiB, or split the payload into chunks",
            msg.len() - 1,
            GUEST_AGENT_LINE_LIMIT
        ));
    }
    if let Err(e) = stream
        .write_all(msg.as_bytes())
        .and_then(|_| stream.flush())
    {
        return RunExecOutcome::WriteFailed(format!("vsock exec write: {e}"));
    }
    if let Some(evt) = pump_wake {
        let _ = evt.write(1);
    }

    let mut acc: Vec<u8> = Vec::new();
    let mut output: Vec<u8> = Vec::new();
    let mut truncated = false;
    let mut started = false;
    let mut buf = [0u8; 4096];

    while start.elapsed() < timeout {
        match stream.read(&mut buf) {
            Ok(0) => return RunExecOutcome::TransportFailed("vsock exec: peer closed".into()),
            Ok(n) => acc.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                continue
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return RunExecOutcome::TransportFailed(format!("vsock exec read: {e}")),
        }
        while let Some(pos) = acc.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = acc.drain(..=pos).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let s = String::from_utf8_lossy(&line);
            if s == "VMM_AGENT_READY" {
                continue;
            }
            if s == "VMM_EXEC_START" {
                started = true;
                continue;
            }
            if let Some(code) = s.strip_prefix("VMM_EXEC_EXIT=") {
                let exit_code: i32 = code.trim().parse().unwrap_or(0);
                let output_str = finish_exec_output(output, truncated);
                return RunExecOutcome::Completed((
                    exit_code,
                    output_str,
                    start.elapsed().as_millis() as u64,
                ));
            }
            if let Some(reason) = s.strip_prefix("VMM_EXEC_ERROR:") {
                // The agent rejected the line itself (e.g. LINE_TOO_LONG past
                // its 1 MiB hard cap) — the command never entered a shell and
                // no exit marker will follow. Report it as a failed exec
                // instead of waiting out the timeout.
                let output_str = finish_exec_output(output, truncated);
                return RunExecOutcome::Completed((
                    -1,
                    format!("[vmm-agent rejected the command: {reason}]\n{output_str}"),
                    start.elapsed().as_millis() as u64,
                ));
            }
            if started {
                append_exec_output(&mut output, &line, &mut truncated);
                append_exec_output(&mut output, b"\n", &mut truncated);
            }
        }
        trim_exec_accumulator(&mut acc, started, &mut output, &mut truncated);
    }
    RunExecOutcome::TimedOut {
        started,
        output: finish_exec_output(output, truncated),
        duration_ms: start.elapsed().as_millis() as u64,
    }
}

fn append_exec_output(output: &mut Vec<u8>, bytes: &[u8], truncated: &mut bool) {
    if *truncated || bytes.is_empty() {
        return;
    }
    let remaining = EXEC_OUTPUT_PAYLOAD_CAP.saturating_sub(output.len());
    if bytes.len() <= remaining {
        output.extend_from_slice(bytes);
        return;
    }
    output.extend_from_slice(&bytes[..remaining]);
    *truncated = true;
}

fn trim_exec_accumulator(
    acc: &mut Vec<u8>,
    started: bool,
    output: &mut Vec<u8>,
    truncated: &mut bool,
) {
    if acc.len() <= EXEC_ACC_TAIL_CAP {
        return;
    }
    let drain_len = acc.len() - EXEC_ACC_TAIL_CAP;
    let drained: Vec<u8> = acc.drain(..drain_len).collect();
    if started {
        append_exec_output(output, &drained, truncated);
    }
}

fn finish_exec_output(mut output: Vec<u8>, truncated: bool) -> String {
    if truncated {
        output.extend_from_slice(EXEC_OUTPUT_TRUNCATED);
    }
    String::from_utf8_lossy(&output).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive `run_exec` over a socketpair, optionally pre-writing the peer's
    /// response bytes before the call.
    fn exec_over_pair(command: &str, reply: Option<&[u8]>) -> RunExecOutcome {
        let (mut ours, mut peer) = UnixStream::pair().expect("socketpair");
        if let Some(bytes) = reply {
            use std::io::Write as _;
            peer.write_all(bytes).expect("write reply");
        }
        // Mirror the accepted-stream setup in `serve`: blocking reads with a
        // short timeout, so the deadline loop in `run_exec` keeps ticking.
        ours.set_nonblocking(false).expect("blocking");
        ours.set_read_timeout(Some(Duration::from_millis(200)))
            .expect("read timeout");
        run_exec(&mut ours, command, Duration::from_secs(5), None)
    }

    #[test]
    fn oversize_command_is_refused_before_anything_is_sent() {
        // tarit#38: a line past the agent's buffer limit must fail fast with
        // the actionable error, not burn the transport timeout.
        let command = "true".to_string() + &" ".repeat(GUEST_AGENT_LINE_LIMIT);
        let RunExecOutcome::WriteFailed(err) = exec_over_pair(&command, None) else {
            panic!("expected WriteFailed for an oversized command");
        };
        assert!(
            err.contains("guest agent's") && err.contains("line limit"),
            "error should explain the line limit, got: {err}"
        );
    }

    #[test]
    fn command_at_the_limit_is_still_sent() {
        // "VMM_EXEC:" + command + "\n" exactly one byte over the bare limit.
        let command = "x".repeat(GUEST_AGENT_LINE_LIMIT - ("VMM_EXEC:".len() + 1));
        let outcome = exec_over_pair(&command, Some(b"VMM_EXEC_START\nhello\nVMM_EXEC_EXIT=0\n"));
        let RunExecOutcome::Completed((code, output, _)) = outcome else {
            panic!("expected Completed, got {outcome:?}");
        };
        assert_eq!(code, 0);
        assert_eq!(output, "hello\n");
    }

    #[test]
    fn agent_error_marker_fails_the_exec_instead_of_timing_out() {
        // tarit#38: the agent's LINE_TOO_LONG rejection must surface as a
        // completed-with-error exec, not an unexplained timeout.
        let outcome = exec_over_pair("true", Some(b"VMM_EXEC_ERROR:LINE_TOO_LONG\n"));
        let RunExecOutcome::Completed((code, output, _)) = outcome else {
            panic!("expected Completed, got {outcome:?}");
        };
        assert_eq!(code, -1);
        assert!(
            output.contains("vmm-agent rejected the command: LINE_TOO_LONG"),
            "output should carry the rejection, got: {output}"
        );
    }
}
