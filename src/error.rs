use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    InvalidInput,
    NotInstalled,
    AuthRequired,
    KeyringUnavailable,
    NotEntitled,
    CapacityRefused,
    AllocationTimeout,
    SshBusy,
    PortInUse,
    PortReserved,
    SessionGone,
    SessionLimitReached,
    ConnectionLost,
    ExecTimeout,
    OrphanSuspected,
    Cancelled,
    Parse,
    Http,
    Io,
    Other,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub context: Map<String, Value>,
}

impl Error {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            context: Map::new(),
        }
    }

    pub fn invalid(field: &str, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidInput, message).with("field", field)
    }

    pub fn with(mut self, key: &str, val: impl Into<Value>) -> Self {
        self.context.insert(key.to_owned(), val.into());
        self
    }

    pub fn with_kind(mut self, kind: ErrorKind) -> Self {
        self.kind = kind;
        self
    }

    /// Map internal `ErrorKind` + message context into a stable UI localization key.
    pub fn ui_kind(&self) -> &'static str {
        let low = self.message.to_ascii_lowercase();
        if low.contains("refresh panel")
            || low.contains("was replaced")
            || low.contains("registry changed")
            || low.contains("backend mode changed")
            || low.contains("account mismatch")
        {
            return "stale";
        }
        if low.contains("stop real runtimes")
            || low.contains("stop active oauth2")
            || low.contains("stop tracked")
            || low.contains("before leaving")
            || low.contains("before removing")
            || low.contains("before changing")
            || low.contains("before saving mock")
        {
            return "busy";
        }
        if low.contains("requires a gpu") {
            return "needs_gpu";
        }
        if low.contains("no gguf files") {
            return "no_gguf";
        }
        if low.contains("warmup") {
            return "warmup_failed";
        }
        if low.contains("llama-server exited")
            || low.contains("health endpoint timed out")
            || low.contains("did not return a completion")
        {
            return "server_exited";
        }
        if low.contains("hugging face repo") {
            return "bad_repo";
        }
        if low.contains("https url") || low.contains("ssh git remote") {
            return "bad_url";
        }
        if low.contains("bundled plugin") {
            return "builtin_plugin";
        }
        if low.contains("built-in adc profile") {
            return "default_account";
        }
        if low.contains("watchdog") && (low.contains("confirm") || low.contains("rollback")) {
            return "watchdog_unconfirmed";
        }
        if low.contains("already exists")
            || low.contains("already active")
            || low.contains("already tracked")
            || low.contains("already reserved")
        {
            return "dup_name";
        }
        if low.contains("ssh session expired") || low.contains("tunnel was closed") {
            return "ssh_expired";
        }
        if low.contains("cannot open required tunnel")
            || low.contains("failed to start ssh")
            || low.contains("ssh transport")
            || low.contains("ssh could not")
            || low.contains("ssh key")
        {
            return "ssh_failed";
        }
        match self.kind {
            ErrorKind::NotInstalled => "cli_missing",
            ErrorKind::AuthRequired | ErrorKind::KeyringUnavailable => {
                if low.contains("differs") || low.contains("mismatch") {
                    "auth_mismatch"
                } else {
                    "auth_expired"
                }
            }
            ErrorKind::SessionLimitReached => "quota",
            ErrorKind::CapacityRefused | ErrorKind::NotEntitled | ErrorKind::AllocationTimeout => {
                "no_gpu"
            }
            ErrorKind::SessionGone => "not_found",
            ErrorKind::PortReserved => "port_8080",
            ErrorKind::PortInUse => "port_in_use",
            ErrorKind::SshBusy => "ssh_limit",
            ErrorKind::ConnectionLost => "ssh_expired",
            _ => "cli_error",
        }
    }

    /// Human-readable `(en, zh-CN)` copy matching `copy_spec.md` §4 for this error.
    pub fn localized(&self) -> (String, String) {
        let kind = self.ui_kind();
        match kind {
            "cli_missing" => (
                "Colab CLI isn't installed. Install google-colab-cli, then retry.".into(),
                "没有安装 Colab CLI。请安装 google-colab-cli 后重试。".into(),
            ),
            "auth_expired" => (
                "Your Google sign-in expired. Sign in again.".into(),
                "Google 登录已过期，请重新登录。".into(),
            ),
            "auth_mismatch" => (
                "This sign-in is for a different Google account.".into(),
                "登录的 Google 账号和所选账号不一致。".into(),
            ),
            "quota" => (
                "You've hit the limit for sessions running at once.".into(),
                "同时运行的会话已达上限。".into(),
            ),
            "no_gpu" => (
                "No GPU of this type is free right now. Try later or pick another.".into(),
                "这类 GPU 暂时没有空闲，请稍后再试或换一种。".into(),
            ),
            "not_found" => (
                "That session no longer exists. Refresh.".into(),
                "这个会话已经不在了，请刷新。".into(),
            ),
            "stale" => (
                "Something changed in the background. Refresh to continue.".into(),
                "数据有变化，请刷新后继续。".into(),
            ),
            "busy" => (
                "Stop running sessions first.".into(),
                "请先停止运行中的会话。".into(),
            ),
            "port_8080" => (
                "Port 8080 is used by Colab. Try 8000.".into(),
                "8080 端口被 Colab 占用，换成 8000 试试。".into(),
            ),
            "port_in_use" => {
                let port = self
                    .message
                    .split(|c: char| !c.is_ascii_digit())
                    .find(|s| (4..=5).contains(&s.len()))
                    .unwrap_or("");
                if port.is_empty() {
                    (
                        "That local port is already in use.".into(),
                        "该本地端口已被占用。".into(),
                    )
                } else {
                    (
                        format!("Local port {port} is already in use."),
                        format!("本地端口 {port} 已被占用。"),
                    )
                }
            }
            "ssh_limit" => (
                "Colab refused another connection. Close a port and try again.".into(),
                "Colab 拒绝了新连接，请先关一个端口再试。".into(),
            ),
            "ssh_expired" => (
                "The connection expired. Open the port again.".into(),
                "连接已过期，请重新开启端口。".into(),
            ),
            "ssh_failed" => ("Couldn't open the port.".into(), "端口转发失败。".into()),
            "needs_gpu" => (
                "This model needs a GPU session.".into(),
                "这个模型需要 GPU 会话。".into(),
            ),
            "no_gguf" => {
                let repo = self
                    .message
                    .split(" in ")
                    .nth(1)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("this repo");
                (
                    format!("No GGUF files found in {repo}."),
                    format!("{repo} 里没有 GGUF 文件。"),
                )
            }
            "server_exited" => (
                "The model server stopped unexpectedly.".into(),
                "模型服务意外退出。".into(),
            ),
            "warmup_failed" => ("Model warm-up failed.".into(), "模型预热失败。".into()),
            "bad_repo" => (
                "Use the format owner/name.".into(),
                "请按 owner/name 格式填写。".into(),
            ),
            "bad_url" => (
                "Use an https:// link without a password in it.".into(),
                "请使用不含账号密码的 https:// 链接。".into(),
            ),
            "dup_name" => (
                "A session with this name already exists.".into(),
                "已有同名会话。".into(),
            ),
            "builtin_plugin" => (
                "Built-in plugins can't be replaced.".into(),
                "内置插件不能覆盖。".into(),
            ),
            "default_account" => (
                "The default account can't be removed.".into(),
                "默认账号不能删除。".into(),
            ),
            "watchdog_unconfirmed" => (
                "Couldn't confirm auto-stop. Check the session.".into(),
                "自动停止没有确认成功，请检查这个会话。".into(),
            ),
            _ => (
                "Colab CLI returned an error.".into(),
                "Colab CLI 出错了。".into(),
            ),
        }
    }

    pub fn localized_json(&self) -> Value {
        let (en, zh) = self.localized();
        serde_json::json!({ "en": en, "zh-CN": zh })
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{:?}] {}", self.kind, self.message)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::new(ErrorKind::Io, e.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::new(ErrorKind::Parse, e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_kind_and_localized_messages_cover_core_variants() {
        let cases = [
            (
                Error::new(ErrorKind::NotInstalled, "missing"),
                "cli_missing",
            ),
            (Error::new(ErrorKind::AuthRequired, "login"), "auth_expired"),
            (Error::new(ErrorKind::SessionLimitReached, "quota"), "quota"),
            (Error::new(ErrorKind::CapacityRefused, "capacity"), "no_gpu"),
            (Error::new(ErrorKind::SessionGone, "gone"), "not_found"),
            (
                Error::new(ErrorKind::PortReserved, "port 8080 is reserved"),
                "port_8080",
            ),
            (
                Error::invalid("backend_mode", "backend mode changed"),
                "stale",
            ),
            (
                Error::invalid("repo", "no GGUF files in org/model"),
                "no_gguf",
            ),
        ];
        for (err, expected_kind) in cases {
            assert_eq!(err.ui_kind(), expected_kind);
            let (en, zh) = err.localized();
            assert!(!en.trim().is_empty(), "empty en for {expected_kind}");
            assert!(!zh.trim().is_empty(), "empty zh for {expected_kind}");
        }
    }
}
