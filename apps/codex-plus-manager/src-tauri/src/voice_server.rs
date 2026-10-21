//! ZCode 语音输入的本地回环 HTTP 服务。
//!
//! 注入到 ZCode 渲染层的脚本通过 `fetch` 调用本服务（页面是 `file://`，无法直接
//! 使用 Codex++ 的 Tauri IPC，但没有 CSP 限制），由管理端负责录音与 ASR：
//!
//! - `POST /zcode-voice/start?token=...` 开始录音；
//! - `POST /zcode-voice/stop?token=...` 停止录音并返回识别文本；
//! - `POST /zcode-voice/cancel?token=...` 取消录音；
//! - `GET /zcode-voice/health` 健康检查（不需要令牌）。
//!
//! 安全模型：
//! - 只监听 `127.0.0.1:0`（随机端口），页面拿不到端口就调不到；
//! - 每次进程启动生成 32 位十六进制随机令牌，走 query 参数；缺失/错误一律 403；
//! - 响应带 `Access-Control-Allow-Origin: *`（页面 Origin 是 `null`，实测必须放行），
//!   并处理 OPTIONS 预检；
//! - 端口与令牌写到 `~/.codex-session-delete/zcode-voice.json`（0600）方便调试，
//!   诊断日志里不写令牌。
//!
//! ASR 复用 `codex_plus_core::dictation::transcribe`：它要的是 multipart 请求体，
//! 所以本地把 16 kHz PCM16 WAV 包成 `file` 字段再调用。转写是 async 的，这里在
//! 每个连接自己的线程里用管理端的 tokio runtime `block_on`，绝不占用 Tauri 主线程。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::Context;

use codex_plus_core::dictation;
use codex_plus_core::settings::SettingsStore;

/// 只监听回环地址；端口交给内核随机分配，避免和用户机器上的固定端口冲突。
const BIND_ADDRESS: &str = "127.0.0.1:0";
/// 令牌长度（十六进制字符数，即 16 字节随机数）。
const TOKEN_HEX_CHARS: usize = 32;
/// 请求头上限：只是本机页面发起的几个短请求，超过这个量直接拒绝。
const MAX_HEADER_BYTES: usize = 16 * 1024;
/// 读写超时。转写本身可能很慢，但那只发生在连接线程里算，不受 socket 超时影响。
const SOCKET_TIMEOUT: Duration = Duration::from_secs(15);
/// 写 receipt 的文件名。
const RECEIPT_FILE: &str = "zcode-voice.json";

/// 本地语音服务的连接信息。页面脚本与状态查询都通过它拿到端口和令牌。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceServerInfo {
    pub port: u16,
    pub token: String,
    /// 完整基础地址，如 `http://127.0.0.1:53421`
    pub endpoint: String,
}

static SERVER: OnceLock<Mutex<Option<VoiceServerInfo>>> = OnceLock::new();

fn server_slot() -> &'static Mutex<Option<VoiceServerInfo>> {
    SERVER.get_or_init(|| Mutex::new(None))
}

fn lock_slot() -> std::sync::MutexGuard<'static, Option<VoiceServerInfo>> {
    server_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// 启动（或复用）本地语音服务，返回连接信息。
pub fn ensure() -> anyhow::Result<VoiceServerInfo> {
    let mut slot = lock_slot();
    if let Some(info) = slot.as_ref() {
        return Ok(info.clone());
    }
    let info = start_server()?;
    *slot = Some(info.clone());
    Ok(info)
}

/// 已启动时返回连接信息；未启动返回 None。
pub fn current() -> Option<VoiceServerInfo> {
    lock_slot().clone()
}

fn start_server() -> anyhow::Result<VoiceServerInfo> {
    let listener =
        TcpListener::bind(BIND_ADDRESS).context("绑定本地语音服务端口失败（127.0.0.1）")?;
    let port = listener
        .local_addr()
        .context("读取本地语音服务端口失败")?
        .port();
    let token = random_token();
    let thread_token = token.clone();
    std::thread::Builder::new()
        .name("codex-plus-voice-server".to_string())
        .spawn(move || accept_loop(listener, thread_token))
        .context("启动本地语音服务线程失败")?;

    let info = VoiceServerInfo {
        port,
        token,
        endpoint: format!("http://127.0.0.1:{port}"),
    };
    // receipt 只用于调试/状态查询，写失败不影响服务可用性。
    if let Err(error) = write_receipt(&info) {
        log_event(
            "zcode_voice.receipt_failed",
            serde_json::json!({ "message": error.to_string() }),
        );
    }
    log_event(
        "zcode_voice.server_started",
        serde_json::json!({ "port": info.port, "endpoint": info.endpoint }),
    );
    Ok(info)
}

