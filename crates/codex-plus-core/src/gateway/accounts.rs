//! 在网关里直接登录订阅账号（目前是 ChatGPT）。
//!
//! 走与 Codex CLI 相同的浏览器 PKCE 登录：本机 1455 端口接回调，换到的 token 只存在 gateway.json，
//! 与 Codex 自己的登录是两次独立的登录（各自的 refresh token），互不影响。

use std::time::Duration;

use anyhow::{Context, bail};
use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::official_login::{GATEWAY_OWNED_MARKER, OFFICIAL_OAUTH_CLIENT_ID};
use crate::settings::{RelayMode, RelayProfile, RelayProtocol};

const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
/// Codex CLI 登记的回调地址，端口固定。
const CALLBACK_PORT: u16 = 1455;
const CALLBACK_PATH: &str = "/auth/callback";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

/// 一次登录拿到的账号。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatGptAccount {
    pub email: String,
    pub plan: String,
    pub account_id: String,
    /// auth.json 结构，带网关所有的标记。
    pub auth_contents: String,
}

/// PKCE：verifier 用两个随机 UUID 拼成（64 个 [0-9a-f] 字符），challenge 是它的 SHA-256。
pub fn pkce_pair() -> (String, String) {
    let verifier = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

fn redirect_uri() -> String {
    format!("http://localhost:{CALLBACK_PORT}{CALLBACK_PATH}")
}

/// 浏览器要打开的登录地址。
pub fn chatgpt_authorize_url(challenge: &str, state: &str) -> String {
    let params = [
        ("response_type", "code".to_string()),
        ("client_id", OFFICIAL_OAUTH_CLIENT_ID.to_string()),
        ("redirect_uri", redirect_uri()),
        ("scope", "openid profile email offline_access".to_string()),
        ("code_challenge", challenge.to_string()),
        ("code_challenge_method", "S256".to_string()),
        ("id_token_add_organizations", "true".to_string()),
        ("codex_cli_simplified_flow", "true".to_string()),
        ("state", state.to_string()),
    ];
    let query = params
        .iter()
        .map(|(key, value)| format!("{key}={}", encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{AUTHORIZE_URL}?{query}")
}

/// 解析回调请求行里的 code / state / error（ChatGPT 的回调路径）。
pub fn parse_callback(request_line: &str) -> Option<Result<(String, String), String>> {
    parse_callback_at(request_line, CALLBACK_PATH)
}

fn parse_callback_at(
    request_line: &str,
    callback_path: &str,
) -> Option<Result<(String, String), String>> {
    let target = request_line.split_whitespace().nth(1)?;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path != callback_path {
        return None;
    }
    let mut code = String::new();
    let mut state = String::new();
    let mut error = String::new();
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let value = decode(value);
        match key {
            "code" => code = value,
            "state" => state = value,
            "error" | "error_description" if error.is_empty() => error = value,
            _ => {}
        }
    }
    if !error.is_empty() {
        return Some(Err(error));
    }
    if code.is_empty() {
        return Some(Err("回调里没有授权码".to_string()));
    }
    Some(Ok((code, state)))
}

fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => out.push(b' '),
            b'%' if index + 2 < bytes.len() => {
                if let Ok(byte) = u8::from_str_radix(&value[index + 1..index + 3], 16) {
                    out.push(byte);
                    index += 2;
                } else {
                    out.push(b'%');
                }
            }
            byte => out.push(byte),
        }
        index += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn jwt_claims(token: &str) -> Value {
    token
        .split('.')
        .nth(1)
        .and_then(|payload| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(payload.as_bytes())
                .ok()
        })
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(Value::Null)
}

