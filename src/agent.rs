use std::process::Stdio;

#[derive(Debug)]
pub struct AgentError(pub String);

impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Agent backend abstraction — tmux operations via local or SSH tunnel.
///
/// # Configuration (env vars)
///
/// | Variable | Default | Description |
/// |----------|---------|-------------|
/// | `AGENT_BACKEND` | `local` | `local` (direct tmux) or `ssh` (tunnel) |
/// | `TMUX_SSH_HOST` | `studio-nps` | SSH target for tunnel mode |
/// | `TMUX_SSH_JUMP` | _(none)_ | Optional jump host (two-hop) |
/// | `TMUX_SSH_OPTS` | `StrictHostKeyChecking=no,ConnectTimeout=5` | SSH options |
pub enum AgentBackend {
    /// Direct local tmux (Docker / upctl-compose mode)
    Local,
    /// Via SSH tunnel (moicen Studio mode)
    Ssh {
        host: String,
        jump: Option<String>,
        opts: Vec<String>,
    },
}

impl AgentBackend {
    pub fn from_env() -> Self {
        let backend_type =
            std::env::var("AGENT_BACKEND").unwrap_or_else(|_| "local".to_string());
        match backend_type.as_str() {
            "ssh" => {
                let host = std::env::var("TMUX_SSH_HOST")
                    .unwrap_or_else(|_| "studio-nps".to_string());
                let jump = std::env::var("TMUX_SSH_JUMP").ok();
                let opts: Vec<String> = std::env::var("TMUX_SSH_OPTS")
                    .unwrap_or_else(|_| {
                        "StrictHostKeyChecking=no,ConnectTimeout=5".to_string()
                    })
                    .split(',')
                    .map(|s| format!("-o={}", s.trim()))
                    .collect();
                AgentBackend::Ssh { host, jump, opts }
            }
            _ => AgentBackend::Local,
        }
    }

    /// Validate session name (alphanumeric, hyphen, underscore only).
    pub fn validate_session(session: &str) -> bool {
        !session.is_empty()
            && session
                .chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    }

    /// Send keystrokes to a tmux session.
    pub async fn send_keys(&self, session: &str, keys: &str) -> Result<(), AgentError> {
        match self {
            AgentBackend::Local => {
                tmux_cmd(&["send-keys", "-t", session, keys]).await
            }
            AgentBackend::Ssh { host, jump, opts } => {
                // Shell-escape the keys to prevent fish/bash from interpreting
                // special characters (>, <, |, $, newlines, etc.) as commands.
                let escaped = shell_escape_for_ssh(keys);
                tmux_cmd_ssh(host, jump.as_deref(), opts, &["send-keys", "-t", session, &escaped])
                    .await
            }
        }
    }

    /// Send a prompt to the agent TUI with two-step submit.
    /// First types the text, then presses Enter separately — avoids
    /// timing issues with DeepSeek TUI where text+Enter in one send-keys
    /// leaves the prompt in draft without submitting.
    pub async fn send_prompt(&self, session: &str, prompt: &str) -> Result<(), AgentError> {
        // Step 1: type the prompt text
        self.send_keys(session, prompt).await?;
        // Brief pause to let the TUI process the text input
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        // Step 2: press Enter to submit
        self.send_keys(session, "Enter").await
    }

    /// Capture pane output from a tmux session (last 200 lines).
    pub async fn capture_pane(&self, session: &str) -> Result<String, AgentError> {
        match self {
            AgentBackend::Local => {
                tmux_cmd_output(&["capture-pane", "-t", session, "-p", "-S", "-200"]).await
            }
            AgentBackend::Ssh { host, jump, opts } => {
                tmux_cmd_ssh_output(
                    host,
                    jump.as_deref(),
                    opts,
                    &["capture-pane", "-t", session, "-p", "-S", "-200"],
                )
                .await
            }
        }
    }

