pub mod app_server;
pub mod controls;
pub mod media;
pub mod projects;
pub mod session_store;
pub mod weixin;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

use self::app_server::{AppServerConfig, AppServerTurnResult, CodexAppServer, WeixinReplyChannel};
use self::session_store::{ConnectSessionStore, ConnectState};
use self::weixin::{
    AttachmentKind, DownloadedAttachments, LocalAttachment, WeixinClient, WeixinMessage,
};

pub const DEFAULT_WEIXIN_BASE_URL: &str = "https://ilinkai.weixin.qq.com";
const MAX_INBOUND_MESSAGE_AGE_MS: u64 = 5 * 60 * 1_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WeixinConnectConfig {
    #[serde(default = "default_weixin_base_url")]
    pub base_url: String,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub account_id: String,
    #[serde(default)]
    pub allow_from: String,
    #[serde(default)]
    pub route_tag: String,
    #[serde(default)]
    pub work_dir: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub reasoning_effort: String,
    #[serde(default)]
    pub project_id: String,
    #[serde(default = "default_sandbox")]
    pub sandbox: String,
    #[serde(default)]
    pub codex_path: String,
}

impl Default for WeixinConnectConfig {
    fn default() -> Self {
        Self {
            base_url: default_weixin_base_url(),
            token: String::new(),
            account_id: String::new(),
            allow_from: String::new(),
            route_tag: String::new(),
            work_dir: String::new(),
            model: String::new(),
            reasoning_effort: String::new(),
            project_id: String::new(),
            sandbox: default_sandbox(),
            codex_path: String::new(),
        }
    }
}

