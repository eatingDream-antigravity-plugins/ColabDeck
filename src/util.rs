use crate::error::{Error, ErrorKind, Result};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Cooperative cancellation token shared across provisioning, tunnels, and plugin runs.
#[derive(Debug, Clone, Default)]
pub struct CancelToken {
    cancelled: Arc<AtomicBool>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(Error::new(ErrorKind::Cancelled, "operation cancelled"))
        } else {
            Ok(())
        }
    }
}

/// Current Unix epoch timestamp in seconds.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn now_f64() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn system_time_from_secs(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

pub fn system_time_to_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// POSIX single-quote escaping for safe shell argv assembly (e.g. OpenSSH ProxyCommand).
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"@%_+=:,./-".contains(&b))
    {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\"'\"'");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Strict environment variable name validation (`[A-Za-z_][A-Za-z0-9_]*`).
pub fn is_env_key(k: &str) -> bool {
    let mut chars = k.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Return at most the last `max_bytes` UTF-8 safe tail of a string.
pub fn tail(s: &str, max_bytes: usize) -> String {
    let trimmed = s.trim();
    if trimmed.len() <= max_bytes {
        return trimmed.to_owned();
    }
    let mut idx = trimmed.len().saturating_sub(max_bytes);
    while idx < trimmed.len() && !trimmed.is_char_boundary(idx) {
        idx += 1;
    }
    format!("…{}", &trimmed[idx..])
}

/// Remove literal secrets and common credential formats before exposing diagnostics.
pub fn redact_secrets(input: &str, secrets: &[String]) -> String {
    let mut redactor = DiagnosticRedactor::new(secrets);
    input
        .split_inclusive('\n')
        .map(|line| redactor.line(line))
        .collect()
}

// Separate diagnostic keywords in the binary string table as well as at runtime.
const CREDENTIAL_FIELDS: &str = "access_token refresh_token id_token oauth_token \
    runtime_proxy_token client_secret private_key api_key hf_token x-colab-runtime-proxy-token \
    x-goog-colab-token proxy_token token authorization credentials adc_credentials \
    oauth_credentials cookie set-cookie ";
const TOKEN_PREFIXES: &str = "hf_ ya29. 1// ";

enum SecretSpan {
    Pem(String),
    Value,
    Quoted {
        quote: char,
        escaped: bool,
    },
    Container {
        depth: usize,
        quote: Option<char>,
        escaped: bool,
    },
}

pub(crate) struct DiagnosticRedactor<'a> {
    secrets: &'a [String],
    span: Option<SecretSpan>,
}

impl<'a> DiagnosticRedactor<'a> {
    pub(crate) fn new(secrets: &'a [String]) -> Self {
        Self {
            secrets,
            span: None,
        }
    }

