//! Bounded subprocess execution and external tool discovery.
use super::{IMPORT_BUDGET, TOOL_MAX_STDOUT, TOOL_TIMEOUT, ToolDiscovery, Tools};
use crate::{Error, Ocr, Result};
use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

pub(super) fn duration_env(name: &str, default: Duration) -> Duration {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(default)
}

// Nonblocking sockets avoid reader threads remaining blocked when a tool's
// descendants inherit stdout/stderr. Both streams and every poll are bounded.
#[cfg(unix)]
pub(super) fn run_tool(cmd: &mut Command, timeout: Duration, max_stdout: usize) -> Result<Output> {
    use std::os::{
        fd::OwnedFd,
        unix::{net::UnixStream, process::CommandExt},
    };
    // Keep Error::Io reserved for spawn errors so only a failed spawn can
    // establish tool absence during discovery.
    fn tool_io(error: std::io::Error) -> Error {
        Error::Extraction(format!("tool I/O: {error}"))
    }
    fn drain(stream: &mut UnixStream, bytes: &mut Vec<u8>, cap: usize) -> Result<()> {
        let mut buffer = [0; 8192];
        // Bound each turn so a continuously writing child cannot defeat timeout.
        for _ in 0..32 {
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    if n > cap.saturating_sub(bytes.len()) {
                        return Err(Error::Extraction("output-too-large".into()));
                    }
                    bytes.extend_from_slice(&buffer[..n]);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(tool_io(e)),
            }
        }
        Ok(())
    }
    if timeout.is_zero() {
        return Err(Error::Extraction("tool-timeout".into()));
    }
    let (mut stdout, child_stdout) = UnixStream::pair().map_err(tool_io)?;
    let (mut stderr, child_stderr) = UnixStream::pair().map_err(tool_io)?;
    stdout.set_nonblocking(true).map_err(tool_io)?;
    stderr.set_nonblocking(true).map_err(tool_io)?;
    let started = Instant::now();
    let mut child = cmd
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::from(OwnedFd::from(child_stdout)))
        .stderr(Stdio::from(OwnedFd::from(child_stderr)))
        .spawn()?;
    let mut out = Vec::new();
    let mut err = Vec::new();
    let result = (|| {
        loop {
            drain(&mut stdout, &mut out, max_stdout)?;
            drain(&mut stderr, &mut err, max_stdout)?;
            if started.elapsed() >= timeout {
                return Err(Error::Extraction("tool-timeout".into()));
            }
            if let Some(status) = child.try_wait().map_err(tool_io)? {
                // Drain everything already queued; descendants cannot hold us open.
                loop {
                    let before = out.len() + err.len();
                    drain(&mut stdout, &mut out, max_stdout)?;
                    drain(&mut stderr, &mut err, max_stdout)?;
                    if before == out.len() + err.len() {
                        break;
                    }
                    if started.elapsed() >= timeout {
                        return Err(Error::Extraction("tool-timeout".into()));
                    }
                }
                return Ok(Output {
                    status,
                    stdout: out,
                    stderr: err,
                });
            }
            std::thread::sleep(
                Duration::from_millis(20).min(timeout.saturating_sub(started.elapsed())),
            );
        }
    })();
    if result.is_err() {
        // A wrapper may have forked children that inherited the output handles.
        // Kill the group before reaping its leader, then fall back to direct kill.
        let _ = Command::new("/bin/kill")
            .args(["-KILL", "--", &format!("-{}", child.id())])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

// Job termination closes inherited writers before bounded reader completion.
#[cfg(windows)]
pub(super) fn run_tool(cmd: &mut Command, timeout: Duration, max_stdout: usize) -> Result<Output> {
    use std::sync::mpsc;
    fn reader(
        mut stream: impl Read + Send + 'static,
        cap: usize,
    ) -> (mpsc::Receiver<Result<Vec<u8>>>, std::thread::JoinHandle<()>) {
        let (send, receive) = mpsc::sync_channel(1);
        let handle = std::thread::spawn(move || {
            let result = (|| {
                let mut bytes = Vec::new();
                let mut buffer = [0; 8192];
                loop {
                    match stream.read(&mut buffer) {
                        Ok(0) => return Ok(bytes),
                        Ok(n) => {
                            if n > cap.saturating_sub(bytes.len()) {
                                return Err(Error::Extraction("output-too-large".into()));
                            }
                            bytes.extend_from_slice(&buffer[..n]);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(e) => return Err(Error::Extraction(format!("tool I/O: {e}"))),
                    }
                }
            })();
            let _ = send.send(result);
        });
        (receive, handle)
    }
    fn poll(receive: &mpsc::Receiver<Result<Vec<u8>>>, bytes: &mut Option<Vec<u8>>) -> Result<()> {
        if bytes.is_none() {
            match receive.try_recv() {
                Ok(result) => *bytes = Some(result?),
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(Error::Extraction("tool reader stopped".into()));
                }
            }
        }
        Ok(())
    }
    if timeout.is_zero() {
        return Err(Error::Extraction("tool-timeout".into()));
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let started = Instant::now();
    let (mut child, job) = wi_sys::process::spawn_in_job(cmd)?;
    let (stdout, out_thread) = reader(
        child
            .stdout
            .take()
            .ok_or_else(|| Error::Extraction("tool stdout unavailable".into()))?,
        max_stdout,
    );
    let (stderr, err_thread) = reader(
        child
            .stderr
            .take()
            .ok_or_else(|| Error::Extraction("tool stderr unavailable".into()))?,
        max_stdout,
    );
    let (mut out, mut err) = (None, None);
    let result = (|| {
        loop {
            poll(&stdout, &mut out)?;
            poll(&stderr, &mut err)?;
            if started.elapsed() >= timeout {
                return Err(Error::Extraction("tool-timeout".into()));
            }
            if let Some(status) = child
                .try_wait()
                .map_err(|e| Error::Extraction(format!("tool I/O: {e}")))?
            {
                return Ok(status);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    })();
    // Also kill descendants after a successful leader exit: they can hold pipes open.
    let terminated = job.terminate(1);
    drop(job);
    let deadline = Instant::now() + Duration::from_secs(2);
    let finish = |receive: mpsc::Receiver<Result<Vec<u8>>>,
                  handle: std::thread::JoinHandle<()>,
                  bytes: Option<Vec<u8>>| {
        let result = match bytes {
            Some(bytes) => Ok(bytes),
            None => receive
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .map_err(|_| Error::Extraction("tool reader did not stop".into()))?,
        };
        while !handle.is_finished() {
            if Instant::now() >= deadline {
                return Err(Error::Extraction("tool reader did not stop".into()));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        handle
            .join()
            .map_err(|_| Error::Extraction("tool reader stopped".into()))?;
        result
    };
    let out = finish(stdout, out_thread, out);
    let err = finish(stderr, err_thread, err);
    let status = result?;
    terminated.map_err(|e| Error::Extraction(format!("tool termination failed: {e}")))?;
    Ok(Output {
        status,
        stdout: out?,
        stderr: err?,
    })
}

#[cfg(not(any(unix, windows)))]
pub(super) fn run_tool(
    _cmd: &mut Command,
    _timeout: Duration,
    _max_stdout: usize,
) -> Result<Output> {
    Err(Error::Extraction(
        "unsupported-platform: bounded local tools require Unix".into(),
    ))
}

pub(super) fn tool(cmd: &mut Command, deadline: Instant) -> Result<Output> {
    run_tool(
        cmd,
        duration_env("WI_TOOL_TIMEOUT_MS", TOOL_TIMEOUT)
            .min(deadline.saturating_duration_since(Instant::now())),
        TOOL_MAX_STDOUT,
    )
}

impl ToolDiscovery {
    pub fn as_deref(&self) -> Option<&str> {
        match self {
            Self::Present(version) => Some(version),
            Self::Absent | Self::Unknown(_) => None,
        }
    }
}
impl Tools {
    pub fn discover(pdf: bool, ocr: Ocr, started: Instant) -> Self {
        let deadline =
            started + duration_env("WI_IMPORT_BUDGET_MS", IMPORT_BUDGET).min(IMPORT_BUDGET);
        let mut diagnostics = Vec::new();
        let mut discover = |name, flag, enabled: bool| {
            if !enabled {
                return ToolDiscovery::Absent;
            }
            let result = tool_version(name, flag, deadline);
            if let ToolDiscovery::Unknown(cause) = &result {
                diagnostics.push(format!("{name} discovery: {cause}"));
            }
            result
        };
        let tesseract = discover("tesseract", "--version", ocr != Ocr::Off);
        let pdftotext = discover("pdftotext", "-v", pdf);
        let pdftoppm = discover("pdftoppm", "-v", pdf && ocr != Ocr::Off);
        let language = if tesseract.as_deref().is_some() {
            match tool(Command::new("tesseract").arg("--list-langs"), deadline) {
                Ok(out) if out.status.success() => {
                    if String::from_utf8_lossy(&out.stdout)
                        .lines()
                        .any(|s| s.trim() == "kor")
                    {
                        "kor+eng"
                    } else {
                        "eng"
                    }
                }
                result => {
                    let cause = match result {
                        Ok(out) => format!("abnormal exit: {}", out.status),
                        Err(e) => e.to_string(),
                    };
                    diagnostics.push(format!("tesseract language discovery: {cause}"));
                    "eng"
                }
            }
        } else {
            "eng"
        }
        .to_owned();
        Self {
            pdftotext,
            pdftoppm,
            tesseract,
            language,
            deadline,
            diagnostics,
        }
    }

    pub fn failure_diagnostic(&self, message: &str) -> String {
        std::iter::once(message)
            .chain(self.diagnostics.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join("; ")
    }

    pub fn retryable(&self) -> bool {
        !self.diagnostics.is_empty()
    }
}
fn tool_version(name: &str, flag: &str, deadline: Instant) -> ToolDiscovery {
    let out = match tool(Command::new(name).arg(flag).stdin(Stdio::null()), deadline) {
        Ok(out) => out,
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return ToolDiscovery::Absent;
        }
        Err(e) => return ToolDiscovery::Unknown(e.to_string()),
    };
    if !out.status.success() {
        return ToolDiscovery::Unknown(format!("abnormal exit: {}", out.status));
    }
    let bytes = if out.stdout.is_empty() {
        &out.stderr
    } else {
        &out.stdout
    };
    match String::from_utf8_lossy(bytes)
        .lines()
        .next()
        .filter(|s| !s.trim().is_empty())
    {
        Some(version) => ToolDiscovery::Present(version.to_owned()),
        None => ToolDiscovery::Unknown("empty version output".into()),
    }
}