/// 换 token 的响应 → 账号。
pub fn account_from_tokens(tokens: &Value) -> anyhow::Result<ChatGptAccount> {
    let field = |key: &str| {
        tokens
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let access_token = field("access_token");
    let refresh_token = field("refresh_token");
    let id_token = field("id_token");
    if access_token.is_empty() || refresh_token.is_empty() {
        bail!("登录响应里缺少 token");
    }
    let claims = jwt_claims(&id_token);
    let auth = claims
        .get("https://api.openai.com/auth")
        .cloned()
        .unwrap_or(Value::Null);
    let account_id = auth
        .get("chatgpt_account_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let plan = auth
        .get("chatgpt_plan_type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let email = claims
        .get("email")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let auth_contents = serde_json::to_string_pretty(&json!({
        "auth_mode": "chatgpt",
        GATEWAY_OWNED_MARKER: true,
        "tokens": {
            "id_token": id_token,
            "access_token": access_token,
            "refresh_token": refresh_token,
            "account_id": account_id,
        }
    }))?;
    Ok(ChatGptAccount {
        email,
        plan,
        account_id,
        auth_contents,
    })
}

/// 登录得到的账号做成网关供应商（官方通道，Responses 协议，没有地址和 Key）。
pub fn chatgpt_relay(account: &ChatGptAccount, models: &[String]) -> RelayProfile {
    let plan = match account.plan.as_str() {
        "" => String::new(),
        plan => format!(" {}", plan[..1].to_uppercase() + &plan[1..]),
    };
    let name = if account.email.is_empty() {
        format!("ChatGPT{plan}")
    } else {
        format!("ChatGPT{plan} · {}", account.email)
    };
    RelayProfile {
        name,
        relay_mode: RelayMode::Official,
        protocol: RelayProtocol::Responses,
        base_url: String::new(),
        api_key: String::new(),
        auth_contents: account.auth_contents.clone(),
        model: models.first().cloned().unwrap_or_default(),
        model_list: models.join("\n"),
        ..RelayProfile::default()
    }
}

/// 跑一次 ChatGPT 浏览器登录：在 1455 端口等回调，换 token。`open_browser` 负责打开登录页。
pub async fn chatgpt_login(open_browser: impl FnOnce(&str)) -> anyhow::Result<ChatGptAccount> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", CALLBACK_PORT))
        .await
        .with_context(|| {
            format!("本机端口 {CALLBACK_PORT} 被占用（可能 Codex 正在登录），请稍后再试")
        })?;
    let (verifier, challenge) = pkce_pair();
    let state = uuid::Uuid::new_v4().simple().to_string();
    open_browser(&chatgpt_authorize_url(&challenge, &state));
    let code = tokio::time::timeout(
        LOGIN_TIMEOUT,
        wait_for_callback(&listener, CALLBACK_PATH, &state),
    )
    .await
    .map_err(|_| anyhow::anyhow!("登录超时：5 分钟内没有完成浏览器登录"))??;
    exchange_code(&code, &verifier).await
}

async fn wait_for_callback(
    listener: &tokio::net::TcpListener,
    callback_path: &str,
    state: &str,
) -> anyhow::Result<String> {
    loop {
        let (mut stream, _) = listener.accept().await?;
        let mut buffer = vec![0_u8; 8192];
        let read = stream.read(&mut buffer).await.unwrap_or(0);
        let request = String::from_utf8_lossy(&buffer[..read]).to_string();
        let line = request.lines().next().unwrap_or_default();
        let Some(result) = parse_callback_at(line, callback_path) else {
            let _ = stream
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                )
                .await;
            continue;
        };
        let (ok, page) = match &result {
            Ok((_, returned)) if returned == state => (true, "登录成功，可以回到 Codex++ 了。"),
            Ok(_) => (
                false,
                "登录校验失败（state 不一致），请回到 Codex++ 重新登录。",
            ),
            Err(_) => (false, "登录没有完成，请回到 Codex++ 重新登录。"),
        };
        let body = format!(
            "<!doctype html><meta charset=\"utf-8\"><title>Codex++</title><body style=\"font-family:system-ui;padding:48px;text-align:center\"><h2>{page}</h2></body>"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.shutdown().await;
        return match result {
            Ok((code, returned)) if returned == state => Ok(code),
            Ok(_) => bail!("登录校验失败（state 不一致）"),
            Err(error) => bail!("登录失败：{error}"),
        };
    }
}

async fn exchange_code(code: &str, verifier: &str) -> anyhow::Result<ChatGptAccount> {
    let token_url = std::env::var("CODEX_PLUS_OFFICIAL_TOKEN_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| crate::official_login::OFFICIAL_OAUTH_TOKEN_URL.to_string());
    let client = reqwest::Client::builder()
        .user_agent(format!("CodexPlusPlus/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .build()?;
    let response = client
        .post(token_url)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &redirect_uri()),
            ("client_id", OFFICIAL_OAUTH_CLIENT_ID),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .context("换取登录 token 失败")?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("换取登录 token 失败：HTTP {}", status.as_u16());
    }
    let value: Value = serde_json::from_str(&text).context("登录响应不是 JSON")?;
    account_from_tokens(&value)
}

/* —————————————— GitHub Copilot —————————————— */

/// VS Code Copilot 的 OAuth App（Copilot 的 token 接口只认它这类白名单 App）。
pub const COPILOT_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const GITHUB_DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const GITHUB_ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const COPILOT_TOKEN_URL: &str = "https://api.github.com/copilot_internal/v2/token";
const GITHUB_USER_URL: &str = "https://api.github.com/user";
const COPILOT_DEFAULT_API: &str = "https://api.githubcopilot.com";
/// 凭据里标识账号类型的字段。
pub const ACCOUNT_KIND_FIELD: &str = "codex_plus_account";
/// Copilot 短期 token 到期前多久换新。
const COPILOT_REFRESH_AHEAD_MS: u64 = 5 * 60 * 1000;

/// Copilot 请求要带的编辑器标识（与 VS Code Copilot Chat 一致）。
pub fn copilot_headers() -> Vec<(&'static str, &'static str)> {
    vec![
        ("Copilot-Integration-Id", "vscode-chat"),
        ("Editor-Version", "vscode/1.95.0"),
        ("Editor-Plugin-Version", "copilot-chat/0.26.7"),
        ("User-Agent", "GitHubCopilotChat/0.26.7"),
        ("Openai-Intent", "conversation-panel"),
        ("X-Github-Api-Version", "2025-04-01"),
    ]
}

/// 设备码登录的第一步：拿到用户要在浏览器里输入的验证码。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval: u64,
    pub expires_in: u64,
}

fn http_client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent("GitHubCopilotChat/0.26.7")
        .timeout(Duration::from_secs(30))
        .build()?)
}

