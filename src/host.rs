//! [DP2.1] Single external process-execution and HTTPS-fetch seam (`Host`).
//!
//! All subprocess spawning (`colab`, `ssh`, `git`, `curl`) goes through this
//! module so timeouts, cooperative cancellation, stdout/stderr streaming, and
//! SSRF checks are enforced uniformly.

use crate::error::{Error, ErrorKind, Result};
use crate::util::{redact_secrets, tail, CancelToken, DiagnosticRedactor};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineAction {
    Continue,
    Kill,
}

#[derive(Debug, Clone)]
pub struct Line {
    pub stream: Stream,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct ExecRequest {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
    pub stdin: Option<Vec<u8>>,
    pub timeout: Option<Duration>,
    pub redact: Vec<String>,
}

impl ExecRequest {
    pub fn new(program: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            args,
            env: Vec::new(),
            cwd: None,
            stdin: None,
            timeout: Some(Duration::from_secs(60)),
            redact: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ExecResult {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub killed: bool,
    pub timed_out: bool,
    pub cancelled: bool,
}

/// External execution host seam.
#[derive(Debug, Clone, Default)]
pub struct Host {
    pub extra_env: Vec<(String, String)>,
    deny_real_exec: bool,
}

impl Host {
    pub fn new() -> Self {
        Self::default()
    }

    /// Integration tests use this host because Cargo links the production library.
    pub fn denied() -> Self {
        Self {
            deny_real_exec: true,
            ..Self::default()
        }
    }

    pub fn permits_external_io(&self) -> bool {
        !cfg!(test) && !self.deny_real_exec
    }

    pub fn require_external_io(&self) -> Result<()> {
        if !self.permits_external_io() {
            return Err(Error::new(
                ErrorKind::Other,
                "real process and network execution is disabled",
            ));
        }
        Ok(())
    }

    /// Resolve once from the process startup environment.
    pub fn discover_colab(&self) -> Option<PathBuf> {
        if let Some(path) = std::env::var_os("COLAB_MCP_COLAB_BIN") {
            return validated_colab(&PathBuf::from(path));
        }
        let mut dirs = Vec::new();
        if let Some(path) = std::env::var_os("PATH") {
            dirs.extend(std::env::split_paths(&path));
        }
        if let Some(home) = std::env::var_os("HOME") {
            dirs.push(PathBuf::from(home).join(".local/bin"));
        }
        dirs.extend(["/opt/homebrew/bin".into(), "/usr/local/bin".into()]);
        dirs.iter()
            .flat_map(|dir| ["colab", "colab.exe"].map(|name| dir.join(name)))
            .find_map(|path| validated_colab(&path))
    }

    pub fn run(
        &self,
        req: ExecRequest,
        cancel: &CancelToken,
        on_line: &mut dyn FnMut(&Line) -> LineAction,
    ) -> Result<ExecResult> {
        self.require_external_io()?;
        cancel.check()?;
        let mut cmd = Command::new(&req.program);
        cmd.args(&req.args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(if req.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            });
        if let Some(dir) = &req.cwd {
            cmd.current_dir(dir);
        }
        for (k, v) in &self.extra_env {
            cmd.env(k, v);
        }
        for (k, v) in &req.env {
            cmd.env(k, v);
        }

        isolate_process_group(&mut cmd);
        let mut child = cmd.spawn().map_err(|e| {
            let kind = if e.kind() == std::io::ErrorKind::NotFound {
                ErrorKind::NotInstalled
            } else {
                ErrorKind::Io
            };
            Error::new(kind, format!("failed to spawn {}: {e}", req.program))
        })?;

        if let (Some(bytes), Some(mut stdin_pipe)) = (req.stdin, child.stdin.take()) {
            thread::spawn(move || {
                let _ = stdin_pipe.write_all(&bytes);
                let _ = stdin_pipe.flush();
            });
        }

        const MAX_STDOUT: usize = 8 * 1024 * 1024;
        const MAX_LINE: u64 = 1024 * 1024;
        const STDERR_TAIL: usize = 16 * 1024;
        let (tx, rx) = mpsc::sync_channel(16);
        fn read_lines<R: Read + Send + 'static>(
            pipe: R,
            stream: Stream,
            tx: mpsc::SyncSender<std::io::Result<Line>>,
        ) {
            thread::spawn(move || {
                let mut reader = BufReader::new(pipe);
                loop {
                    let mut bytes = Vec::new();
                    let line = match reader
                        .by_ref()
                        .take(MAX_LINE + 1)
                        .read_until(b'\n', &mut bytes)
                    {
                        Ok(0) => break,
                        Ok(n) if n as u64 <= MAX_LINE => Ok(Line {
                            stream,
                            text: String::from_utf8_lossy(&bytes).into_owned(),
                        }),
                        Ok(_) => Err(std::io::Error::other("subprocess line exceeded 1 MiB")),
                        Err(e) => Err(e),
                    };
                    let failed = line.is_err();
                    if tx.send(line).is_err() || failed {
                        break;
                    }
                }
            });
        }
        read_lines(child.stdout.take().unwrap(), Stream::Stdout, tx.clone());
        read_lines(child.stderr.take().unwrap(), Stream::Stderr, tx);
        let started = Instant::now();
        let deadline = req.timeout.unwrap_or(Duration::from_secs(120));
        let mut stdout = String::new();
        let mut stderr = String::new();
        let mut stderr_redactor = DiagnosticRedactor::new(&req.redact);
        let mut pipes_closed = false;
        let mut reaped = false;
        let mut exit_status: Option<ExitStatus> = None;

        let result = loop {
            if cancel.is_cancelled() {
                break Err(Error::new(ErrorKind::Cancelled, "command cancelled"));
            }
            if started.elapsed() >= deadline {
                break Err(Error::new(
                    ErrorKind::ExecTimeout,
                    format!(
                        "{} timed out after {}s: {}",
                        req.program,
                        deadline.as_secs(),
                        tail(&stderr, 300),
                    ),
                ));
            }
            if !reaped {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        reaped = true;
                        exit_status = Some(status);
                        if pipes_closed {
                            break Ok((status.code(), false));
                        }
                    }
                    Ok(None) => {}
                    Err(e) => break Err(e.into()),
                }
            } else if pipes_closed {
                break Ok((exit_status.and_then(|s| s.code()), false));
            }
            if pipes_closed {
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            match rx.recv_timeout(Duration::from_millis(10)) {
                Ok(Ok(line)) => {
                    match line.stream {
                        Stream::Stdout => {
                            if stdout.len() + line.text.len() > MAX_STDOUT {
                                break Err(Error::new(
                                    ErrorKind::Io,
                                    "subprocess stdout exceeded 8 MiB",
                                ));
                            }
                            stdout.push_str(&line.text);
                        }
                        Stream::Stderr => {
                            stderr.push_str(&stderr_redactor.line(&line.text));
                            if stderr.len() > STDERR_TAIL {
                                stderr = tail(&stderr, STDERR_TAIL - 4);
                            }
                        }
                    }
                    if on_line(&line) == LineAction::Kill {
                        break Ok((None, true));
                    }
                }
                Ok(Err(e)) => break Err(e.into()),
                Err(mpsc::RecvTimeoutError::Disconnected) => pipes_closed = true,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        };
        if !matches!(result, Ok((_, false))) && !reaped {
            kill_process_group(&mut child);
            thread::spawn(move || {
                let _ = child.wait();
            });
        }
        let (code, killed) = result?;
        Ok(ExecResult {
            code,
            stdout: if req.redact.is_empty() {
                stdout
            } else {
                redact_secrets(&stdout, &req.redact)
            },
            stderr,
            killed,
            ..ExecResult::default()
        })
    }

    /// GET only from approved HF domains. Resolve and pin a public address at every hop.
    pub fn https_get(
        &self,
        url: &str,
        bearer_token: Option<&str>,
        timeout_secs: u64,
    ) -> Result<Vec<u8>> {
        self.require_external_io()?;
        let deadline = Instant::now() + Duration::from_secs(timeout_secs.clamp(2, 60));
        let mut url = url.to_owned();
        for _ in 0..=5 {
            let domain = hf_domain(&url)?;
            let remaining = || deadline.saturating_duration_since(Instant::now());
            let (tx, rx) = mpsc::sync_channel(1);
            thread::spawn(move || {
                let addresses = (domain, 443)
                    .to_socket_addrs()
                    .map(|ips| ips.collect::<Vec<_>>());
                let _ = tx.send(addresses);
            });
            let addresses = rx
                .recv_timeout(remaining())
                .map_err(|_| Error::new(ErrorKind::Http, "Hugging Face DNS timed out"))??;
            let ipv4 = addresses
                .iter()
                .find_map(|addr| match addr.ip() {
                    std::net::IpAddr::V4(ip) if public_ipv4(ip) => Some(ip),
                    _ => None,
                })
                .ok_or_else(|| {
                    Error::new(
                        ErrorKind::Http,
                        "Hugging Face DNS returned no public IPv4 address",
                    )
                })?;
            if addresses
                .iter()
                .any(|addr| matches!(addr.ip(), std::net::IpAddr::V4(ip) if !public_ipv4(ip)))
            {
                return Err(Error::new(
                    ErrorKind::Http,
                    "Hugging Face DNS returned a non-public address",
                ));
            }
            let mut cfg = format!(
                "silent\nshow-error\ninclude\nmax-time = {}\nheader = \"Accept: application/json\"\n",
                remaining().as_secs_f64(),
            );
            if let Some(token) = bearer_token.filter(|_| domain == "huggingface.co") {
                if !token
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._~-".contains(&b))
                {
                    return Err(Error::invalid("token", "invalid Hugging Face token format"));
                }
                use std::fmt::Write as _;
                writeln!(cfg, "header = \"Authorization: Bearer {token}\"").unwrap();
            }
            use std::fmt::Write as _;
            writeln!(cfg, "url = \"{url}\"").unwrap();
            let mut req = ExecRequest::new(
                "curl",
                vec![
                    "-q".into(),
                    "--globoff".into(),
                    "--noproxy".into(),
                    "*".into(),
                    "--proto".into(),
                    "=https".into(),
                    "--resolve".into(),
                    format!("{domain}:443:{ipv4}"),
                    "-K".into(),
                    "-".into(),
                ],
            );
            req.stdin = Some(cfg.into_bytes());

            req.timeout = Some(remaining());
            let out = self.run(req, &CancelToken::new(), &mut |_| LineAction::Continue)?;
            if out.code != Some(0) {
                return Err(Error::new(
                    ErrorKind::Http,
                    "Hugging Face HTTPS transport failed",
                ));
            }
            let (status, location, body) = http_reply(&out.stdout)?;
            match status {
                200..=299 => return Ok(body.as_bytes().to_vec()),
                301 | 302 | 303 | 307 | 308 => {
                    let next = location.ok_or_else(|| {
                        Error::new(ErrorKind::Http, "Hugging Face redirect has no Location")
                    })?;
                    url = if next.starts_with('/') && !next.starts_with("//") {
                        format!("https://{domain}{next}")
                    } else {
                        next.to_owned()
                    };
                    hf_domain(&url)?;
                }
                _ => {
                    return Err(Error::new(
                        ErrorKind::Http,
                        format!("Hugging Face returned HTTP {status}"),
                    ))
                }
            }
        }
        Err(Error::new(
            ErrorKind::Http,
            "too many Hugging Face redirects",
        ))
    }
}

fn http_reply(mut reply: &str) -> Result<(u16, Option<&str>, &str)> {
    let bad = || Error::new(ErrorKind::Http, "invalid Hugging Face HTTP response");
    loop {
        let (headers, body) = reply.split_once("\r\n\r\n").ok_or_else(bad)?;
        let mut lines = headers.lines();
        let status = lines
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse::<u16>().ok())
            .ok_or_else(bad)?;
        if (100..200).contains(&status) {
            reply = body;
        } else {
            let location = lines
                .filter_map(|line| line.split_once(':'))
                .find(|(key, _)| key.eq_ignore_ascii_case("location"))
                .map(|(_, v)| v.trim());
            return Ok((status, location, body));
        }
    }
}

pub(crate) fn isolate_process_group(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
}

pub(crate) fn kill_process_group(child: &mut Child) {
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn kill(pid: i32, signal: i32) -> i32;
        }
        // A negative PID targets the process group created at spawn, including descendants.
        unsafe {
            kill(-(child.id() as i32), 9);
        }
    }
    let _ = child.kill();
    // Reap asynchronously so exiting our deadline never waits on an uninterruptible child.
    let _ = child.try_wait();
}