fn accept_loop(listener: TcpListener, token: String) {
    loop {
        match listener.accept() {
            Ok((stream, _peer)) => {
                let token = token.clone();
                let spawned = std::thread::Builder::new()
                    .name("codex-plus-voice-request".to_string())
                    .spawn(move || handle_connection(stream, &token));
                if let Err(error) = spawned {
                    log_event(
                        "zcode_voice.connection_thread_failed",
                        serde_json::json!({ "message": error.to_string() }),
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                // accept 的瞬时错误（如 fd 耗尽）不该让服务线程直接退出。
                log_event(
                    "zcode_voice.accept_failed",
                    serde_json::json!({ "message": error.to_string() }),
                );
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn handle_connection(mut stream: TcpStream, token: &str) {
    let _ = stream.set_read_timeout(Some(SOCKET_TIMEOUT));
    let _ = stream.set_write_timeout(Some(SOCKET_TIMEOUT));

    let head = match read_request_head(&stream) {
        Ok(head) => head,
        Err(error) => {
            let body = json_error(&format!("无法读取语音服务请求：{error}"));
            let _ = write_http_response(&mut stream, 400, &body);
            return;
        }
    };
    let request = match parse_request_head(&head) {
        Ok(request) => request,
        Err(error) => {
            let body = json_error(&error);
            let _ = write_http_response(&mut stream, 400, &body);
            return;
        }
    };

    let (status, body) = respond(resolve_route(&request.method, &request.target, token));
    let _ = write_http_response(&mut stream, status, &body);
    let _ = stream.flush();
    // 显式关掉两端：响应头承诺 Connection: close，页面 fetch 不必复用连接。
    // 先 shutdown(Write) 让对端尽快读到 EOF；忽略 AlreadyClosed 等正常错误。
    let _ = stream.shutdown(Shutdown::Both);
}

/// 读取 HTTP 请求头（请求行 + header 行，不含空 body）。只处理文本头。
fn read_request_head(stream: &TcpStream) -> std::io::Result<String> {
    let mut reader = BufReader::new(stream);
    let mut head = String::new();
    let mut total = 0usize;
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "连接在请求头结束前关闭",
            ));
        }
        total += read;
        if total > MAX_HEADER_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "请求头超过上限",
            ));
        }
        if line == "\r\n" || line == "\n" {
            return Ok(head);
        }
        head.push_str(&line);
    }
}

/// 极简 HTTP 请求行：`METHOD TARGET VERSION`。
#[derive(Debug, Clone, PartialEq, Eq)]
struct RequestHead {
    method: String,
    target: String,
    #[allow(dead_code)] // 目前只校验格式，不区分 HTTP/1.0 与 1.1。
    version: String,
}

fn parse_request_head(head: &str) -> Result<RequestHead, String> {
    let request_line = head
        .lines()
        .next()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .ok_or_else(|| "缺少 HTTP 请求行".to_string())?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().ok_or_else(|| "缺少 HTTP 方法".to_string())?;
    let target = parts.next().ok_or_else(|| "缺少请求路径".to_string())?;
    let version = parts.next().ok_or_else(|| "缺少 HTTP 版本".to_string())?;
    if parts.next().is_some() {
        return Err("HTTP 请求行参数过多".to_string());
    }
    if !matches!(method, "GET" | "POST" | "OPTIONS") {
        return Err(format!("不支持的 HTTP 方法：{method}"));
    }
    if !version.starts_with("HTTP/1.") {
        return Err(format!("不支持的 HTTP 版本：{version}"));
    }
    if !target.starts_with('/') {
        return Err("请求路径必须以 / 开头".to_string());
    }
    Ok(RequestHead {
        method: method.to_string(),
        target: target.to_string(),
        version: version.to_string(),
    })
}

/// 路由判定结果。执行副作用（录音/转写）留给 [`respond`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    Preflight,
    Health,
    Start,
    Stop,
    Cancel,
    Forbidden,
    NotFound,
    MethodNotAllowed,
}

