use anyhow::Context;

fn apply_configured_ca_certificate(
    mut builder: reqwest::ClientBuilder,
) -> anyhow::Result<reqwest::ClientBuilder> {
    let Some(value) = std::env::var_os("CODEX_CA_CERTIFICATE") else {
        return Ok(builder);
    };
    let value = value.to_string_lossy().trim().to_string();
    if value.is_empty() {
        return Ok(builder);
    }
    let pem = if value.contains("BEGIN CERTIFICATE") {
        value.into_bytes()
    } else {
        std::fs::read(&value).with_context(|| {
            format!("读取 CODEX_CA_CERTIFICATE 指定的证书文件失败：{value}")
        })?
    };
    let certificate = reqwest::Certificate::from_pem(&pem)
        .context("CODEX_CA_CERTIFICATE 不是有效的 PEM 证书")?;
    builder = builder.add_root_certificate(certificate);
    Ok(builder)
}

pub fn proxied_client(user_agent: &str) -> anyhow::Result<reqwest::Client> {
    let ua = if user_agent.trim().is_empty() {
        format!("CodexPlusPlus/{}", env!("CARGO_PKG_VERSION"))
    } else {
        user_agent.trim().to_string()
    };
    let mut builder = reqwest::Client::builder().user_agent(ua);
    if let Some(proxy) = crate::proxy::detect_system_proxy()
        .or_else(|| std::env::var("HTTPS_PROXY").ok())
        .or_else(|| std::env::var("https_proxy").ok())
        .or_else(|| std::env::var("ALL_PROXY").ok())
        .or_else(|| std::env::var("all_proxy").ok())
    {
        builder = builder.proxy(reqwest::Proxy::all(proxy)?);
    }
    Ok(apply_configured_ca_certificate(builder)?.build()?)
}

/// VLM 专用 HTTP client（带超时）。
/// 不复用通用 proxied_client，避免 VLM 服务无响应时永久阻塞整个代理。
pub fn vlm_http_client() -> anyhow::Result<reqwest::Client> {
    vlm_http_client_with_timeout(
        std::time::Duration::from_secs(5),
        std::time::Duration::from_secs(30),
    )
}

pub(crate) fn vlm_http_client_with_timeout(
    connect: std::time::Duration,
    total: std::time::Duration,
) -> anyhow::Result<reqwest::Client> {
    Ok(apply_configured_ca_certificate(reqwest::Client::builder()
        .user_agent(format!("CodexPlusPlus-VLM/{}", env!("CARGO_PKG_VERSION")))
        .connect_timeout(connect)
        .timeout(total))?
        .build()?)
}