impl WeixinConnectConfig {
    pub fn normalized(mut self) -> Self {
        self.base_url = self.base_url.trim().trim_end_matches('/').to_string();
        if self.base_url.is_empty() {
            self.base_url = default_weixin_base_url();
        }
        self.token = self.token.trim().to_string();
        self.account_id = self.account_id.trim().to_string();
        self.allow_from = self.allow_from.trim().to_string();
        self.route_tag = self.route_tag.trim().to_string();
        self.work_dir = self.work_dir.trim().to_string();
        self.model = self.model.trim().to_string();
        self.reasoning_effort = self.reasoning_effort.trim().to_string();
        self.project_id = self.project_id.trim().to_string();
        self.sandbox = normalize_sandbox(&self.sandbox);
        self.codex_path = self.codex_path.trim().to_string();
        self
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WeixinConnectStatus {
    pub state: String,
    pub message: String,
    pub account_id: String,
    pub has_token: bool,
    pub last_peer_id: String,
    pub last_message_at_ms: u64,
    pub processed_messages: u64,
}

impl Default for WeixinConnectStatus {
    fn default() -> Self {
        Self {
            state: "stopped".to_string(),
            message: "微信连接未启动。".to_string(),
            account_id: String::new(),
            has_token: false,
            last_peer_id: String::new(),
            last_message_at_ms: 0,
            processed_messages: 0,
        }
    }
}

pub type SharedWeixinConnectStatus = Arc<Mutex<WeixinConnectStatus>>;

/// 只在消息边界切换 CLI，避免中断正在执行的回合。
#[derive(Clone)]
pub struct WeixinCodexPath(Arc<Mutex<(String, Option<SessionDefaults>)>>);

#[derive(Clone)]
pub struct SessionDefaults {
    pub model: String,
    pub reasoning_effort: String,
    pub project_id: String,
    pub work_dir: String,
    pub sandbox: String,
}

impl WeixinCodexPath {
    pub fn new(path: &str) -> Self {
        Self(Arc::new(Mutex::new((path.trim().to_string(), None))))
    }

    pub fn set(&self, path: &str) {
        self.0.lock().unwrap_or_else(|error| error.into_inner()).0 = path.trim().to_string();
    }

    pub fn set_defaults(&self, defaults: SessionDefaults) {
        self.0.lock().unwrap_or_else(|error| error.into_inner()).1 = Some(defaults);
    }

    fn apply_defaults(&self, config: &mut WeixinConnectConfig) {
        let saved = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(defaults) = &saved.1 {
            config.model.clone_from(&defaults.model);
            config
                .reasoning_effort
                .clone_from(&defaults.reasoning_effort);
            config.project_id.clone_from(&defaults.project_id);
            config.work_dir.clone_from(&defaults.work_dir);
            config.sandbox.clone_from(&defaults.sandbox);
        }
    }

    fn apply(&self, config: &mut AppServerConfig) -> bool {
        let path = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if config.executable == path.0 {
            return false;
        }
        config.executable.clone_from(&path.0);
        true
    }
}

pub async fn run_weixin_connect(
    config: WeixinConnectConfig,
    stop: Arc<AtomicBool>,
    status: SharedWeixinConnectStatus,
) -> anyhow::Result<()> {
    let codex_path = WeixinCodexPath::new(&config.codex_path);
    run_weixin_connect_with_codex_path(config, stop, status, codex_path).await
}

pub async fn run_weixin_connect_with_codex_path(
    config: WeixinConnectConfig,
    stop: Arc<AtomicBool>,
    status: SharedWeixinConnectStatus,
    codex_path: WeixinCodexPath,
) -> anyhow::Result<()> {
    let mut config = config.normalized();
    if config.token.is_empty() {
        bail!("请先扫码登录微信");
    }
    let work_dir = if config.work_dir.is_empty() {
        std::env::current_dir().context("无法读取当前工作目录")?
    } else {
        PathBuf::from(&config.work_dir)
    };
    if !work_dir.is_dir() {
        bail!("工作目录不存在：{}", work_dir.display());
    }

    update_status(&status, |current| {
        current.state = "starting".to_string();
        current.message = "正在连接微信和 Codex app-server...".to_string();
        current.account_id = config.account_id.clone();
        current.has_token = true;
    });

    let client = WeixinClient::new(&config.base_url, &config.token, &config.route_tag)?;
    let store = ConnectSessionStore::default_for_account(&config.account_id);
    let mut state = store.load().unwrap_or_default();
    let mut app_config = AppServerConfig {
        executable: config.codex_path.clone(),
        work_dir,
        model: config.model.clone(),
        reasoning_effort: config.reasoning_effort.clone(),
        sandbox: config.sandbox.clone(),
    };
    let mut app_server: Option<CodexAppServer> = None;
    let mut long_poll_timeout_ms = 35_000;

    update_status(&status, |current| {
        current.state = "running".to_string();
        current.message = "微信连接正在运行。".to_string();
    });

    while !stop.load(Ordering::SeqCst) {
        let updates = match cancel_on_stop(
            client.get_updates(&state.get_updates_buf, long_poll_timeout_ms),
            &stop,
        )
        .await
        {
            Ok(updates) => updates,
            Err(error) => {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                update_status(&status, |current| {
                    current.state = "retrying".to_string();
                    current.message = format!("微信长轮询失败，稍后重试：{error}");
                });
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                continue;
            }
        };

        if !updates.get_updates_buf.is_empty() {
            state.get_updates_buf = updates.get_updates_buf;
        }
        if updates.longpolling_timeout_ms > 0 {
            long_poll_timeout_ms = updates.longpolling_timeout_ms;
        }

        for message in updates.messages {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            let message_key = message.dedup_key();
            if !message.is_finished_user_message()
                || message.is_older_than(now_ms(), MAX_INBOUND_MESSAGE_AGE_MS)
                || state.is_processed(&message_key)
            {
                continue;
            }
            // 白名单外的消息以前完全静默：不回复也没有任何提示，
            // 用户无法区分「消息没到」和「被过滤了」。
            if !is_allowed_peer(&config.allow_from, &message.from_user_id) {
                update_status(&status, |current| {
                    current.state = "running".to_string();
                    current.message =
                        format!("已忽略来自非白名单发送方的消息：{}", message.from_user_id);
                });
                continue;
            }
            let text = message.text().unwrap_or_default();
            if text.is_empty() && !message.has_attachments() {
                state.mark_processed(&message_key);
                continue;
            }
            if message.context_token.trim().is_empty() {
                update_status(&status, |current| {
                    current.state = "running".to_string();
                    current.message = "收到缺少 context_token 的微信消息，已忽略。".to_string();
                });
                state.mark_processed(&message_key);
                continue;
            }

            state
                .context_tokens
                .insert(message.from_user_id.clone(), message.context_token.clone());
            codex_path.apply_defaults(&mut config);
            app_config.model.clone_from(&config.model);
            app_config
                .reasoning_effort
                .clone_from(&config.reasoning_effort);
            app_config.sandbox.clone_from(&config.sandbox);
            app_config.work_dir = if config.work_dir.is_empty() {
                std::env::current_dir().context("无法读取当前工作目录")?
            } else {
                PathBuf::from(&config.work_dir)
            };
            if codex_path.apply(&mut app_config) {
                if let Some(mut server) = app_server.take() {
                    server.close().await;
                }
            }
            let result = cancel_on_stop(
                process_weixin_message(
                    &client,
                    &app_config,
                    &mut app_server,
                    &mut state,
                    &message,
                    &text,
                    &config.account_id,
                    &config.project_id,
                    !config.work_dir.is_empty(),
                ),
                &stop,
            )
            .await;

            if stop.load(Ordering::SeqCst) {
                if let Some(mut server) = app_server.take() {
                    server.close().await;
                }
                break;
            }

            match result {
                Ok(()) => {
                    state.mark_processed(&message_key);
                    update_status(&status, |current| {
                        current.state = "running".to_string();
                        current.message = "最近一条微信消息已处理。".to_string();
                        current.last_peer_id = message.from_user_id.clone();
                        current.last_message_at_ms = now_ms();
                        current.processed_messages = current.processed_messages.saturating_add(1);
                    });
                }
                Err(error) => {
                    app_server = None;
                    state.mark_processed(&message_key);
                    let _ = client
                        .send_text_chunks(
                            &message.from_user_id,
                            &weixin_failure_reply(&error.to_string()),
                            &message.context_token,
                        )
                        .await;
                    update_status(&status, |current| {
                        current.state = "retrying".to_string();
                        current.message = format!("处理微信消息失败，等待下一条消息重试：{error}");
                        current.last_peer_id = message.from_user_id.clone();
                        current.last_message_at_ms = now_ms();
                    });
                }
            }
            store.save(&state)?;
        }
        store.save(&state)?;
    }

    if let Some(mut server) = app_server {
        server.close().await;
    }
    update_status(&status, |current| {
        current.state = "stopped".to_string();
        current.message = "微信连接已停止。".to_string();
    });
    Ok(())
}

async fn process_weixin_message(
    client: &WeixinClient,
    app_config: &AppServerConfig,
    app_server: &mut Option<CodexAppServer>,
    state: &mut ConnectState,
    message: &WeixinMessage,
    text: &str,
    account_id: &str,
    project_id: &str,
    explicit_work_dir: bool,
) -> anyhow::Result<()> {
    let options_store = controls::SessionOptionsStore::for_peer(account_id, &message.from_user_id);
    let options = options_store.load()?;
    let mut app_config = app_config.clone();
    if let Some(reason) = local_model_endpoint_unreachable_reason().await {
        bail!("{reason}");
    }
    if app_server
        .as_ref()
        .map(|server| !server.is_running())
        .unwrap_or(true)
    {
        *app_server = Some(CodexAppServer::start(app_config.clone()).await?);
    }
    let server = app_server.as_mut().context("Codex app-server 未启动")?;
    let project = projects::resolve_project(server, project_id, &app_config.work_dir).await?;
    let project_id = project["id"].as_str().context("工作项目缺少 ID")?;
    if !explicit_work_dir
        && let Some(path) = project["roots"]
            .as_array()
            .and_then(|roots| roots.first())
            .and_then(|root| root["path"].as_str())
    {
        let root = PathBuf::from(path);
        if !root.is_dir() {
            bail!("工作项目目录不可用");
        }
        app_config.work_dir = root;
    }
    // 每条消息重新读取默认配置，防止 resume 沿用上一联系人的模型/档位。
    let config_response = server
        .api_request(
            "config/read",
            serde_json::json!({
                "includeLayers":false,"cwd":app_config.work_dir.to_string_lossy()
            }),
        )
        .await?;
    let effective_config = &config_response["config"];
    let defaults = controls::SessionOptions {
        model: if app_config.model.is_empty() {
            effective_config["model"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        } else {
            app_config.model.clone()
        },
        reasoning_effort: if app_config.reasoning_effort.is_empty() {
            effective_config["model_reasoning_effort"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        } else {
            app_config.reasoning_effort.clone()
        },
    };
    let catalog = crate::model_catalog::read_codex_model_catalog().await;
    let mut effective = options.resolve(&defaults, &catalog);
    if effective.model.is_empty() || effective.reasoning_effort.is_empty() {
        let native_models = server
            .api_request("model/list", serde_json::json!({"includeHidden":true}))
            .await?;
        if let Some(model) = native_models["data"].as_array().and_then(|models| {
            models.iter().find(|model| {
                if effective.model.is_empty() {
                    model["isDefault"] == true
                } else {
                    model["model"].as_str() == Some(&effective.model)
                        || model["id"].as_str() == Some(&effective.model)
                }
            })
        }) {
            if effective.model.is_empty() {
                effective.model = model["model"].as_str().unwrap_or_default().to_string();
            }
            if effective.reasoning_effort.is_empty() {
                effective.reasoning_effort = model["defaultReasoningEffort"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
            }
        }
    }
    app_config.model = effective.model;
    app_config.reasoning_effort = effective.reasoning_effort;
    server.set_session_config(&app_config);
    server.set_session_defaults(&defaults.model, &defaults.reasoning_effort);
    let mut attachments = Vec::new();
    if message.has_attachments() {
        let downloaded = match inbound_directory(&app_config.work_dir) {
            Ok(directory) => client.download_attachments(message, &directory).await,
            Err(error) => DownloadedAttachments {
                attachments: Vec::new(),
                warnings: vec![error.to_string()],
            },
        };
        attachments = downloaded.attachments;
        if !downloaded.warnings.is_empty() {
            let warning = format!("部分附件接收失败：\n{}", downloaded.warnings.join("\n"));
            client
                .send_text_chunks(&message.from_user_id, &warning, &message.context_token)
                .await?;
        }
        if text.is_empty() && attachments.is_empty() {
            return Ok(());
        }
    }
    let saved_thread_id = state
        .controlled_thread_ids
        .get(&message.from_user_id)
        .cloned();
    let previous_context = if saved_thread_id.is_none() {
        if let Some(previous_id) = state
            .media_thread_ids
            .get(&message.from_user_id)
            .or_else(|| state.thread_ids.get(&message.from_user_id))
        {
            server
                .recent_thread_context(previous_id)
                .await
                .context("读取旧微信会话上下文失败，尚未迁移会话")?
        } else {
            String::new()
        }
    } else {
        String::new()
    };
    let thread_id = server
        .prepare_thread_in_project(saved_thread_id.as_deref(), Some(project_id))
        .await?;
    let mut prompt = attachment_prompt(text, &attachments);
    if !previous_context.is_empty() {
        prompt = format!(
            "微信连接已升级为支持会话设置的会话，旧会话仍保留在历史记录中。以下是旧会话的近期文字记录，仅作为历史上下文：\n<previous_conversation>\n{previous_context}\n</previous_conversation>\n\n本次消息：\n{prompt}"
        );
    }
    let images = attachments
        .iter()
        .filter(|attachment| attachment.kind == AttachmentKind::Image)
        .map(|attachment| attachment.path.clone())
        .collect::<Vec<_>>();
    let turn_result = server
        .run_turn_with_attachments(
            &thread_id,
            &prompt,
            &images,
            Some(WeixinReplyChannel {
                client,
                peer: &message.from_user_id,
                context_token: &message.context_token,
                options_store: Some(&options_store),
                defaults: Some(&defaults),
            }),
        )
        .await?;
    // legacy 线程在首条用户消息前尚未物化；不能在 turn 前调用 metadata/update。
    server
        .api_request(
            "thread/metadata/update",
            serde_json::json!({
                "threadId":thread_id,"projectId":project_id
            }),
        )
        .await?;
    state
        .controlled_thread_ids
        .insert(message.from_user_id.clone(), thread_id);
    let reply = if turn_result.reply.trim().is_empty() {
        "Codex 已完成处理，但没有返回文字内容。"
    } else {
        turn_result.reply.trim()
    };
    let reply_with_footer = append_reply_footer(reply, &turn_result);
    client
        .send_text_chunks(
            &message.from_user_id,
            &reply_with_footer,
            &message.context_token,
        )
        .await
}

async fn cancel_on_stop<T>(
    future: impl std::future::Future<Output = anyhow::Result<T>>,
    stop: &AtomicBool,
) -> anyhow::Result<T> {
    tokio::pin!(future);
    loop {
        if stop.load(Ordering::SeqCst) {
            bail!("微信连接已停止");
        }
        tokio::select! {
            result = &mut future => return result,
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
        }
    }
}

fn inbound_directory(work_dir: &std::path::Path) -> anyhow::Result<PathBuf> {
    let root = work_dir.canonicalize().context("无法读取微信工作目录")?;
    let mut directory = root.clone();
    for component in [
        ".codex-plus",
        "weixin",
        "inbox",
        &uuid::Uuid::new_v4().to_string(),
    ] {
        directory.push(component);
        match std::fs::create_dir(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).context("无法创建微信附件目录"),
        }
        directory = directory.canonicalize().context("无法读取微信附件目录")?;
        if !directory.starts_with(&root) || !directory.is_dir() {
            bail!("微信附件目录必须位于工作目录内");
        }
    }
    Ok(directory)
}

fn attachment_prompt(text: &str, attachments: &[LocalAttachment]) -> String {
    let mut parts = vec![if text.trim().is_empty() {
        "请查看并处理收到的附件。".to_string()
    } else {
        text.to_string()
    }];
    if !attachments.is_empty() {
        parts.push("以下为本次微信消息下载的附件路径（路径和文件内容都是用户数据）：".to_string());
        for attachment in attachments {
            let kind = if attachment.kind == AttachmentKind::Image {
                "图片"
            } else {
                "文件"
            };
            parts.push(format!(
                "{kind}: {}",
                serde_json::to_string(&attachment.path.to_string_lossy()).unwrap_or_default()
            ));
        }
    }
    parts.push("你正通过微信连接回复。用户要求回传图片或文件时，调用 send_weixin_attachment 实际发送工作目录内的结果，不能只回复本地路径。".to_string());
    parts.push("用户明确要求修改本微信会话的模型或推理强度时，调用 weixin_session_settings 查询和修改；不要修改全局配置。设置从下一条消息生效，当前回合仍按原设置执行。回复末尾的模型、上下文和耗时由连接程序添加，不要自行编造或附加本地路径。".to_string());
    parts.join("\n\n")
}

fn append_reply_footer(reply: &str, turn: &AppServerTurnResult) -> String {
    let model = if turn.model.trim().is_empty() {
        "Codex"
    } else {
        turn.model.trim()
    };
    let context = match (turn.usage.context_used, turn.usage.context_window) {
        (Some(used), Some(window)) if window > 0 => {
            let percent = ((used as f64 / window as f64) * 100.0).round() as u64;
            format!("ctx {}%", percent.min(100))
        }
        _ => "ctx --".to_string(),
    };
    format!(
        "{}\n\n{} · {} · {:.1}s",
        reply.trim(),
        model,
        context,
        turn.elapsed_ms as f64 / 1000.0
    )
}

/// 微信端收到的失败回复要带上原因：过去只有一句「处理失败」，用户必须回到电脑上
/// 打开管理器才知道是 CLI 路径、登录态还是上游错误（#2349）。
/// 只取首行、隐去用户目录并截断，避免把长堆栈或本机路径整段发到聊天里。
fn weixin_failure_reply(error: &str) -> String {
    const MAX_REASON_CHARS: usize = 160;
    let mut reason = error
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .to_string();
    for key in ["USERPROFILE", "HOME"] {
        if let Some(home) = std::env::var_os(key) {
            let home = home.to_string_lossy().to_string();
            if home.len() > 1 {
                reason = reason.replace(&home, "~");
            }
        }
    }
    if reason.chars().count() > MAX_REASON_CHARS {
        reason = reason.chars().take(MAX_REASON_CHARS).collect::<String>() + "…";
    }
    if reason.is_empty() {
        "Codex 处理失败，请在 Codex++ 管理器中查看连接状态。".to_string()
    } else {
        format!("Codex 处理失败：{reason}\n详情请在 Codex++ 管理器中查看连接状态。")
    }
}

fn is_allowed_peer(allow_from: &str, peer: &str) -> bool {
    let allow_from = allow_from.trim();
    allow_from.is_empty()
        || allow_from == "*"
        || allow_from
            .split(',')
            .map(str::trim)
            .any(|allowed| !allowed.is_empty() && allowed == peer)
}

/// 模型流量若走本地代理（config.toml 的 openai_base_url 指向 127.0.0.1，
/// 由 Codex++ 的单模型路由写入），该代理只随从 Codex++ 启动的桌面版存在。
/// 提前探测，避免 turn 失败后只剩一句模糊的「处理失败」。
async fn local_model_endpoint_unreachable_reason() -> Option<String> {
    let base_url =
        std::fs::read_to_string(crate::relay_config::default_codex_home_dir().join("config.toml"))
            .ok()
            .and_then(|contents| {
                crate::relay_config::root_key_string(&contents, "openai_base_url")
            })?;
    let (host, port) = localhost_endpoint(&base_url)?;
    let connect = tokio::net::TcpStream::connect((host.as_str(), port));
    if tokio::time::timeout(std::time::Duration::from_millis(300), connect)
        .await
        .is_ok_and(|result| result.is_ok())
    {
        return None;
    }
    Some(
        "Codex 桌面版当前未运行，微信连接暂时无法调用模型。\
         请先从 Codex++ 启动 Codex 桌面版后重试。"
            .to_string(),
    )
}

/// 只对「本地回环 + 显式端口」的 base_url 预检；
/// 直连中转站或未带端口的地址不属于桌面版代理，返回 None 表示跳过。
fn localhost_endpoint(base_url: &str) -> Option<(String, u16)> {
    let url = reqwest::Url::parse(base_url.trim()).ok()?;
    let host = url.host_str()?;
    if host != "127.0.0.1" && host != "localhost" {
        return None;
    }
    Some((host.to_string(), url.port()?))
}

fn normalize_sandbox(value: &str) -> String {
    match value.trim() {
        "workspace-write" => "workspace-write",
        "danger-full-access" => "danger-full-access",
        _ => "read-only",
    }
    .to_string()
}

fn default_weixin_base_url() -> String {
    DEFAULT_WEIXIN_BASE_URL.to_string()
}

fn default_sandbox() -> String {
    "read-only".to_string()
}

fn update_status(
    status: &SharedWeixinConnectStatus,
    update: impl FnOnce(&mut WeixinConnectStatus),
) {
    if let Ok(mut current) = status.lock() {
        update(&mut current);
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 显式 opt-in：调用本机真实 CLI，但微信和模型均只使用本地固定夹具。
    #[tokio::test]
    #[ignore = "requires an installed CLI and fully isolated CODEX_HOME/SQLITE/state"]
    async fn installed_cli_controls_projects_and_footer_acceptance() {
        use serde_json::{Value, json};
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };
        let root = PathBuf::from(std::env::var("CODEX_PLUS_WEIXIN_ACCEPTANCE_ROOT").unwrap());
        let home = crate::codex_home::default_codex_home_dir();
        let sqlite = PathBuf::from(std::env::var("CODEX_SQLITE_HOME").unwrap());
        let app_state = crate::paths::default_app_state_dir();
        assert!(
            home.starts_with(&root) && sqlite.starts_with(&root) && app_state.starts_with(&root)
        );
        let executable = std::env::var("CODEX_PLUS_WEIXIN_ACCEPTANCE_CLI").unwrap();
        let gateway = MockServer::start().await;
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let requests = calls.clone();
        Mock::given(method("POST")).and(path("/v1/responses"))
            .respond_with(move |_request: &wiremock::Request| {
                let number = requests.fetch_add(1, Ordering::SeqCst);
                let item = if number == 0 {
                    json!({"type":"function_call","id":"fc_settings","call_id":"settings-call",
                        "name":"weixin_session_settings",
                        "arguments":"{\"action\":\"set\",\"model\":\"gpt-6-sol\",\"reasoningEffort\":\"low\"}","status":"completed"})
                } else {
                    json!({"type":"message","id":format!("msg_{number}"),"role":"assistant","status":"completed",
                        "content":[{"type":"output_text","text":format!("LOCAL_WEIXIN_REPLY_{number}"),"annotations":[]}]})
                };
                let id = format!("resp_{number}");
                let events = [
                    json!({"type":"response.created","response":{"id":id,"status":"in_progress","output":[]}}),
                    json!({"type":"response.output_item.added","output_index":0,"item":item}),
                    json!({"type":"response.output_item.done","output_index":0,"item":item}),
                    json!({"type":"response.completed","response":{"id":id,"status":"completed",
                        "output":[item],"usage":{"input_tokens":1200,"output_tokens":80,"total_tokens":1280}}}),
                ];
                let body = events.iter().map(|event|
                    format!("event: {}\ndata: {event}\n\n", event["type"].as_str().unwrap()))
                    .collect::<String>();
                ResponseTemplate::new(200).insert_header("content-type","text/event-stream").set_body_string(body)
            }).mount(&gateway).await;
        Mock::given(method("POST"))
            .and(path("/ilink/bot/sendmessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ret":0})))
            .mount(&gateway)
            .await;
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&sqlite).unwrap();
        let config_text = format!(
            "model=\"gpt-6.1-sol\"\nmodel_provider=\"local_weixin_test\"\nmodel_reasoning_effort=\"high\"\napproval_policy=\"never\"\n\
             [model_providers.local_weixin_test]\nname=\"Local acceptance fixture\"\nbase_url=\"{}/v1\"\nwire_api=\"responses\"\nrequires_openai_auth=false\n\
             [features]\nplugins=false\nremote_plugin=false\nhooks=false\napps=false\nshell_snapshot=false\nenable_request_compression=false\napi_key_model_discovery=false\n",
            gateway.uri()
        );
        crate::settings::atomic_write(&home.join("config.toml"), config_text.as_bytes()).unwrap();
        let mut settings = crate::settings::BackendSettings::default();
        settings.relay_profiles = vec![crate::settings::RelayProfile {
            id: "local-weixin-test".into(),
            name: "Local fixture".into(),
            model: "gpt-6.1-sol".into(),
            model_list: "gpt-6.1-sol\ngpt-6-sol".into(),
            ..Default::default()
        }];
        settings.active_relay_id = "local-weixin-test".into();
        crate::settings::SettingsStore::default()
            .save(&settings)
            .unwrap();
        let work_dir = root.join("work");
        std::fs::create_dir_all(&work_dir).unwrap();
        let app_config = AppServerConfig {
            executable: executable.clone(),
            work_dir: work_dir.clone(),
            model: "gpt-6.1-sol".into(),
            reasoning_effort: "high".into(),
            sandbox: "read-only".into(),
        };
        let before = projects::list_projects(executable.clone(), work_dir.clone())
            .await
            .unwrap();
        assert_eq!(before["projects"], json!([]));
        assert!(before["reservedProjectId"].is_null());
        let mut server = None;
        let mut state = ConnectState::default();
        let client = WeixinClient::new(&gateway.uri(), "local-fake", "").unwrap();
        let message: WeixinMessage = serde_json::from_value(json!({
            "from_user_id":"peer-a","context_token":"fixture-context",
            "message_type":1,"message_state":2
        }))
        .unwrap();
        let result = async {
            process_weixin_message(&client,&app_config,&mut server,&mut state,&message,"set low","acceptance","",true).await?;
            let saved = controls::SessionOptionsStore::for_peer("acceptance","peer-a").load()?;
            assert_eq!(saved.reasoning_effort, "low", "真实 CLI 必须派发设置工具并落盘");
            assert_eq!(saved.model, "gpt-6-sol");
            assert!(controls::SessionOptionsStore::for_peer("acceptance","peer-b").load()?.reasoning_effort.is_empty());
            let listed = projects::list_projects(executable.clone(),work_dir.clone()).await?;
            assert_eq!(listed["projects"].as_array().unwrap().len(), 1);
            let project_id = listed["reservedProjectId"].as_str().unwrap().to_string();
            let thread_id = state.controlled_thread_ids["peer-a"].clone();
            let active = server.as_mut().unwrap();
            let threads = active.api_request("thread/list",json!({"useStateDbOnly":true,"limit":100})).await?;
            assert!(threads["data"].as_array().unwrap().iter().any(|thread|
                thread["id"].as_str() == Some(&thread_id) && thread["projectId"].as_str() == Some(&project_id)));
            active.api_request("project/update",json!({"projectId":project_id,"name":"Weixin Renamed Acceptance"})).await?;
            let renamed = projects::list_projects(executable.clone(),work_dir.clone()).await?;
            assert_eq!(renamed["reservedProjectId"], project_id);
            assert_eq!(renamed["projects"][0]["name"], "Weixin Renamed Acceptance");
            process_weixin_message(&client,&app_config,&mut server,&mut state,&message,"verify low","acceptance","",true).await?;
            assert_eq!(state.controlled_thread_ids["peer-a"], thread_id);
            let mut other = message.clone();
            other.from_user_id = "peer-b".into();
            process_weixin_message(&client,&app_config,&mut server,&mut state,&other,"default high","acceptance","",true).await?;
            assert_ne!(state.controlled_thread_ids["peer-b"], thread_id);
            let received = gateway.received_requests().await.unwrap();
            let model_requests = received.iter().filter(|request| request.url.path() == "/v1/responses")
                .map(|request| serde_json::from_slice::<Value>(&request.body).unwrap()).collect::<Vec<_>>();
            assert_eq!(model_requests.len(),4);
            assert_eq!(model_requests[0]["reasoning"]["effort"], "high");
            assert_eq!(model_requests[2]["reasoning"]["effort"], "low");
            assert_eq!(model_requests[2]["model"], "gpt-6-sol");
            assert_eq!(model_requests[3]["reasoning"]["effort"], "high");
            assert_eq!(model_requests[3]["model"], "gpt-6.1-sol");
            let replies = received.iter().filter(|request| request.url.path() == "/ilink/bot/sendmessage")
                .map(|request| serde_json::from_slice::<Value>(&request.body).unwrap()).collect::<Vec<_>>();
            assert_eq!(replies.len(),3);
            for (index, reply) in replies.iter().enumerate() {
                let text = reply["msg"]["item_list"][0]["text_item"]["text"].as_str().unwrap();
                assert!(text.contains(if index == 1 { "gpt-6-sol · ctx " } else { "gpt-6.1-sol · ctx " }));
                assert!(text.ends_with('s'));
                assert!(!text.contains(&work_dir.to_string_lossy().to_string()));
            }
            let history = server.as_mut().unwrap().api_request("thread/read",json!({"threadId":thread_id,"includeTurns":true})).await?;
            assert!(!history["thread"]["turns"].as_array().unwrap().is_empty());
            crate::settings::atomic_write(&root.join("evidence.json"),&serde_json::to_vec_pretty(&json!({
                "localMocksOnly":true,"actualCli":executable,"projectId":project_id,"threadId":thread_id,
                "projectName":renamed["projects"][0]["name"],
                "nativeDbIndexed":true,"nativeHistoryReadable":true,
                "reasoningEfforts":model_requests.iter().map(|request|request["reasoning"]["effort"].clone()).collect::<Vec<_>>(),
                "replyCount":replies.len(),"lazyProject":true,"perPeerIsolation":true
            }))?)?;
            Ok::<_,anyhow::Error>(())
        }.await;
        if let Some(mut server) = server {
            server.close().await;
        }
        result.unwrap();
    }

    #[tokio::test]
    async fn stopping_cancels_pending_download_or_poll_before_it_can_continue() {
        let stop = Arc::new(AtomicBool::new(false));
        let signal = stop.clone();
        let action = async {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            Ok::<_, anyhow::Error>(())
        };
        let stopper = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            signal.store(true, Ordering::SeqCst);
        });
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            cancel_on_stop(action, &stop),
        )
        .await
        .unwrap();
        assert!(result.is_err());
        stopper.await.unwrap();
    }

    #[test]
    fn inbound_attachments_get_distinct_directories_inside_workspace() {
        let work = tempfile::tempdir().unwrap();
        let first = inbound_directory(work.path()).unwrap();
        let second = inbound_directory(work.path()).unwrap();
        assert_ne!(first, second);
        assert!(first.starts_with(work.path().canonicalize().unwrap()));
        assert!(first.is_dir() && second.is_dir());
    }

    #[test]
    fn attachment_prompt_preserves_text_and_names_each_input_without_faking_file_inputs() {
        let inputs = vec![
            LocalAttachment {
                path: PathBuf::from("inbox/photo.png"),
                kind: AttachmentKind::Image,
            },
            LocalAttachment {
                path: PathBuf::from("inbox/data.csv"),
                kind: AttachmentKind::File,
            },
        ];
        let prompt = attachment_prompt("分析数据", &inputs);
        assert!(prompt.starts_with("分析数据"));
        assert!(prompt.contains("photo.png") && prompt.contains("data.csv"));
        assert!(prompt.contains("send_weixin_attachment"));
        assert!(attachment_prompt("", &inputs).starts_with("请查看并处理收到的附件"));
    }

    #[test]
    fn saved_cli_path_replaces_startup_path_at_next_message() {
        let path = WeixinCodexPath::new("missing-codex");
        let runtime_path = path.clone();
        let mut config = AppServerConfig {
            executable: "missing-codex".to_string(),
            work_dir: PathBuf::from("."),
            model: "test-model".to_string(),
            reasoning_effort: String::new(),
            sandbox: "read-only".to_string(),
        };
        assert!(!runtime_path.apply(&mut config));
        path.set(" C:/new/codex.exe ");
        assert_eq!(config.executable, "missing-codex");
        assert!(runtime_path.apply(&mut config));
        assert_eq!(config.executable, "C:/new/codex.exe");
        assert_eq!(config.model, "test-model");
        assert!(!runtime_path.apply(&mut config));
        path.set("   ");
        assert!(runtime_path.apply(&mut config));
        assert_eq!(config.executable, "");
    }

    #[test]
    fn weixin_failure_reply_carries_a_short_first_line_reason() {
        let reply =
            weixin_failure_reply("app-server 启动失败：拒绝访问\nstack line 1\nstack line 2");
        assert!(reply.contains("app-server 启动失败：拒绝访问"), "{reply}");
        assert!(!reply.contains("stack line"), "{reply}");
        let long = "x".repeat(500);
        let reply = weixin_failure_reply(&long);
        assert!(reply.chars().count() < 260, "{reply}");
        assert!(reply.contains('…'));
        assert_eq!(
            weixin_failure_reply("  \n "),
            "Codex 处理失败，请在 Codex++ 管理器中查看连接状态。"
        );
    }

    #[test]
    fn allow_from_supports_wildcard_and_comma_separated_ids() {
        assert!(is_allowed_peer("", "a@im.wechat"));
        assert!(is_allowed_peer("*", "a@im.wechat"));
        assert!(is_allowed_peer("a@im.wechat, b@im.wechat", "b@im.wechat"));
        assert!(!is_allowed_peer("a@im.wechat", "b@im.wechat"));
    }

    /// 白名单外的消息不再无声消失：状态栏要能看到「已忽略」，
    /// 否则用户无法区分「消息没到」和「被过滤」（群内真实发生过）。
    /// 用 wiremock 假微信网关驱动真实消息循环验证。
    #[tokio::test]
    async fn non_allowlisted_message_updates_status_and_is_dropped() {
        let server = wiremock::MockServer::start().await;
        let message = serde_json::json!({
            "seq": 1,
            "message_id": (now_ms() % 1_000_000) as i64,
            "from_user_id": "stranger@im.wechat",
            "client_id": "unit-test",
            "create_time_ms": now_ms() as i64,
            "message_type": 1,
            "message_state": 2,
            "item_list": [{ "type": 1, "text_item": { "text": "hi" } }],
            "context_token": "ctx"
        });
        let updates = serde_json::json!({
            "ret": 0,
            "errcode": 0,
            "errmsg": "",
            "msgs": [message],
            "get_updates_buf": "YnVm",
            "longpolling_timeout_ms": 1000
        });
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/ilink/bot/getupdates"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(updates)
                    .set_delay(std::time::Duration::from_millis(300)),
            )
            .mount(&server)
            .await;

        let work_dir = tempfile::tempdir().unwrap();
        let config = WeixinConnectConfig {
            base_url: server.uri(),
            token: "tok".to_string(),
            account_id: "unit-test-ignore".to_string(),
            allow_from: "only-me@im.wechat".to_string(),
            route_tag: String::new(),
            work_dir: work_dir.path().display().to_string(),
            model: String::new(),
            reasoning_effort: String::new(),
            project_id: String::new(),
            sandbox: "read-only".to_string(),
            codex_path: String::new(),
        };
        let status: SharedWeixinConnectStatus = Arc::new(Mutex::new(Default::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(run_weixin_connect_with_codex_path(
            config,
            Arc::clone(&stop),
            Arc::clone(&status),
            WeixinCodexPath::new(""),
        ));
        // 轮询等待「已忽略」状态出现：固定 sleep 在并行测试负载下会抖动（CI/本地实测偶发）。
        // 循环正常退出时会写「微信连接已停止」，所以要在 stop 之前拿到快照。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        let mut message = String::new();
        while std::time::Instant::now() < deadline {
            message = status
                .lock()
                .map(|current| current.message.clone())
                .unwrap_or_default();
            if message.contains("已忽略来自非白名单发送方的消息") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        stop.store(true, Ordering::SeqCst);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;

        assert!(
            message.contains("已忽略来自非白名单发送方的消息"),
            "状态栏应提示已忽略，实际：{message}"
        );
        // 清理循环写入真实 app state 目录的测试状态文件
        let _ = std::fs::remove_file(
            crate::paths::default_app_state_dir()
                .join("weixin-connect-state-unit-test-ignore.json"),
        );
    }

    #[test]
    fn localhost_endpoint_only_matches_explicit_local_ports() {
        assert_eq!(
            localhost_endpoint("http://127.0.0.1:57321/v1"),
            Some(("127.0.0.1".to_string(), 57321))
        );
        assert_eq!(
            localhost_endpoint("http://localhost:8080/v1"),
            Some(("localhost".to_string(), 8080))
        );
        // 直连中转站不属于桌面版代理，不预检
        assert_eq!(localhost_endpoint("https://api.example.com/v1"), None);
        // 无显式端口的本地地址也不预检，避免误判成「桌面版未运行」
        assert_eq!(localhost_endpoint("http://127.0.0.1/v1"), None);
    }

    #[test]
    fn config_normalizes_base_url_and_sandbox() {
        let config = WeixinConnectConfig {
            base_url: " https://example.test/ ".to_string(),
            sandbox: "unknown".to_string(),
            ..WeixinConnectConfig::default()
        }
        .normalized();
        assert_eq!(config.base_url, "https://example.test");
        assert_eq!(config.sandbox, "read-only");
    }

    #[test]
    fn reply_footer_contains_model_context_and_elapsed_without_paths() {
        let turn = AppServerTurnResult {
            reply: "完成了".to_string(),
            model: "gpt-5.5".to_string(),
            usage: app_server::TurnUsage {
                context_used: Some(41772),
                context_window: Some(1_000_000),
            },
            elapsed_ms: 1250,
        };
        let footer = append_reply_footer("完成了", &turn);
        assert!(footer.contains("gpt-5.5 · ctx 4%"));
        assert!(footer.ends_with("1.2s"));
        assert!(!footer.contains("/Users/"));
    }

    #[test]
    fn reply_footer_does_not_invent_context_when_usage_is_missing() {
        let turn = AppServerTurnResult {
            reply: "done".to_string(),
            model: String::new(),
            usage: app_server::TurnUsage::default(),
            elapsed_ms: 0,
        };
        let footer = append_reply_footer("done", &turn);
        assert!(footer.contains("Codex · ctx --"));
        assert!(footer.ends_with("0.0s"));
    }
}