    pub(crate) fn line(&mut self, input: &str) -> String {
        let line = input;
        let lower = line.to_ascii_lowercase();
        let mut cursor = 0;
        let mut clean = String::with_capacity(line.len());
        while cursor < line.len() {
            let rest = &line[cursor..];
            match self.span.take() {
                Some(SecretSpan::Pem(end)) => {
                    if let Some(i) = lower[cursor..].find(&end) {
                        cursor += i + end.len();
                    } else {
                        self.span = Some(SecretSpan::Pem(end));
                        break;
                    }
                }
                Some(SecretSpan::Value) => {
                    let value = rest.trim_start_matches([' ', '\t', '\r', '\n']);
                    let start = line.len() - value.len();
                    clean.push_str(&line[cursor..start]);
                    cursor = start;
                    if value.is_empty() {
                        self.span = Some(SecretSpan::Value);
                    } else if let Some((header_len, end)) = pem_header(&lower[start..]) {
                        clean.push_str("***");
                        cursor += header_len;
                        self.span = Some(SecretSpan::Pem(end));
                    } else if let Some(quote @ ('"' | '\'')) = value.chars().next() {
                        clean.push(quote);
                        clean.push_str("***");
                        cursor += 1;
                        self.span = Some(SecretSpan::Quoted {
                            quote,
                            escaped: false,
                        });
                    } else if value.starts_with(['{', '[']) {
                        clean.push_str("***");
                        self.span = Some(SecretSpan::Container {
                            depth: 0,
                            quote: None,
                            escaped: false,
                        });
                    } else if let Some(scheme) = ["bearer", "basic"].into_iter().find(|scheme| {
                        lower[start..].strip_prefix(scheme).is_some_and(|tail| {
                            tail.starts_with(char::is_whitespace) || tail.is_empty()
                        })
                    }) {
                        clean.push_str(&line[start..start + scheme.len()]);
                        cursor += scheme.len();
                        self.span = Some(SecretSpan::Value);
                    } else {
                        clean.push_str("***");
                        let bytes = value.find(secret_delimiter).unwrap_or(value.len());
                        cursor += if bytes == 0 {
                            value.chars().next().unwrap().len_utf8()
                        } else {
                            bytes
                        };
                    }
                }
                Some(SecretSpan::Quoted { quote, mut escaped }) => {
                    if let Some(end) = rest.char_indices().find_map(|(index, ch)| {
                        if ch == quote && !escaped {
                            return Some(index);
                        }
                        escaped = ch == '\\' && !escaped;
                        None
                    }) {
                        clean.push(quote);
                        cursor += end + 1;
                    } else {
                        self.span = Some(SecretSpan::Quoted { quote, escaped });
                        break;
                    }
                }
                Some(SecretSpan::Container {
                    mut depth,
                    mut quote,
                    mut escaped,
                }) => {
                    let end = rest.char_indices().find_map(|(index, ch)| {
                        if let Some(delimiter) = quote {
                            if ch == delimiter && !escaped {
                                quote = None;
                            }
                            escaped = ch == '\\' && !escaped;
                        } else {
                            match ch {
                                '\'' | '"' => quote = Some(ch),
                                '[' | '{' => depth += 1,
                                ']' | '}' if depth <= 1 => return Some(index),
                                ']' | '}' => depth -= 1,
                                _ => {}
                            }
                        }
                        None
                    });
                    if let Some(end) = end {
                        cursor += end + 1;
                    } else {
                        self.span = Some(SecretSpan::Container {
                            depth,
                            quote,
                            escaped,
                        });
                        break;
                    }
                }
                None => {
                    let mut next = None;
                    let mut select = |start, end, span| {
                        if next.as_ref().is_none_or(|(first, _, _)| start < *first) {
                            next = Some((start, end, span));
                        }
                    };
                    for (offset, _) in lower[cursor..].match_indices("-----begin ") {
                        let start = cursor + offset;
                        if let Some((len, footer)) = pem_header(&lower[start..]) {
                            select(start, start + len, Some(SecretSpan::Pem(footer)));
                            break;
                        }
                    }
                    for (offset, _) in lower[cursor..].match_indices("bearer") {
                        let index = cursor + offset;
                        let start = index + "bearer".len();
                        if !lower[..index].ends_with(char::is_alphanumeric)
                            && lower[start..].starts_with(char::is_whitespace)
                        {
                            select(start, start, Some(SecretSpan::Value));
                            break;
                        }
                    }
                    for marker in TOKEN_PREFIXES.split_ascii_whitespace() {
                        for (offset, _) in lower[cursor..].match_indices(marker) {
                            let start = cursor + offset;
                            let len = line[start..]
                                .find(secret_delimiter)
                                .unwrap_or(line.len() - start);
                            if marker != "hf_"
                                || line[start + 3..]
                                    .bytes()
                                    .take_while(u8::is_ascii_alphanumeric)
                                    .count()
                                    >= 12
                            {
                                select(start, start + len, None);
                                break;
                            }
                        }
                    }
                    for field in CREDENTIAL_FIELDS.split_ascii_whitespace() {
                        for (offset, _) in lower[cursor..].match_indices(field) {
                            let start = cursor + offset;
                            if lower[..start]
                                .ends_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
                            {
                                continue;
                            }
                            let after = &line[start + field.len()..];
                            let Some(value) = after
                                .trim_start_matches(['"', '\'', ' ', '\t', '\\'])
                                .strip_prefix([':', '='])
                            else {
                                continue;
                            };
                            let position = line.len() - value.len();
                            select(position, position, Some(SecretSpan::Value));
                            break;
                        }
                    }
                    let Some((start, end, span)) = next else {
                        clean.push_str(rest);
                        break;
                    };
                    clean.push_str(&line[cursor..start]);
                    if span.is_none() || matches!(span, Some(SecretSpan::Pem(_))) {
                        clean.push_str("***");
                    }
                    self.span = span;
                    cursor = end;
                }
            }
        }
        for secret in self.secrets.iter().flat_map(|secret| secret.lines()) {
            if !secret.trim().is_empty() {
                clean = clean.replace(secret, "***");
            }
        }
        clean
    }
}

