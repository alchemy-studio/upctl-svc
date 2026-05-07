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
    env::var("GITEA_AUTH_HEADER")
        .unwrap_or_else(|_| "Basic REPLACED".to_string())
}
