use std::env;

pub fn port() -> u16 {
    env::var("UPCTL_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3005)
}

pub fn gitea_api_base() -> String {
    env::var("GITEA_API_BASE").unwrap_or_else(|_| "https://ci.moicen.com/api/v1".to_string())
}

pub fn gitea_auth_header() -> String {
    env::var("GITEA_AUTH_HEADER").expect("GITEA_AUTH_HEADER env var must be set")
}

pub fn jwt_key() -> Vec<u8> {
    env::var("JWT_KEY")
        .expect("JWT_KEY env var must be set")
        .into_bytes()
}

pub fn data_dir() -> String {
    env::var("DATA_DIR").unwrap_or_else(|_| "./data".to_string())
}

/// Read the stored prompt prefix from disk.
/// Falls back to env var `AGENT_PROMPT_PREFIX`, then to the hardcoded default.
pub fn claude_prompt_prefix() -> String {
    // Try stored file first
    let path = std::path::Path::new(&data_dir()).join("prompt_prefix.txt");
    if path.exists() {
        if let Ok(content) = std::fs::read_to_string(&path) {
            let trimmed = content.trim().to_string();
            if !trimmed.is_empty() {
                return trimmed + "\n\n";
            }
        }
    }
    // Fall back to env var
    env::var("AGENT_PROMPT_PREFIX").unwrap_or_else(|_| "不要进入plan mode，直接干活\n\n".to_string())
}

/// Persist a custom prompt prefix to disk.
/// Returns the actual prefix that will be used (with trailing newlines).
pub fn set_claude_prompt_prefix(text: &str) -> std::io::Result<String> {
    let data_dir = data_dir();
    let dir = std::path::Path::new(&data_dir);
    std::fs::create_dir_all(dir)?;
    let path = dir.join("prompt_prefix.txt");
    let trimmed = text.trim();
    std::fs::write(&path, trimmed)?;
    let result = if trimmed.is_empty() {
        // If empty, remove the file and use default
        let _ = std::fs::remove_file(&path);
        env::var("AGENT_PROMPT_PREFIX").unwrap_or_else(|_| "不要进入plan mode，直接干活\n\n".to_string())
    } else {
        format!("{trimmed}\n\n")
    };
    Ok(result)
}

/// Read the stored memory directory from disk.
/// Falls back to env var `AGENT_MEMORY_DIR`. Returns empty string when unset.
pub fn agent_memory_dir() -> String {
    // Try stored file first
    let path = std::path::Path::new(&data_dir()).join("memory_dir.txt");
    if path.exists() {
        if let Ok(content) = std::fs::read_to_string(&path) {
            let trimmed = content.trim().to_string();
            if !trimmed.is_empty() {
                return trimmed;
            }
        }
    }
    // Fall back to env var
    env::var("AGENT_MEMORY_DIR").unwrap_or_default()
}

/// Persist a custom memory directory to disk.
/// If empty, clears the stored value and falls back to env var.
pub fn set_agent_memory_dir(text: &str) -> std::io::Result<String> {
    let data_dir = data_dir();
    let dir = std::path::Path::new(&data_dir);
    std::fs::create_dir_all(dir)?;
    let path = dir.join("memory_dir.txt");
    let trimmed = text.trim();
    std::fs::write(&path, trimmed)?;
    let result = if trimmed.is_empty() {
        let _ = std::fs::remove_file(&path);
        env::var("AGENT_MEMORY_DIR").unwrap_or_default()
    } else {
        trimmed.to_string()
    };
    Ok(result)
}