fn secret_delimiter(c: char) -> bool {
    c.is_whitespace() || "\"'&,;<>])}\\\\\u{60}".contains(c)
}

fn pem_header(input: &str) -> Option<(usize, String)> {
    let prefix = "-----begin ";
    let rest = input.strip_prefix(prefix)?;
    let (label, _) = rest.split_once("-----")?;
    (!label.is_empty()
        && label
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b" -".contains(&c)))
    .then(|| {
        (
            prefix.len() + label.len() + 5,
            format!("-----end {label}-----"),
        )
    })
}

pub fn redact_json(value: &mut serde_json::Value, secrets: &[String]) {
    use serde_json::Value;
    match value {
        Value::String(s) => *s = redact_secrets(s, secrets),
        Value::Array(arr) => {
            let mut redactor = DiagnosticRedactor::new(secrets);
            for value in arr {
                if let Value::String(line) = value {
                    *line = line
                        .split_inclusive('\n')
                        .map(|line| redactor.line(line))
                        .collect();
                } else {
                    redactor = DiagnosticRedactor::new(secrets);
                    redact_json(value, secrets);
                }
            }
        }
        Value::Object(map) => {
            for (key, mut val) in std::mem::take(map) {
                let field = key.to_ascii_lowercase().replace('-', "_");
                if matches!(
                    field.as_str(),
                    "token"
                        | "authorization"
                        | "cookie"
                        | "set_cookie"
                        | "client_secret"
                        | "private_key"
                        | "api_key"
                        | "credentials"
                        | "adc_credentials"
                        | "oauth_credentials"
                ) || field.ends_with("_token")
                {
                    val = Value::String("***".into());
                } else {
                    redact_json(&mut val, secrets);
                }
                map.insert(redact_secrets(&key, secrets), val);
            }
        }
        _ => {}
    }
}