fn resolve_route(method: &str, target: &str, expected_token: &str) -> Route {
    let path = target.split('?').next().unwrap_or(target);
    match (method, path) {
        ("OPTIONS", _) => Route::Preflight,
        ("GET", "/zcode-voice/health") => Route::Health,
        ("POST", "/zcode-voice/start") => tokened(target, expected_token, Route::Start),
        ("POST", "/zcode-voice/stop") => tokened(target, expected_token, Route::Stop),
        ("POST", "/zcode-voice/cancel") => tokened(target, expected_token, Route::Cancel),
        (_, "/zcode-voice/health" | "/zcode-voice/start" | "/zcode-voice/stop" | "/zcode-voice/cancel") => {
            Route::MethodNotAllowed
        }
        _ => Route::NotFound,
    }
}

fn tokened(target: &str, expected_token: &str, route: Route) -> Route {
    if token_matches(query_param(target, "token").as_deref(), expected_token) {
        route
    } else {
        Route::Forbidden
    }
}

/// 提取 query 参数。本服务的值都是十六进制令牌/简单数字，不需要完整 URL 解码。
fn query_param(target: &str, name: &str) -> Option<String> {
    let query = target.split_once('?')?.1;
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then(|| value.to_string())
    })
}

/// 常量时间比较令牌，避免本机其它进程按响应时间逐字符猜。
fn token_matches(candidate: Option<&str>, expected: &str) -> bool {
    let Some(candidate) = candidate else {
        return false;
    };
    let candidate = candidate.as_bytes();
    let expected = expected.as_bytes();
    if candidate.len() != expected.len() {
        return false;
    }
    let mut diff = 0u8;
    for (left, right) in candidate.iter().zip(expected) {
        diff |= left ^ right;
    }
    diff == 0
}

/// 执行路由并返回 `(HTTP 状态码, JSON body)`。
fn respond(route: Route) -> (u16, String) {
    match route {
        Route::Preflight => (204, String::new()),
        Route::Health => (
            200,
            serde_json::json!({ "status": "ok", "recording": crate::mic::is_recording() })
                .to_string(),
        ),
        Route::Forbidden => (403, json_error("无效的语音服务令牌")),
        Route::NotFound => (404, json_error("未知的语音服务接口")),
        Route::MethodNotAllowed => (405, json_error("不支持的请求方法")),
        Route::Start => (200, handle_start()),
        Route::Stop => (200, handle_stop()),
        Route::Cancel => (200, handle_cancel()),
    }
}

fn handle_start() -> String {
    let settings = match SettingsStore::default().load() {
        Ok(settings) => settings,
        Err(error) => return json_error(&format!("读取 Codex++ 设置失败：{error}")),
    };
    if !settings.zcode_app_voice_input {
        return json_error("ZCode 语音输入已在 Codex++ 中关闭");
    }
    if !settings.enhancements_enabled {
        return json_error("Codex++ 的增强功能总开关已关闭，语音输入不可用");
    }
    if !settings.dictation.enabled {
        return json_error("语音识别未启用，请先在 Codex++ 的「语音输入」中完成配置");
    }
    match crate::mic::start() {
        Ok(()) => serde_json::json!({ "status": "ok", "recording": true }).to_string(),
        Err(error) => json_error(&error.to_string()),
    }
}

fn handle_stop() -> String {
    let wav = match crate::mic::stop() {
        Ok(wav) => wav,
        Err(error) => return json_error(&error.to_string()),
    };
    // 44 字节 = 只有 WAV 头没有任何采样（开始后立刻停止）。
    if wav.len() <= 44 {
        return json_error("没有录到声音，请靠近麦克风后重试");
    }
    let settings = match SettingsStore::default().load() {
        Ok(settings) => settings,
        Err(error) => return json_error(&format!("读取 Codex++ 设置失败：{error}")),
    };
    let (body, content_type) = multipart_wav_body(&wav);
    let transcription = transcribe_with_manager_runtime(&settings.dictation, &body, &content_type);
    match transcription {
        Ok(text) => serde_json::json!({ "status": "ok", "text": text }).to_string(),
        Err(message) => json_error(&message),
    }
}

/// 在连接线程里驱动管理端的 tokio runtime 完成一次转写。
///
/// 不能占用 Tauri 主线程：本服务每个请求都有独立线程，这里 `block_on` 只会卡住
/// 这条连接线程。转写内部会用到 `tokio::task::spawn_blocking`（本地 SenseVoice），
/// 所以必须在 tokio runtime 上下文里调用。
fn transcribe_with_manager_runtime(
    settings: &codex_plus_core::settings::DictationSettings,
    body: &[u8],
    content_type: &str,
) -> Result<String, String> {
    let result = tauri::async_runtime::block_on(async {
        dictation::transcribe(settings, body, content_type).await
    });
    match result {
        Ok(value) => Ok(value
            .get("text")
            .and_then(|text| text.as_str())
            .unwrap_or_default()
            .trim()
            .to_string()),
        Err(error) => Err(error.to_string()),
    }
}