    /// Check if a tmux session exists.
    pub async fn has_session(&self, session: &str) -> bool {
        match self {
            AgentBackend::Local => {
                tokio::process::Command::new("tmux")
                    .args(["has-session", "-t", session])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .await
                    .map(|s| s.success())
                    .unwrap_or(false)
            }
            AgentBackend::Ssh { host, jump, opts } => {
                tmux_cmd_ssh_status(host, jump.as_deref(), opts, &["has-session", "-t", session])
                    .await
            }
        }
    }

    /// Ensure a tmux session exists, optionally creating it with a command.
    ///
    /// In SSH mode, creating sessions is not supported — only checks existence.
    pub async fn ensure_session(
        &self,
        session: &str,
        cmd: Option<&str>,
    ) -> Result<(), AgentError> {
        if self.has_session(session).await {
            return Ok(());
        }
        match self {
            AgentBackend::Local => {
                let mut c = tokio::process::Command::new("tmux");
                c.args(["new-session", "-d", "-s", session]);
                if let Some(cmd_str) = cmd {
                    c.arg(cmd_str);
                }
                let output = c.output().await.map_err(|e| AgentError(e.to_string()))?;
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Err(AgentError(format!(
                        "tmux new-session failed: {stderr}"
                    )));
                }
                Ok(())
            }
            AgentBackend::Ssh { .. } => {
                Err(AgentError(
                    "Cannot create tmux session via SSH. Create it manually.".to_string(),
                ))
            }
        }
    }
}

// ── local tmux helpers ────────────────────────────────────────

async fn tmux_cmd(args: &[&str]) -> Result<(), AgentError> {
    let output = tokio::process::Command::new("tmux")
        .args(args)
        .output()
        .await
        .map_err(|e| AgentError(format!("tmux spawn: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AgentError(format!("tmux error: {stderr}")));
    }
    Ok(())
}

async fn tmux_cmd_output(args: &[&str]) -> Result<String, AgentError> {
    let output = tokio::process::Command::new("tmux")
        .args(args)
        .output()
        .await
        .map_err(|e| AgentError(format!("tmux spawn: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AgentError(format!("tmux error: {stderr}")));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

// ── SSH tmux helpers ──────────────────────────────────────────

/// Wrap text in single quotes for safe passage through the remote shell.
/// Single quotes prevent ALL shell interpretation (variable expansion,
/// globbing, redirection).  Embedded single quotes are handled with
/// the standard '\'' (end quote, escaped quote, resume quote) sequence.
fn shell_escape_for_ssh(text: &str) -> String {
    let escaped = text.replace('\'', "'\\''");
    format!("'{}'", escaped)
}

fn ssh_cmd(
    host: &str,
    jump: Option<&str>,
    opts: &[String],
    tmux_args: &[&str],
) -> tokio::process::Command {
    let mut c = tokio::process::Command::new("ssh");
    for opt in opts {
        c.arg(opt);
    }
    if let Some(j) = jump {
        c.args(["-J", j]);
    }
    c.arg(host);
    c.arg("tmux");
    c.args(tmux_args);
    c
}

async fn tmux_cmd_ssh(
    host: &str,
    jump: Option<&str>,
    opts: &[String],
    tmux_args: &[&str],
) -> Result<(), AgentError> {
    let output = ssh_cmd(host, jump, opts, tmux_args)
        .output()
        .await
        .map_err(|e| AgentError(format!("ssh spawn: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AgentError(format!("ssh error: {stderr}")));
    }
    Ok(())
}

async fn tmux_cmd_ssh_output(
    host: &str,
    jump: Option<&str>,
    opts: &[String],
    tmux_args: &[&str],
) -> Result<String, AgentError> {
    let output = ssh_cmd(host, jump, opts, tmux_args)
        .output()
        .await
        .map_err(|e| AgentError(format!("ssh spawn: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AgentError(format!("ssh error: {stderr}")));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

async fn tmux_cmd_ssh_status(
    host: &str,
    jump: Option<&str>,
    opts: &[String],
    tmux_args: &[&str],
) -> bool {
    ssh_cmd(host, jump, opts, tmux_args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}