/// Standard RFC 4648 Base64 encoder (zero dependencies).
pub fn b64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut i = 0;
    while i < bytes.len() {
        let b0 = bytes[i];
        let b1 = if i + 1 < bytes.len() { bytes[i + 1] } else { 0 };
        let b2 = if i + 2 < bytes.len() { bytes[i + 2] } else { 0 };
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if i + 1 < bytes.len() {
            out.push(TABLE[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if i + 2 < bytes.len() {
            out.push(TABLE[(b2 & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        i += 3;
    }
    out
}

/// FNV-1a 64-bit hash for fast snapshot delta deduplication.
pub fn fnv1a_64(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in data {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

pub const MAX_STATE_BYTES: u64 = 4 * 1024 * 1024;
pub const MAX_CREDENTIAL_BYTES: u64 = 256 * 1024;

#[cfg(unix)]
fn current_euid() -> u32 {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: geteuid takes no pointers and always succeeds on POSIX.
    unsafe { geteuid() }
}

fn is_temp_root(path: &Path) -> bool {
    let tmp = std::env::temp_dir();
    path == tmp || path.canonicalize().ok() == tmp.canonicalize().ok()
}

fn harden_existing_dir(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.file_type().is_dir() {
        return Err(Error::invalid(
            "path",
            format!("{} must be a real directory, not a symlink", path.display()),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
        if meta.uid() != current_euid() {
            return Err(Error::invalid(
                "path",
                format!("{} is not owned by the current user", path.display()),
            ));
        }
        if meta.permissions().mode() & 0o777 != 0o700 {
            let mut opts = OpenOptions::new();
            opts.read(true);
            #[cfg(target_os = "linux")]
            opts.custom_flags(0o400000 | 0o200000); // O_NOFOLLOW | O_DIRECTORY
            #[cfg(target_os = "macos")]
            opts.custom_flags(0x0100 | 0x100000); // O_NOFOLLOW | O_DIRECTORY
            if let Ok(dir) = opts.open(path) {
                let fd_meta = dir.metadata()?;
                if !fd_meta.is_dir() || fd_meta.uid() != current_euid() {
                    return Err(Error::invalid(
                        "path",
                        format!("{} must be an owner-owned directory", path.display()),
                    ));
                }
                dir.set_permissions(fs::Permissions::from_mode(0o700))?;
            } else {
                fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
            }
        }
    }
    Ok(())
}

/// Ensure `path` is a real directory owned by the current user with `0700` permissions on Unix.
pub fn ensure_private_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => return harden_existing_dir(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    harden_existing_dir(path)
}

/// Ensure `root` and each nested child directory exist with `0700` permissions on Unix.
pub fn ensure_private_subdir(root: &Path, parts: &[&str]) -> Result<PathBuf> {
    let mut cur = root.to_path_buf();
    ensure_private_dir(&cur)?;
    for part in parts {
        cur.push(part);
        ensure_private_dir(&cur)?;
    }
    Ok(cur)
}

/// Open a regular file without following symlinks or blocking on FIFOs.
pub fn open_private_nofollow(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        #[cfg(target_os = "linux")]
        const FLAGS: i32 = 0o400000 | 0o4000; // O_NOFOLLOW | O_NONBLOCK
        #[cfg(target_os = "macos")]
        const FLAGS: i32 = 0x0100 | 0x0004; // O_NOFOLLOW | O_NONBLOCK
        options.custom_flags(FLAGS);
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "symlinks are rejected",
        ));
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    Ok(file)
}

/// Read a regular file (without following symlinks) up to `max_bytes`.
pub fn read_private_bounded(path: &Path, max_bytes: u64) -> io::Result<Vec<u8>> {
    let file = open_private_nofollow(path)?;
    let meta = file.metadata()?;
    if meta.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("file exceeds {max_bytes} bytes"),
        ));
    }
    let mut buf = Vec::with_capacity((meta.len() as usize).min(64 * 1024));
    file.take(max_bytes + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("file exceeds {max_bytes} bytes"),
        ));
    }
    Ok(buf)
}

/// Read a UTF-8 regular file (without following symlinks) up to `max_bytes`.
pub fn read_private_bounded_string(path: &Path, max_bytes: u64) -> io::Result<String> {
    let bytes = read_private_bounded(path, max_bytes)?;
    String::from_utf8(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Remove dead-PID owner-owned `.cmcp-{adc,session,list}-<pid>-*` temp directories.
pub fn sweep_stale_temp_dirs() {
    sweep_stale_temp_dirs_in(&std::env::temp_dir());
}

pub fn sweep_stale_temp_dirs_in(dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        const PREFIXES: [&str; 3] = [".cmcp-adc-", ".cmcp-session-", ".cmcp-list-"];
        const ESRCH: i32 = 3;
        unsafe extern "C" {
            fn kill(pid: i32, signal: i32) -> i32;
        }
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        let euid = current_euid();
        let self_pid = std::process::id();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name_str) = name.to_str() else {
                continue;
            };
            let Some(rest) = PREFIXES.iter().find_map(|p| name_str.strip_prefix(p)) else {
                continue;
            };
            let Some(pid_str) = rest.split('-').next() else {
                continue;
            };
            let Ok(pid) = pid_str.parse::<u32>() else {
                continue;
            };
            if pid == 0 || pid > i32::MAX as u32 || pid == self_pid {
                continue;
            }
            let path = entry.path();
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if !meta.file_type().is_dir() || meta.uid() != euid {
                continue;
            }
            // SAFETY: kill(pid, 0) checks process existence without delivering a signal.
            let rc = unsafe { kill(pid as i32, 0) };
            if rc == -1 && io::Error::last_os_error().raw_os_error() == Some(ESRCH) {
                let _ = fs::remove_dir_all(path);
            }
        }
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// Hold the returned file open for the duration of the transaction.
pub(crate) fn lock_file(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        if !is_temp_root(parent) {
            ensure_private_dir(parent)?;
        }
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
        #[cfg(target_os = "linux")]
        options.custom_flags(0o400000 | 0o4000); // O_NOFOLLOW | O_NONBLOCK
        #[cfg(target_os = "macos")]
        options.custom_flags(0x0100 | 0x0004); // O_NOFOLLOW | O_NONBLOCK
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(Error::invalid("lock", "lock target must be a regular file"));
    }
    file.lock()?;
    Ok(file)
}

pub(crate) fn display_path(path: &Path) -> String {
    if let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) {
        if let Ok(rest) = path.strip_prefix(home) {
            return Path::new("~").join(rest).to_string_lossy().into_owned();
        }
    }
    path.to_string_lossy().into_owned()
}

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub struct PrivateDir(PathBuf);

impl PrivateDir {
    pub fn new(parent: &Path, purpose: &str) -> Result<Self> {
        fs::create_dir_all(parent)?;
        loop {
            let seq = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!(
                ".cmcp-{purpose}-{}-{}-{seq}",
                std::process::id(),
                now_millis()
            ));
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Preserve a recovery receipt when the caller cannot record its endpoint.
    pub(crate) fn keep(mut self) {
        self.0 = PathBuf::new();
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        if !self.0.as_os_str().is_empty() {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

/// Atomically write a file with owner-only (`0600`) permissions on Unix.
pub fn write_atomic_private(path: &Path, content: &[u8]) -> Result<()> {
    let tmp = PrivateDir::new(path.parent().unwrap_or(Path::new(".")), "write")?;
    let staged = tmp.path().join("data");
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(&staged)?.write_all(content)?;
    fs::rename(staged, path)?;
    Ok(())
}

/// Percent-encode a query parameter value (RFC 3986 unreserved chars preserved).
pub fn encode_query(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else if b == b' ' {
            out.push_str("%20");
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

/// Generate a collision-resistant human-friendly session name (`colab-<acc>-<short-ts>`).
pub fn generate_session_name(prefix: &str, existing: &[String]) -> String {
    let clean: String = prefix
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(12)
        .collect();
    let tag = if clean.is_empty() { "vm" } else { &clean };
    let base_n = now_secs() % 100_000;
    for offset in 0..1000u64 {
        let candidate = format!("c-{tag}-{:05}", (base_n + offset) % 100_000);
        if !existing.iter().any(|s| s == &candidate) {
            return candidate;
        }
    }
    format!("c-{tag}-{}", now_millis() % 1_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_and_shell_quote_and_redact() {
        assert_eq!(b64_encode(b""), "");
        assert_eq!(b64_encode(b"f"), "Zg==");
        assert_eq!(b64_encode(b"fo"), "Zm8=");
        assert_eq!(b64_encode(b"foo"), "Zm9v");
        assert_eq!(b64_encode(b"hello world"), "aGVsbG8gd29ybGQ=");

        assert_eq!(shell_quote("abc-1.2"), "abc-1.2");
        assert_eq!(shell_quote("a b'c"), "'a b'\"'\"'c'");
        assert!(is_env_key("PLUGIN_INPUT_REPO"));
        assert!(!is_env_key("1BAD"));
        assert!(!is_env_key("BAD-KEY"));

        let sample = format!("{}_{}", "demo", "aaaa1111");
        let input = format!("val={sample} ok");
        let masked = redact_secrets(&input, &[sample]);
        assert_eq!(masked, "val=*** ok");
    }

    #[test]
    fn ensure_private_dir_and_bounded_reads_harden_permissions_and_reject_symlinks() {
        let root = PrivateDir::new(&std::env::temp_dir(), "sec-util").unwrap();
        let dir = root.path().join("nested");
        fs::create_dir(&dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
            ensure_private_dir(&dir).unwrap();
            let mode = fs::symlink_metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700);

            let target = root.path().join("secret.txt");
            fs::write(&target, b"hello").unwrap();
            let link = root.path().join("link.txt");
            std::os::unix::fs::symlink(&target, &link).unwrap();
            assert!(ensure_private_dir(&link).is_err());
            assert!(read_private_bounded(&link, 1024).is_err());
            assert!(lock_file(&link).is_err());
        }
        let big = root.path().join("big.bin");
        fs::write(&big, vec![b'a'; 64]).unwrap();
        assert!(read_private_bounded(&big, 32).is_err());
        assert_eq!(read_private_bounded(&big, 64).unwrap().len(), 64);
    }

    #[test]
    fn sweep_stale_temp_dirs_removes_only_dead_pid_cmcp_dirs() {
        let root = PrivateDir::new(&std::env::temp_dir(), "sec-sweep").unwrap();
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let dead_pid = child.id();
        let _ = child.wait().unwrap();

        let dead_adc = root.path().join(format!(".cmcp-adc-{dead_pid}-1-0"));
        let dead_sess = root.path().join(format!(".cmcp-session-{dead_pid}-1-0"));
        let dead_list = root.path().join(format!(".cmcp-list-{dead_pid}-1-0"));
        let live_adc = root
            .path()
            .join(format!(".cmcp-adc-{}-1-0", std::process::id()));
        let unrelated = root.path().join(format!("other-{dead_pid}-1-0"));
        for d in [&dead_adc, &dead_sess, &dead_list, &live_adc, &unrelated] {
            fs::create_dir(d).unwrap();
        }
        sweep_stale_temp_dirs_in(root.path());
        assert!(!dead_adc.exists());
        assert!(!dead_sess.exists());
        assert!(!dead_list.exists());
        assert!(live_adc.exists());
        assert!(unrelated.exists());
    }
}