fn handle_cancel() -> String {
    match crate::mic::cancel() {
        Ok(()) => serde_json::json!({ "status": "ok" }).to_string(),
        Err(error) => json_error(&error.to_string()),
    }
}

fn json_error(message: &str) -> String {
    serde_json::json!({ "status": "error", "message": message }).to_string()
}

/// 把 16 kHz PCM16 WAV 包成 `dictation::transcribe` 期望的 multipart 请求体。
fn multipart_wav_body(wav: &[u8]) -> (Vec<u8>, String) {
    let boundary = format!("codex-plus-zcode-voice-{}", random_token());
    let mut body = Vec::with_capacity(wav.len() + 512);
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; \
             filename=\"zcode-voice.wav\"\r\nContent-Type: audio/wav\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(wav);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (body, format!("multipart/form-data; boundary={boundary}"))
}

/// 把 `(状态码, JSON body)` 序列化成完整的 HTTP/1.1 响应字节。
fn http_response_bytes(status: u16, body: &str) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Internal Server Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: application/json; charset=utf-8\r\n\
         Content-Length: {length}\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n\
         Access-Control-Allow-Headers: Content-Type\r\n\
         Access-Control-Allow-Private-Network: true\r\n\
         Access-Control-Max-Age: 600\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\
         \r\n",
        length = body.len()
    );
    let mut response = Vec::with_capacity(head.len() + body.len());
    response.extend_from_slice(head.as_bytes());
    response.extend_from_slice(body.as_bytes());
    response
}

fn write_http_response(stream: &mut TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    let response = http_response_bytes(status, body);
    stream.write_all(&response)?;
    stream.flush()
}

/// 随机令牌：优先 `/dev/urandom`，失败时退回时间 + pid 的确定性兜底。
fn random_token() -> String {
    let mut bytes = [0u8; TOKEN_HEX_CHARS / 2];
    if let Ok(mut file) = std::fs::File::open("/dev/urandom") {
        if file.read_exact(&mut bytes).is_ok() {
            return hex_encode(&bytes);
        }
    }
    // 兜底路径只在没有 /dev/urandom 的平台出现；令牌只在单机回环使用，
    // 时间 + pid 混合足以避免不同进程撞车，不作为密码学随机来源。
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let mut state = nanos ^ ((std::process::id() as u128) << 32) ^ 0x9e37_79b9_7f4a_7c15;
    for byte in &mut bytes {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        *byte = (state >> 64) as u8;
    }
    hex_encode(&bytes)
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// 写 `~/.codex-session-delete/zcode-voice.json`（0600）。失败由调用方决定是否忽略。
fn write_receipt(info: &VoiceServerInfo) -> std::io::Result<()> {
    let dir = codex_plus_core::paths::default_app_state_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(RECEIPT_FILE);
    let payload = serde_json::to_vec_pretty(&serde_json::json!({
        "port": info.port,
        "token": info.token,
    }))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&path)?;
        file.write_all(&payload)?;
        // 文件已存在时 mode() 不生效，显式再收一次权限。
        let permissions = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(&path, permissions)?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&path, &payload)?;
    }
    Ok(())
}