pub async fn copilot_device_code() -> anyhow::Result<DeviceCode> {
    let response = http_client()?
        .post(GITHUB_DEVICE_CODE_URL)
        .header(reqwest::header::ACCEPT, "application/json")
        .form(&[("client_id", COPILOT_CLIENT_ID), ("scope", "read:user")])
        .send()
        .await
        .context("连不上 GitHub")?;
    let value: Value = response.json().await.context("GitHub 返回的不是 JSON")?;
    parse_device_code(&value)
}

pub fn parse_device_code(value: &Value) -> anyhow::Result<DeviceCode> {
    let text = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let code = DeviceCode {
        device_code: text("device_code"),
        user_code: text("user_code"),
        verification_uri: text("verification_uri"),
        interval: value
            .get("interval")
            .and_then(Value::as_u64)
            .unwrap_or(5)
            .max(1),
        expires_in: value
            .get("expires_in")
            .and_then(Value::as_u64)
            .unwrap_or(900),
    };
    if code.device_code.is_empty() || code.user_code.is_empty() {
        bail!(
            "GitHub 没有返回设备码：{}",
            value
                .get("error_description")
                .and_then(Value::as_str)
                .unwrap_or("未知错误")
        );
    }
    Ok(code)
}

/// 轮询结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevicePoll {
    Token(String),
    Pending,
    SlowDown,
    Failed(String),
}

pub fn parse_device_poll(value: &Value) -> DevicePoll {
    if let Some(token) = value
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
    {
        return DevicePoll::Token(token.to_string());
    }
    match value
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "authorization_pending" => DevicePoll::Pending,
        "slow_down" => DevicePoll::SlowDown,
        "expired_token" => DevicePoll::Failed("验证码过期了，请重新登录".to_string()),
        "access_denied" => DevicePoll::Failed("在 GitHub 上取消了授权".to_string()),
        other => DevicePoll::Failed(format!("GitHub 登录失败：{other}")),
    }
}