pub fn validate_public_https_url(url: &str) -> Result<()> {
    hf_domain(url).map(|_| ())
}

fn hf_domain(url: &str) -> Result<&'static str> {
    let bad = || {
        Error::invalid(
            "url",
            "expected HTTPS to an approved Hugging Face domain on port 443",
        )
    };
    let rest = url.strip_prefix("https://").ok_or_else(bad)?;
    if url
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '"' | '\\' | '#'))
    {
        return Err(bad());
    }
    match rest.split(['/', '?']).next() {
        Some("huggingface.co" | "huggingface.co:443") => Ok("huggingface.co"),
        Some("hf.co" | "hf.co:443") => Ok("hf.co"),
        _ => Err(bad()),
    }
}

pub(crate) fn public_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !matches!(
        (a, b, c),
        (0 | 10 | 127 | 224..=255, _, _)
            | (100, 64..=127, _)
            | (169, 254, _)
            | (172, 16..=31, _)
            | (192, 168, _)
            | (192, 0, 0 | 2)
            | (192, 88, 99)
            | (198, 18 | 19, _)
            | (198, 51, 100)
            | (203, 0, 113)
    )
}

pub(crate) fn public_ipv6(ip: Ipv6Addr) -> bool {
    let seg = ip.segments();
    if ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || ip.to_ipv4_mapped().is_some()
        || ip.to_ipv4().is_some()
    {
        return false;
    }
    // Reject unique local (fc00::/7), link-local (fe80::/10), site-local (fec0::/10),
    // Teredo (2001::/32), 6to4 (2002::/16), documentation (2001:db8::/32), and ORCHID (2001:20::/28).
    let unique_local = (seg[0] & 0xfe00) == 0xfc00;
    let link_or_site_local = (seg[0] & 0xff80) == 0xfe80;
    let teredo = seg[0] == 0x2001 && seg[1] == 0;
    let doc = seg[0] == 0x2001 && seg[1] == 0x0db8;
    let six_to_four = seg[0] == 0x2002;
    !(unique_local || link_or_site_local || teredo || doc || six_to_four)
}

