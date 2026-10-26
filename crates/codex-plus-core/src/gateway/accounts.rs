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

/// 解析回调请求行里的 code / state / error。
pub fn parse_callback(request_line: &str) -> Option<Result<(String, String), String>> {
    let target = request_line.split_whitespace().nth(1)?;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path != CALLBACK_PATH {
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
    let code = tokio::time::timeout(LOGIN_TIMEOUT, wait_for_code(&listener, &state))
        .await
        .map_err(|_| anyhow::anyhow!("登录超时：5 分钟内没有完成浏览器登录"))??;
    exchange_code(&code, &verifier).await
}

async fn wait_for_code(listener: &tokio::net::TcpListener, state: &str) -> anyhow::Result<String> {
    loop {
        let (mut stream, _) = listener.accept().await?;
        let mut buffer = vec![0_u8; 8192];
        let read = stream.read(&mut buffer).await.unwrap_or(0);
        let request = String::from_utf8_lossy(&buffer[..read]).to_string();
        let line = request.lines().next().unwrap_or_default();
        let Some(result) = parse_callback(line) else {
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
}
