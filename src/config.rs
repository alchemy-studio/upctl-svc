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

pub fn claude_prompt_prefix() -> String {
    env::var("AGENT_PROMPT_PREFIX").unwrap_or_else(|_| "不要进入plan mode，直接干活\n\n".to_string())
}