pub(crate) fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => public_ipv4(v4),
        IpAddr::V6(v6) => public_ipv6(v6),
    }
}

fn validated_colab(path: &Path) -> Option<PathBuf> {
    let named_colab =
        |p: &Path| matches!(p.file_name()?.to_str()?, "colab" | "colab.exe").then_some(());
    named_colab(path)?;
    let resolved = path.canonicalize().ok()?;
    named_colab(&resolved)?;
    is_executable(&resolved).then_some(resolved)
}

fn is_executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssrf_guard_blocks_private_and_non_https() {
        assert!(validate_public_https_url("https://huggingface.co/api/models").is_ok());
        assert!(validate_public_https_url("http://huggingface.co/api/models").is_err());
        assert!(validate_public_https_url("https://127.0.0.1/admin").is_err());
        assert!(validate_public_https_url("https://169.254.169.254/latest").is_err());
        assert!(validate_public_https_url("https://172.16.0.1/x").is_err());
        assert!(validate_public_https_url("https://localhost:8080/x").is_err());
        assert!(!is_public_ip("127.0.0.1".parse().unwrap()));
        assert!(!is_public_ip("::1".parse().unwrap()));
        assert!(!is_public_ip("fe80::1".parse().unwrap()));
        assert!(!is_public_ip("fc00::1".parse().unwrap()));
        assert!(!is_public_ip("::ffff:127.0.0.1".parse().unwrap()));
        assert!(!is_public_ip("::ffff:8.8.8.8".parse().unwrap()));
        assert!(is_public_ip("2606:4700:4700::1111".parse().unwrap()));
    }
}