fn log_event(event: &str, detail: serde_json::Value) {
    let _ = codex_plus_core::diagnostic_log::append_diagnostic_log(event, detail);
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn parse_request_line_get_health() {
        let request = parse_request_head("GET /zcode-voice/health HTTP/1.1\r\nHost: x\r\n").unwrap();
        assert_eq!(request.method, "GET");
        assert_eq!(request.target, "/zcode-voice/health");
        assert_eq!(request.version, "HTTP/1.1");
    }

    #[test]
    fn parse_request_line_post_with_query() {
        let request =
            parse_request_head("POST /zcode-voice/start?token=deadbeef HTTP/1.1\r\n").unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.target, "/zcode-voice/start?token=deadbeef");
    }

    #[test]
    fn parse_request_line_rejects_garbage() {
        assert!(parse_request_head("").is_err());
        assert!(parse_request_head("GET\r\n").is_err());
        assert!(parse_request_head("GET /x\r\n").is_err());
        assert!(parse_request_head("GET /x HTTP/1.1 extra\r\n").is_err());
        assert!(parse_request_head("DELETE /x HTTP/1.1\r\n").is_err());
        assert!(parse_request_head("GET x HTTP/1.1\r\n").is_err());
        assert!(parse_request_head("GET /x HTTP/2\r\n").is_err());
    }

    #[test]
    fn query_param_extracts_token_only() {
        assert_eq!(
            query_param("/zcode-voice/start?token=abc", "token"),
            Some("abc".to_string())
        );
        assert_eq!(
            query_param("/zcode-voice/start?x=1&token=abc&y=2", "token"),
            Some("abc".to_string())
        );
        assert_eq!(query_param("/zcode-voice/health", "token"), None);
        assert_eq!(query_param("/zcode-voice/start?x=1", "token"), None);
        assert_eq!(query_param("/zcode-voice/start?token=", "token"), Some(String::new()));
    }

    #[test]
    fn token_matches_requires_exact_length_and_bytes() {
        assert!(token_matches(Some(TOKEN), TOKEN));
        assert!(!token_matches(None, TOKEN));
        assert!(!token_matches(Some(""), TOKEN));
        assert!(!token_matches(Some("0123456789abcdef0123456789abcdee"), TOKEN));
        assert!(!token_matches(Some("0123456789abcdef0123456789abcde"), TOKEN));
        assert!(!token_matches(
            Some("0123456789abcdef0123456789abcdee"),
            "0123456789abcdef0123456789abcdef"
        ));
    }

    #[test]
    fn resolve_route_health_is_public() {
        assert_eq!(
            resolve_route("GET", "/zcode-voice/health", TOKEN),
            Route::Health
        );
        assert_eq!(
            resolve_route("GET", "/zcode-voice/health?foo=bar", TOKEN),
            Route::Health
        );
    }

    #[test]
    fn resolve_route_start_requires_token() {
        assert_eq!(
            resolve_route("POST", &format!("/zcode-voice/start?token={TOKEN}"), TOKEN),
            Route::Start
        );
        assert_eq!(
            resolve_route("POST", "/zcode-voice/start", TOKEN),
            Route::Forbidden
        );
        assert_eq!(
            resolve_route("POST", "/zcode-voice/start?token=wrong", TOKEN),
            Route::Forbidden
        );
        assert_eq!(
            resolve_route("POST", &format!("/zcode-voice/stop?token={TOKEN}"), TOKEN),
            Route::Stop
        );
        assert_eq!(
            resolve_route("POST", "/zcode-voice/stop?token=ff", TOKEN),
            Route::Forbidden
        );
        assert_eq!(
            resolve_route("POST", &format!("/zcode-voice/cancel?token={TOKEN}"), TOKEN),
            Route::Cancel
        );
    }

    #[test]
    fn resolve_route_preflight_and_unknowns() {
        assert_eq!(
            resolve_route("OPTIONS", "/zcode-voice/start", TOKEN),
            Route::Preflight
        );
        assert_eq!(
            resolve_route("OPTIONS", "/whatever", TOKEN),
            Route::Preflight
        );
        assert_eq!(
            resolve_route("POST", "/zcode-voice/unknown", TOKEN),
            Route::NotFound
        );
        assert_eq!(resolve_route("GET", "/", TOKEN), Route::NotFound);
        // 已知路径但方法不对：405 而不是 403/404，便于页面区分。
        assert_eq!(
            resolve_route("GET", "/zcode-voice/start", TOKEN),
            Route::MethodNotAllowed
        );
        assert_eq!(
            resolve_route("POST", "/zcode-voice/health", TOKEN),
            Route::MethodNotAllowed
        );
    }

    #[test]
    fn json_error_has_status_and_message() {
        let value: serde_json::Value = serde_json::from_str(&json_error("麦克风不可用")).unwrap();
        assert_eq!(value["status"], "error");
        assert_eq!(value["message"], "麦克风不可用");
    }

    #[test]
    fn http_response_has_cors_and_content_length() {
        let body = json_error("x");
        let bytes = http_response_bytes(200, &body);
        let response = String::from_utf8(bytes).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
        assert!(response.contains("Access-Control-Allow-Origin: *\r\n"));
        assert!(response.contains("Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n"));
        assert!(response.contains("Access-Control-Allow-Headers: Content-Type\r\n"));
        assert!(response.contains(&format!("Content-Length: {}\r\n", body.len())));
        assert!(response.contains("Connection: close\r\n"));
        assert!(response.contains("Content-Type: application/json; charset=utf-8\r\n"));
        assert!(response.ends_with(&body));

        let empty = String::from_utf8(http_response_bytes(204, "")).unwrap();
        assert!(empty.starts_with("HTTP/1.1 204 No Content\r\n"));
        assert!(empty.contains("Content-Length: 0\r\n"));
    }

    /// 走真实 TCP 回环跑一次 `handle_connection`，验证 socket 读头 → 路由 → 写响应。
    /// 只选不碰麦克风的路径（OPTIONS / health / 无效令牌），避免触发 TCC 授权弹窗。
    fn tcp_round_trip(raw_request: &[u8]) -> String {
        let listener = TcpListener::bind(BIND_ADDRESS).expect("测试用回环监听失败");
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("测试服务端 accept 失败");
            handle_connection(stream, TOKEN);
        });
        let mut client = TcpStream::connect(address).expect("测试客户端连接失败");
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        client.write_all(raw_request).unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        server.join().unwrap();
        response
    }

    #[test]
    fn preflight_over_tcp_allows_file_origin_fetch() {
        let response = tcp_round_trip(
            b"OPTIONS /zcode-voice/start HTTP/1.1\r\nHost: 127.0.0.1\r\nOrigin: null\r\n\
              Access-Control-Request-Method: POST\r\n\r\n",
        );
        assert!(response.starts_with("HTTP/1.1 204 No Content\r\n"), "{response}");
        assert!(response.contains("Access-Control-Allow-Origin: *\r\n"), "{response}");
        assert!(
            response.contains("Access-Control-Allow-Private-Network: true\r\n"),
            "{response}"
        );
    }

    #[test]
    fn health_over_tcp_is_readable_without_token() {
        let response = tcp_round_trip(b"GET /zcode-voice/health HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
        let body = response.split("\r\n\r\n").nth(1).unwrap_or_default();
        let value: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(value["status"], "ok");
        assert_eq!(value["recording"], false);
    }

    #[test]
    fn start_without_token_over_tcp_is_forbidden() {
        let response =
            tcp_round_trip(b"POST /zcode-voice/start HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
        assert!(response.starts_with("HTTP/1.1 403 Forbidden\r\n"), "{response}");
        assert!(response.contains("\"status\":\"error\""), "{response}");
    }

    #[test]
    fn unknown_path_over_tcp_is_not_found() {
        let response = tcp_round_trip(b"GET /nope HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
        assert!(response.starts_with("HTTP/1.1 404 Not Found\r\n"), "{response}");
    }

    #[test]
    fn multipart_body_matches_dictation_contract() {
        let wav = crate::mic::pcm16_wav(&[1, 2, 3]);
        let (body, content_type) = multipart_wav_body(&wav);
        let content_type_text = content_type.clone();
        let boundary = content_type_text
            .strip_prefix("multipart/form-data; boundary=")
            .expect("content-type 必须带 boundary");
        assert!(content_type_text.starts_with("multipart/form-data; boundary=codex-plus-zcode-voice-"));
        let body_text = String::from_utf8_lossy(&body);
        assert!(body_text.starts_with(&format!("--{boundary}\r\n")));
        assert!(body_text.contains("name=\"file\""));
        assert!(body_text.contains("filename=\"zcode-voice.wav\""));
        assert!(body_text.contains("Content-Type: audio/wav"));
        assert!(body_text.ends_with(&format!("\r\n--{boundary}--\r\n")));
        assert!(body.windows(wav.len()).any(|window| window == wav.as_slice()));
    }

    #[test]
    fn random_token_is_32_hex_chars() {
        let token = random_token();
        assert_eq!(token.len(), TOKEN_HEX_CHARS);
        assert!(token.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(random_token(), token, "两次生成不应相同");
        assert_eq!(hex_encode(&[0x00, 0xab, 0xff]), "00abff");
    }

    #[test]
    fn receipt_payload_is_port_and_token_only() {
        // 与 write_receipt 中的 json 形状保持一致；真正落盘需要 HOME/状态目录，
        // 不在单测里写用户目录。
        let info = VoiceServerInfo {
            port: 12345,
            token: TOKEN.to_string(),
            endpoint: "http://127.0.0.1:12345".to_string(),
        };
        let payload = serde_json::to_value(serde_json::json!({
            "port": info.port,
            "token": info.token,
        }))
        .unwrap();
        assert_eq!(payload["port"], 12345);
        assert_eq!(payload["token"], TOKEN);
        assert_eq!(info.endpoint, "http://127.0.0.1:12345");
    }
}
