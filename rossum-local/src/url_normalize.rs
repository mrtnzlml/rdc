use anyhow::{anyhow, Result};

pub fn normalize_api_base(input: &str) -> Result<String> {
    let trimmed = input.trim();
    let url = url::Url::parse(trimmed)
        .map_err(|e| anyhow!("Not a valid URL: {e}"))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(anyhow!("URL must use http or https"));
    }
    let path = url.path().trim_end_matches('/');
    let final_path = if path.is_empty() {
        "/api/v1".to_string()
    } else if path.ends_with("/api/v1") || path == "/api/v1" {
        path.to_string()
    } else if path == "/api" {
        "/api/v1".to_string()
    } else {
        path.to_string()
    };
    let host = url.host_str().ok_or_else(|| anyhow!("URL missing host"))?;
    let port = url
        .port()
        .map(|p| format!(":{p}"))
        .unwrap_or_default();
    Ok(format!("{}://{host}{port}{final_path}", url.scheme()))
}