/// 等用户在浏览器里输入验证码，拿到 GitHub token。
pub async fn copilot_wait_for_github_token(code: &DeviceCode) -> anyhow::Result<String> {
    let client = http_client()?;
    let mut interval = code.interval;
    let deadline = std::time::Instant::now() + Duration::from_secs(code.expires_in.max(60));
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let response = client
            .post(GITHUB_ACCESS_TOKEN_URL)
            .header(reqwest::header::ACCEPT, "application/json")
            .form(&[
                ("client_id", COPILOT_CLIENT_ID),
                ("device_code", code.device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .await;
        let Ok(response) = response else { continue };
        let Ok(value) = response.json::<Value>().await else {
            continue;
        };
        match parse_device_poll(&value) {
            DevicePoll::Token(token) => return Ok(token),
            DevicePoll::Pending => {}
            DevicePoll::SlowDown => interval += 5,
            DevicePoll::Failed(message) => bail!(message),
        }
    }
    bail!("登录超时：验证码已过期")
}

/// Copilot 的短期 token。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopilotToken {
    pub token: String,
    pub expires_at_ms: u64,
    pub api_base: String,
}

pub fn parse_copilot_token(value: &Value) -> anyhow::Result<CopilotToken> {
    let token = value
        .get("token")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if token.is_empty() {
        bail!("这个 GitHub 账号没有可用的 Copilot 订阅");
    }
    let api_base = value
        .pointer("/endpoints/api")
        .and_then(Value::as_str)
        .map(|base| base.trim().trim_end_matches('/').to_string())
        .filter(|base| base.starts_with("https://") && base.contains("githubcopilot.com"))
        .unwrap_or_else(|| COPILOT_DEFAULT_API.to_string());
    Ok(CopilotToken {
        token,
        expires_at_ms: value.get("expires_at").and_then(Value::as_u64).unwrap_or(0) * 1000,
        api_base,
    })
}

pub async fn copilot_exchange(github_token: &str) -> anyhow::Result<CopilotToken> {
    let mut request = http_client()?
        .get(COPILOT_TOKEN_URL)
        .header(
            reqwest::header::AUTHORIZATION,
            format!("token {github_token}"),
        )
        .header(reqwest::header::ACCEPT, "application/json");
    for (key, value) in copilot_headers() {
        request = request.header(key, value);
    }
    let response = request.send().await.context("连不上 GitHub")?;
    let status = response.status();
    let value: Value = response.json().await.unwrap_or(Value::Null);
    if status.as_u16() == 401 {
        bail!("GitHub 登录已失效，请重新登录 Copilot");
    }
    if !status.is_success() {
        bail!(
            "换取 Copilot token 失败：HTTP {}（这个账号可能没有 Copilot 订阅）",
            status.as_u16()
        );
    }
    parse_copilot_token(&value)
}

async fn github_login_name(github_token: &str) -> String {
    let Ok(client) = http_client() else {
        return String::new();
    };
    let Ok(response) = client
        .get(GITHUB_USER_URL)
        .header(
            reqwest::header::AUTHORIZATION,
            format!("token {github_token}"),
        )
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
    else {
        return String::new();
    };
    response
        .json::<Value>()
        .await
        .ok()
        .and_then(|value| {
            value
                .get("login")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default()
}

/// Copilot 账号可用的对话模型。
pub async fn copilot_models(token: &CopilotToken) -> Vec<String> {
    let Ok(client) = http_client() else {
        return Vec::new();
    };
    let mut request = client
        .get(format!("{}/models", token.api_base))
        .bearer_auth(&token.token);
    for (key, value) in copilot_headers() {
        request = request.header(key, value);
    }
    let Ok(response) = request.send().await else {
        return Vec::new();
    };
    let Ok(value) = response.json::<Value>().await else {
        return Vec::new();
    };
    parse_copilot_models(&value)
}

pub fn parse_copilot_models(value: &Value) -> Vec<String> {
    let mut models = Vec::new();
    for model in value
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(id) = model.get("id").and_then(Value::as_str) else {
            continue;
        };
        let chat = model
            .pointer("/capabilities/type")
            .and_then(Value::as_str)
            .is_none_or(|kind| kind == "chat");
        let picker = model
            .get("model_picker_enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        if chat && picker && !models.iter().any(|known: &String| known == id) {
            models.push(id.to_string());
        }
    }
    models
}

/// Copilot 供应商：Chat Completions 协议，地址是 Copilot 的 API（`#` 表示不再补 `/v1`），Key 是短期 token。
pub fn copilot_relay(
    login: &str,
    github_token: &str,
    token: &CopilotToken,
    models: &[String],
) -> RelayProfile {
    let auth = json!({
        ACCOUNT_KIND_FIELD: "copilot",
        "login": login,
        "github_token": github_token,
        "copilot_token": token.token,
        "expires_at_ms": token.expires_at_ms,
        "api_base": token.api_base,
    });
    RelayProfile {
        name: if login.is_empty() {
            "GitHub Copilot".to_string()
        } else {
            format!("Copilot · {login}")
        },
        relay_mode: RelayMode::PureApi,
        protocol: RelayProtocol::ChatCompletions,
        base_url: format!("{}#", token.api_base),
        api_key: token.token.clone(),
        auth_contents: auth.to_string(),
        model: models.first().cloned().unwrap_or_default(),
        model_list: models.join("\n"),
        custom_headers: copilot_headers()
            .into_iter()
            .map(|(key, value)| crate::settings::RelayHeaderKeyValue {
                key: key.to_string(),
                value: value.to_string(),
            })
            .collect(),
        ..RelayProfile::default()
    }
}

/// 完成 Copilot 登录：拿 GitHub token → 换 Copilot token → 读模型。
pub async fn copilot_finish(code: &DeviceCode) -> anyhow::Result<RelayProfile> {
    let github_token = copilot_wait_for_github_token(code).await?;
    let token = copilot_exchange(&github_token).await?;
    let login = github_login_name(&github_token).await;
    let models = copilot_models(&token).await;
    Ok(copilot_relay(&login, &github_token, &token, &models))
}

/* —————————————— xAI Grok（SuperGrok 订阅） —————————————— */

/// Grok CLI 的公开 OAuth 客户端（没有 secret）。
pub const GROK_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const GROK_AUTHORIZE_URL: &str = "https://auth.x.ai/oauth2/authorize";
const GROK_TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";
const GROK_SCOPES: &str = "openid profile email offline_access grok-cli:access api:access";
/// 用订阅额度的推理代理（api.x.ai 对 OAuth token 只给免费额度）。
pub const GROK_PROXY_BASE: &str = "https://cli-chat-proxy.grok.com/v1";
const GROK_CALLBACK_PORT: u16 = 56121;
const GROK_REFRESH_AHEAD_MS: u64 = 5 * 60 * 1000;
const GROK_DEFAULT_MODELS: &[&str] =
    &["grok-4.6", "grok-build-0.1", "grok-4.3", "grok-code-fast-1"];

/// 推理代理要求的客户端标识，缺了会回 426。版本号被 xAI 提高时要跟着改。
pub fn grok_proxy_headers() -> Vec<(&'static str, &'static str)> {
    vec![
        ("X-XAI-Token-Auth", "xai-grok-cli"),
        ("x-authenticateresponse", "authenticate-response"),
        ("x-grok-client-mode", "headless"),
        ("x-grok-client-version", "1.0.5"),
        ("x-grok-client-identifier", "grok-shell"),
    ]
}

pub fn grok_authorize_url(redirect_uri: &str, challenge: &str, state: &str) -> String {
    let params = [
        ("response_type", "code".to_string()),
        ("client_id", GROK_CLIENT_ID.to_string()),
        ("redirect_uri", redirect_uri.to_string()),
        ("scope", GROK_SCOPES.to_string()),
        ("code_challenge", challenge.to_string()),
        ("code_challenge_method", "S256".to_string()),
        ("state", state.to_string()),
        ("nonce", uuid::Uuid::new_v4().simple().to_string()),
    ];
    let query = params
        .iter()
        .map(|(key, value)| format!("{key}={}", encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{GROK_AUTHORIZE_URL}?{query}")
}

/// Grok 的 token：access / refresh 与到期时间。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrokTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at_ms: u64,
    pub email: String,
}

pub fn parse_grok_tokens(value: &Value, previous_refresh: &str) -> anyhow::Result<GrokTokens> {
    let text = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let access_token = text("access_token");
    if access_token.is_empty() {
        bail!(
            "Grok 登录失败：{}",
            value
                .get("error_description")
                .or_else(|| value.get("error"))
                .and_then(Value::as_str)
                .unwrap_or("没有拿到 token")
        );
    }
    let refresh_token = Some(text("refresh_token"))
        .filter(|token| !token.is_empty())
        .unwrap_or_else(|| previous_refresh.to_string());
    let expires_in = value
        .get("expires_in")
        .and_then(Value::as_u64)
        .unwrap_or(3600);
    let email = jwt_claims(&text("id_token"))
        .get("email")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Ok(GrokTokens {
        access_token,
        refresh_token,
        expires_at_ms: now_ms() + expires_in * 1000,
        email,
    })
}

async fn grok_token_request(form: &[(&str, &str)]) -> anyhow::Result<Value> {
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?
        .post(GROK_TOKEN_URL)
        .header(reqwest::header::ACCEPT, "application/json")
        .form(form)
        .send()
        .await
        .context("连不上 auth.x.ai")?;
    let status = response.status();
    let value: Value = response.json().await.unwrap_or(Value::Null);
    if !status.is_success() && value.get("access_token").is_none() {
        let reason = value
            .get("error_description")
            .or_else(|| value.get("error"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        bail!("Grok 登录失败：HTTP {} {reason}", status.as_u16());
    }
    Ok(value)
}

/// 浏览器登录 Grok（PKCE，本机回调；默认端口被占时换一个空闲端口）。
pub async fn grok_login(open_browser: impl FnOnce(&str)) -> anyhow::Result<GrokTokens> {
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", GROK_CALLBACK_PORT)).await {
        Ok(listener) => listener,
        Err(_) => tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .context("无法在本机监听登录回调")?,
    };
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    let (verifier, challenge) = pkce_pair();
    let state = uuid::Uuid::new_v4().simple().to_string();
    open_browser(&grok_authorize_url(&redirect_uri, &challenge, &state));
    let code = tokio::time::timeout(
        LOGIN_TIMEOUT,
        wait_for_callback(&listener, "/callback", &state),
    )
    .await
    .map_err(|_| anyhow::anyhow!("登录超时：5 分钟内没有完成浏览器登录"))??;
    let value = grok_token_request(&[
        ("grant_type", "authorization_code"),
        ("code", &code),
        ("redirect_uri", &redirect_uri),
        ("client_id", GROK_CLIENT_ID),
        ("code_verifier", &verifier),
    ])
    .await?;
    parse_grok_tokens(&value, "")
}

/// SuperGrok 账号可用的模型；读不到时用常见的几个。
pub async fn grok_models(access_token: &str) -> Vec<String> {
    let fetched = async {
        let mut request = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .ok()?
            .get(format!("{GROK_PROXY_BASE}/models-v2"))
            .bearer_auth(access_token);
        for (key, value) in grok_proxy_headers() {
            request = request.header(key, value);
        }
        let value: Value = request.send().await.ok()?.json().await.ok()?;
        Some(parse_grok_models(&value))
    }
    .await
    .unwrap_or_default();
    if fetched.is_empty() {
        GROK_DEFAULT_MODELS
            .iter()
            .map(|model| model.to_string())
            .collect()
    } else {
        fetched
    }
}

/// 模型目录的字段名各版本不一，`data` / `models` 里的 `id` / `name` / `model` 都认。
pub fn parse_grok_models(value: &Value) -> Vec<String> {
    let mut models: Vec<String> = Vec::new();
    for key in ["data", "models"] {
        for item in value
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let id = item.as_str().or_else(|| {
                ["id", "name", "model"]
                    .iter()
                    .find_map(|field| item.get(*field).and_then(Value::as_str))
            });
            if let Some(id) = id.map(str::trim).filter(|id| !id.is_empty())
                && !models.iter().any(|known| known == id)
            {
                models.push(id.to_string());
            }
        }
    }
    models
}

/// SuperGrok 供应商：Responses 协议，地址是订阅推理代理，Key 是 OAuth access token。
pub fn grok_relay(tokens: &GrokTokens, models: &[String]) -> RelayProfile {
    let auth = json!({
        ACCOUNT_KIND_FIELD: "grok",
        "email": tokens.email,
        "access_token": tokens.access_token,
        "refresh_token": tokens.refresh_token,
        "expires_at_ms": tokens.expires_at_ms,
    });
    RelayProfile {
        name: if tokens.email.is_empty() {
            "SuperGrok".to_string()
        } else {
            format!("SuperGrok · {}", tokens.email)
        },
        relay_mode: RelayMode::PureApi,
        protocol: RelayProtocol::Responses,
        base_url: GROK_PROXY_BASE.to_string(),
        api_key: tokens.access_token.clone(),
        auth_contents: auth.to_string(),
        model: models.first().cloned().unwrap_or_default(),
        model_list: models.join("\n"),
        custom_headers: grok_proxy_headers()
            .into_iter()
            .map(|(key, value)| crate::settings::RelayHeaderKeyValue {
                key: key.to_string(),
                value: value.to_string(),
            })
            .collect(),
        ..RelayProfile::default()
    }
}

fn grok_from_auth(relay: &RelayProfile, value: &Value) -> Option<RelayProfile> {
    let expires = value
        .get("expires_at_ms")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let token = value
        .get("access_token")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if token.is_empty() || expires <= now_ms() + GROK_REFRESH_AHEAD_MS {
        return None;
    }
    let mut updated = relay.clone();
    updated.api_key = token.to_string();
    updated.base_url = GROK_PROXY_BASE.to_string();
    Some(updated)
}

async fn prepare_grok(relay: &RelayProfile, value: &Value) -> anyhow::Result<RelayProfile> {
    if let Some(ready) = grok_from_auth(relay, value) {
        return Ok(ready);
    }
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    let _guard = LOCK.get_or_init(Default::default).lock().await;
    let store = crate::gateway::store::GatewayStore::default();
    let latest = store
        .load()
        .ok()
        .and_then(|gateway| {
            gateway
                .provider(&relay.id)
                .and_then(|provider| account_kind(&provider.relay))
                .map(|(_, value)| value)
        })
        .unwrap_or_else(|| value.clone());
    if let Some(ready) = grok_from_auth(relay, &latest) {
        return Ok(ready);
    }
    let refresh = latest
        .get("refresh_token")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if refresh.is_empty() {
        bail!("SuperGrok 登录已过期，请重新登录");
    }
    let response = grok_token_request(&[
        ("grant_type", "refresh_token"),
        ("client_id", GROK_CLIENT_ID),
        ("refresh_token", &refresh),
    ])
    .await
    .context("SuperGrok 登录已失效，请重新登录")?;
    let tokens = parse_grok_tokens(&response, &refresh)?;
    let mut auth = latest.clone();
    auth["access_token"] = json!(tokens.access_token);
    auth["refresh_token"] = json!(tokens.refresh_token);
    auth["expires_at_ms"] = json!(tokens.expires_at_ms);
    let mut updated = relay.clone();
    updated.auth_contents = auth.to_string();
    updated.api_key = tokens.access_token.clone();
    updated.base_url = GROK_PROXY_BASE.to_string();
    if let Ok(mut gateway) = store.load()
        && let Some(provider) = gateway
            .providers
            .iter_mut()
            .find(|provider| provider.relay.id == relay.id)
    {
        provider.relay.auth_contents = updated.auth_contents.clone();
        let _ = store.save(&gateway);
    }
    Ok(updated)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis() as u64)
        .unwrap_or_default()
}

fn account_kind(relay: &RelayProfile) -> Option<(String, Value)> {
    let value: Value = serde_json::from_str(&relay.auth_contents).ok()?;
    let kind = value.get(ACCOUNT_KIND_FIELD)?.as_str()?.to_string();
    Some((kind, value))
}

/// 请求前准备账号类供应商的凭据：Copilot 的短期 token 快过期时换新并写回 gateway.json。
/// 普通供应商原样返回。
pub async fn prepare_relay(relay: &RelayProfile) -> anyhow::Result<RelayProfile> {
    let Some((kind, value)) = account_kind(relay) else {
        return Ok(relay.clone());
    };
    match kind.as_str() {
        "copilot" => prepare_copilot(relay, &value).await,
        "grok" => prepare_grok(relay, &value).await,
        _ => Ok(relay.clone()),
    }
}

/// 从凭据里取出当前的 Copilot token 和地址（地址、Key 不单独落盘，读回来要从凭据补上）。
/// token 快过期时返回 None。
fn copilot_from_auth(relay: &RelayProfile, value: &Value) -> Option<RelayProfile> {
    let expires = value
        .get("expires_at_ms")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let token = value
        .get("copilot_token")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if token.is_empty() || expires <= now_ms() + COPILOT_REFRESH_AHEAD_MS {
        return None;
    }
    let api_base = value
        .get("api_base")
        .and_then(Value::as_str)
        .filter(|base| !base.is_empty())
        .unwrap_or(COPILOT_DEFAULT_API);
    let mut updated = relay.clone();
    updated.api_key = token.to_string();
    updated.base_url = format!("{api_base}#");
    Some(updated)
}

async fn prepare_copilot(relay: &RelayProfile, value: &Value) -> anyhow::Result<RelayProfile> {
    if let Some(ready) = copilot_from_auth(relay, value) {
        return Ok(ready);
    }
    // 串行换新：并发请求只换一次。
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    let _guard = LOCK.get_or_init(Default::default).lock().await;
    // 等锁期间别的请求可能已经换过了。
    let store = crate::gateway::store::GatewayStore::default();
    if let Some(latest) = store.load().ok().and_then(|gateway| {
        gateway
            .provider(&relay.id)
            .map(|provider| provider.relay.clone())
    }) && let Some((_, latest_value)) = account_kind(&latest)
        && let Some(ready) = copilot_from_auth(relay, &latest_value)
    {
        return Ok(ready);
    }
    let github_token = value
        .get("github_token")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if github_token.is_empty() {
        bail!("Copilot 登录信息不完整，请重新登录");
    }
    let token = copilot_exchange(github_token).await?;
    let mut updated = relay.clone();
    let mut auth = value.clone();
    auth["copilot_token"] = json!(token.token);
    auth["expires_at_ms"] = json!(token.expires_at_ms);
    auth["api_base"] = json!(token.api_base);
    updated.auth_contents = auth.to_string();
    updated.api_key = token.token.clone();
    updated.base_url = format!("{}#", token.api_base);
    if let Ok(mut gateway) = store.load()
        && let Some(provider) = gateway
            .providers
            .iter_mut()
            .find(|provider| provider.relay.id == relay.id)
    {
        provider.relay.auth_contents = updated.auth_contents.clone();
        provider.relay.api_key = updated.api_key.clone();
        provider.relay.base_url = updated.base_url.clone();
        let _ = store.save(&gateway);
    }
    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt(payload: Value) -> String {
        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        format!(
            "{}.{}.",
            engine.encode(br#"{"alg":"none"}"#),
            engine.encode(serde_json::to_vec(&payload).unwrap())
        )
    }

    #[test]
    fn pkce_challenge_is_sha256_of_verifier() {
        let (verifier, challenge) = pkce_pair();
        assert_eq!(verifier.len(), 64);
        let expected = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(verifier.as_bytes()));
        assert_eq!(challenge, expected);
        let url = chatgpt_authorize_url(&challenge, "st");
        assert!(url.starts_with("https://auth.openai.com/oauth/authorize?response_type=code"));
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"));
        assert!(url.contains("scope=openid%20profile%20email%20offline_access"));
        assert!(url.contains(&format!("code_challenge={challenge}")));
    }

    #[test]
    fn callback_parsing() {
        assert_eq!(
            parse_callback("GET /auth/callback?code=ab%2Fc&state=xyz HTTP/1.1"),
            Some(Ok(("ab/c".to_string(), "xyz".to_string())))
        );
        assert_eq!(
            parse_callback("GET /auth/callback?error=access_denied HTTP/1.1"),
            Some(Err("access_denied".to_string()))
        );
        assert_eq!(parse_callback("GET /favicon.ico HTTP/1.1"), None);
    }

    #[test]
    fn tokens_become_a_gateway_owned_official_provider() {
        let tokens = json!({
            "access_token": "at",
            "refresh_token": "rt",
            "id_token": jwt(json!({
                "email": "me@example.com",
                "https://api.openai.com/auth": { "chatgpt_account_id": "acc-1", "chatgpt_plan_type": "pro" }
            }))
        });
        let account = account_from_tokens(&tokens).unwrap();
        assert_eq!(account.email, "me@example.com");
        assert_eq!(account.account_id, "acc-1");
        assert_eq!(account.plan, "pro");
        let relay = chatgpt_relay(
            &account,
            &["gpt-6.1-sol".to_string(), "gpt-6-luna".to_string()],
        );
        assert_eq!(relay.name, "ChatGPT Pro · me@example.com");
        assert!(crate::official_login::member_uses_official_login_transport(
            &relay
        ));
        let auth: Value = serde_json::from_str(&relay.auth_contents).unwrap();
        assert_eq!(auth[GATEWAY_OWNED_MARKER], true);
        assert_eq!(auth["tokens"]["account_id"], "acc-1");
        assert!(account_from_tokens(&json!({ "access_token": "x" })).is_err());
    }

    #[test]
    fn refreshed_tokens_are_written_back_to_the_gateway_store() {
        let temp = tempfile::tempdir().unwrap();
        let store = crate::gateway::store::GatewayStore::at(temp.path().join("gateway.json"));
        let account = account_from_tokens(
            &json!({ "access_token": "old-at", "refresh_token": "old-rt", "id_token": "" }),
        )
        .unwrap();
        let mut relay = chatgpt_relay(&account, &["gpt-6.1-sol".to_string()]);
        relay.id = "chatgpt".to_string();
        let mut gateway = crate::gateway::store::GatewaySettings::default();
        gateway
            .providers
            .push(crate::gateway::store::GatewayProvider {
                enabled: true,
                source: "login:chatgpt".to_string(),
                price: None,
                relay,
            });
        store.save(&gateway).unwrap();
        crate::official_login::persist_gateway_login(
            &store, "chatgpt", "new-at", "new-rt", "acc-9",
        );
        let saved = store.load().unwrap();
        let auth: Value =
            serde_json::from_str(&saved.provider("chatgpt").unwrap().relay.auth_contents).unwrap();
        assert_eq!(auth["tokens"]["access_token"], "new-at");
        assert_eq!(auth["tokens"]["refresh_token"], "new-rt");
        assert_eq!(auth["tokens"]["account_id"], "acc-9");
        assert_eq!(auth[GATEWAY_OWNED_MARKER], true, "标记保留");
    }

    #[test]
    fn copilot_device_flow_and_token_parsing() {
        let code = parse_device_code(&json!({ "device_code": "dc", "user_code": "ABCD-1234", "verification_uri": "https://github.com/login/device", "interval": 5, "expires_in": 900 })).unwrap();
        assert_eq!(code.user_code, "ABCD-1234");
        assert!(parse_device_code(&json!({ "error": "x" })).is_err());
        assert_eq!(
            parse_device_poll(&json!({ "error": "authorization_pending" })),
            DevicePoll::Pending
        );
        assert_eq!(
            parse_device_poll(&json!({ "error": "slow_down" })),
            DevicePoll::SlowDown
        );
        assert_eq!(
            parse_device_poll(&json!({ "access_token": "gho_x" })),
            DevicePoll::Token("gho_x".to_string())
        );
        assert!(matches!(
            parse_device_poll(&json!({ "error": "access_denied" })),
            DevicePoll::Failed(_)
        ));

        let token = parse_copilot_token(&json!({ "token": "tid=1", "expires_at": 1_800_000_000, "endpoints": { "api": "https://api.individual.githubcopilot.com/" } })).unwrap();
        assert_eq!(token.api_base, "https://api.individual.githubcopilot.com");
        assert_eq!(token.expires_at_ms, 1_800_000_000_000);
        let untrusted = parse_copilot_token(
            &json!({ "token": "t", "endpoints": { "api": "https://evil.example" } }),
        )
        .unwrap();
        assert_eq!(
            untrusted.api_base, "https://api.githubcopilot.com",
            "只信任 githubcopilot.com"
        );
        assert!(parse_copilot_token(&json!({})).is_err());

        let models = parse_copilot_models(&json!({ "data": [
            { "id": "gpt-5.5", "model_picker_enabled": true, "capabilities": { "type": "chat" } },
            { "id": "text-embedding-3-small", "capabilities": { "type": "embeddings" } },
            { "id": "hidden", "model_picker_enabled": false },
            { "id": "claude-sonnet-5.5" }
        ]}));
        assert_eq!(models, vec!["gpt-5.5", "claude-sonnet-5.5"]);

        let relay = copilot_relay("octo", "gho_x", &token, &models);
        assert_eq!(relay.name, "Copilot · octo");
        assert_eq!(relay.protocol, RelayProtocol::ChatCompletions);
        assert_eq!(
            crate::protocol_proxy::chat_completions_url(&relay.base_url),
            "https://api.individual.githubcopilot.com/chat/completions"
        );
        assert!(
            relay
                .custom_headers
                .iter()
                .any(|header| header.key == "Copilot-Integration-Id")
        );
        assert_eq!(account_kind(&relay).unwrap().0, "copilot");
    }

    #[tokio::test]
    async fn fresh_copilot_tokens_are_used_as_is() {
        let token = CopilotToken {
            token: "fresh".to_string(),
            expires_at_ms: now_ms() + 3_600_000,
            api_base: "https://api.githubcopilot.com".to_string(),
        };
        let relay = copilot_relay("octo", "gho_x", &token, &[]);
        let mut stored = relay.clone();
        // 读回来时地址和 Key 不在（不单独落盘），要从凭据补上。
        stored.api_key.clear();
        stored.base_url = "https://api.openai.com/v1".to_string();
        let prepared = prepare_relay(&stored).await.unwrap();
        assert_eq!(prepared.api_key, "fresh");
        assert_eq!(prepared.base_url, "https://api.githubcopilot.com#");
        let plain = RelayProfile {
            api_key: "sk".to_string(),
            ..RelayProfile::default()
        };
        assert_eq!(prepare_relay(&plain).await.unwrap().api_key, "sk");
    }

    #[test]
    fn grok_login_and_relay() {
        let url = grok_authorize_url("http://127.0.0.1:56121/callback", "ch", "st");
        assert!(url.starts_with("https://auth.x.ai/oauth2/authorize?response_type=code"));
        assert!(url.contains("client_id=b1a00492-073a-47ea-816f-4c329264a828"));
        assert!(url.contains(
            "scope=openid%20profile%20email%20offline_access%20grok-cli%3Aaccess%20api%3Aaccess"
        ));
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A56121%2Fcallback"));
        assert_eq!(
            parse_callback_at("GET /callback?code=c1&state=st HTTP/1.1", "/callback"),
            Some(Ok(("c1".to_string(), "st".to_string())))
        );

        let tokens = parse_grok_tokens(&json!({ "access_token": "at", "refresh_token": "rt", "expires_in": 3600, "id_token": jwt(json!({ "email": "g@x.ai" })) }), "").unwrap();
        assert_eq!(tokens.email, "g@x.ai");
        let rotated = parse_grok_tokens(&json!({ "access_token": "at2" }), "rt").unwrap();
        assert_eq!(
            rotated.refresh_token, "rt",
            "没有返回新的 refresh token 时沿用旧的"
        );
        assert!(parse_grok_tokens(&json!({ "error": "invalid_grant" }), "").is_err());

        assert_eq!(
            parse_grok_models(
                &json!({ "data": [{ "id": "grok-4.6" }, { "name": "grok-build-0.1" }] })
            ),
            vec!["grok-4.6", "grok-build-0.1"]
        );
        assert_eq!(
            parse_grok_models(&json!({ "models": ["grok-4.3"] })),
            vec!["grok-4.3"]
        );

        let relay = grok_relay(&tokens, &["grok-4.6".to_string()]);
        assert_eq!(relay.name, "SuperGrok · g@x.ai");
        assert_eq!(relay.protocol, RelayProtocol::Responses);
        assert_eq!(relay.base_url, GROK_PROXY_BASE);
        assert!(
            relay
                .custom_headers
                .iter()
                .any(|header| header.key == "X-XAI-Token-Auth" && header.value == "xai-grok-cli")
        );
        let mut stored = relay.clone();
        stored.api_key.clear();
        let ready = grok_from_auth(&stored, &account_kind(&relay).unwrap().1).unwrap();
        assert_eq!(ready.api_key, "at");
    }
}
