use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdout, Command};

use super::weixin::{AttachmentKind, WeixinClient};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// app-server 起不来时保留多少行 stderr 用于报错。
const STDERR_TAIL_LINES: usize = 8;
const TURN_TIMEOUT: Duration = Duration::from_secs(600);
const ATTACHMENT_TOOL: &str = "send_weixin_attachment";
const MAX_ATTACHMENTS_PER_TURN: usize = 10;
const MAX_PENDING_TURN_MESSAGES: usize = 256;

pub struct WeixinReplyChannel<'a> {
    pub client: &'a WeixinClient,
    pub peer: &'a str,
    pub context_token: &'a str,
    pub options_store: Option<&'a super::controls::SessionOptionsStore>,
    pub defaults: Option<&'a super::controls::SessionOptions>,
}

#[derive(Debug, Clone)]
pub struct AppServerConfig {
    pub executable: String,
    pub work_dir: PathBuf,
    pub model: String,
    pub reasoning_effort: String,
    pub allow_outbound_attachments: bool,
    pub sandbox: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnUsage {
    /// The active context size reported by app-server for the latest turn.
    pub context_used: Option<u64>,
    pub context_window: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppServerTurnResult {
    pub reply: String,
    pub model: String,
    pub usage: TurnUsage,
    pub elapsed_ms: u64,
}

pub struct CodexAppServer {
    child: Child,
    stdin: tokio::process::ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    next_id: u64,
    running: bool,
    named_permissions: bool,
    config: AppServerConfig,
    #[cfg(test)]
    test_catalog: Option<Value>,
    session_defaults: super::controls::SessionOptions,
}

fn collect_stderr_tail(sink: &Arc<Mutex<VecDeque<String>>>) -> String {
    sink.lock()
        .map(|lines| lines.iter().cloned().collect::<Vec<_>>().join("；"))
        .unwrap_or_default()
}

/// 留空或托管路径失效时，Windows 选用桌面版维护的标准 CLI，
/// 其余平台沿用 PATH 里的 codex（macOS 桌面版内置路径本来就可执行）。
fn resolve_startup_executable(configured: &str) -> String {
    #[cfg(windows)]
    {
        return resolve_windows_startup_executable(
            configured,
            std::env::var_os("LOCALAPPDATA").as_deref().map(Path::new),
        );
    }
    #[cfg(not(windows))]
    {
        if !configured.is_empty() {
            return configured.to_string();
        }
        "codex".to_string()
    }
}

#[cfg(windows)]
fn resolve_windows_startup_executable(configured: &str, local_appdata: Option<&Path>) -> String {
    // 桌面版更新会替换哈希目录；仅恢复已失效的托管路径，保留自定义路径的校验。
    let missing_managed = !configured.is_empty()
        && !Path::new(configured).exists()
        && local_appdata.is_some_and(|local| {
            let bin = local.join("OpenAI").join("Codex").join("bin");
            let prefix = format!(
                "{}\\",
                bin.to_string_lossy()
                    .replace('/', "\\")
                    .to_ascii_lowercase()
            );
            let normalized = configured.replace('/', "\\").to_ascii_lowercase();
            normalized.strip_prefix(&prefix).is_some_and(|suffix| {
                let parts: Vec<_> = suffix.split('\\').collect();
                parts == ["codex.exe"]
                    || (parts.len() == 2
                        && !parts[0].is_empty()
                        && parts[0].bytes().all(|byte| byte.is_ascii_hexdigit())
                        && parts[1] == "codex.exe")
            })
        });
    if configured.is_empty() || missing_managed {
        if let Some(path) = crate::app_paths::find_desktop_managed_codex_cli_from_var(local_appdata)
        {
            return path.to_string_lossy().into_owned();
        }
        if configured.is_empty() {
            return "codex".to_string();
        }
    }
    configured.to_string()
}

/// 启动前先把「路径本身就不对」的情况挑出来。
///
/// 之前不管什么原因失败，用户只会看到一句「请检查 Codex CLI 路径」——填的是目录、
/// 文件不存在、没有执行权限、甚至 app-server 协议不兼容，全都是这一句，
/// 于是 #1879 下面积了十几条「到底该填什么路径」。
///
/// 裸命令名（codex）交给 PATH 解析，这里只校验看起来像路径的输入。
fn validate_codex_executable(executable: &str) -> anyhow::Result<()> {
    let looks_like_path = executable.contains('/')
        || executable.contains('\\')
        || Path::new(executable).is_absolute();
    if !looks_like_path {
        return Ok(());
    }
    if crate::app_paths::is_windows_store_cli_path(executable) {
        bail!(
            "Codex CLI 路径位于系统保护的安装目录内，Codex++ 无法直接运行它：{executable}\n\
             请清空「Codex CLI 路径」保存（留空时自动查找），\
             或点击「使用桌面版内置 CLI」重新选择。"
        );
    }
    let path = Path::new(executable);
    if !path.exists() {
        bail!(
            "Codex CLI 路径不存在：{executable}\n请填 Codex CLI 可执行文件的完整路径。\
             macOS 桌面版通常在 /Applications/ChatGPT.app/Contents/Resources/codex；\
             npm 全局安装可用 `which codex` / `where codex` 查看。"
        );
    }
    if path.is_dir() {
        bail!(
            "Codex CLI 路径指向的是目录而不是可执行文件：{executable}\n\
             请去掉结尾的路径分隔符，或补上具体的可执行文件名。"
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = std::fs::metadata(path)
            && metadata.permissions().mode() & 0o111 == 0
        {
            bail!("Codex CLI 没有执行权限：{executable}");
        }
    }
    Ok(())
}

/// spawn 失败按错误类别给出对应提示，不再一律归成「请检查路径」。
fn spawn_failure_hint(executable: &str, kind: std::io::ErrorKind) -> &'static str {
    match kind {
        std::io::ErrorKind::NotFound => {
            if cfg!(windows) && !executable.contains('/') && !executable.contains('\\') {
                // CreateProcess 解析裸命令名只补 .exe，npm 生成的 codex.cmd 不在其列（#1879）
                "：找不到可运行的 Codex CLI；请点击「使用桌面版内置 CLI」自动填入，\
                 或将「Codex CLI 路径」留空自动查找"
            } else {
                "：找不到该文件；填 Codex CLI 可执行文件的完整路径，或确保 codex 在 PATH 里"
            }
        }
        std::io::ErrorKind::PermissionDenied
            if crate::app_paths::is_windows_store_cli_path(executable) =>
        {
            "：此路径位于系统保护的安装目录，无法直接运行；\
             请清空「Codex CLI 路径」自动查找，或点击「使用桌面版内置 CLI」重新选择"
        }
        std::io::ErrorKind::PermissionDenied => "：没有执行权限",
        _ => "",
    }
}

impl CodexAppServer {
    pub async fn start(mut config: AppServerConfig) -> anyhow::Result<Self> {
        config.work_dir = std::path::absolute(&config.work_dir).context("无法解析微信工作目录")?;
        let executable = resolve_startup_executable(config.executable.trim());
        validate_codex_executable(&executable)?;
        let mut command = Command::new(&executable);
        command
            .arg("app-server")
            .env_remove("CODEX_APP_TOOLS_PIPE_PATH")
            .current_dir(&config.work_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Windows 下不设 CREATE_NO_WINDOW 时，app-server 会拿到可见控制台并弹出
        // CMD/Windows Terminal 窗口，它派生的 MCP 子进程（memory 等）也会各起一个
        // conhost。仓库其它 spawn 路径都已设该标志，仅此处遗漏（issue #2048）。
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(crate::windows_integration::CREATE_NO_WINDOW);
        }
        let mut child = command.spawn().map_err(|error| {
            let hint = spawn_failure_hint(&executable, error.kind());
            anyhow::anyhow!("无法启动 Codex app-server（{executable}）{hint}（{error}）")
        })?;
        let stdin = child
            .stdin
            .take()
            .context("无法连接 Codex app-server stdin")?;
        let stdout = child
            .stdout
            .take()
            .context("无法连接 Codex app-server stdout")?;
        // 别把 stderr 读了就扔——app-server 起不来时，真正的原因只在这里。
        // 留最近几行，初始化失败时附到错误信息上。
        let stderr_tail = Arc::new(Mutex::new(VecDeque::<String>::new()));
        if let Some(stderr) = child.stderr.take() {
            let sink = Arc::clone(&stderr_tail);
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let line = line.trim().to_string();
                    if line.is_empty() {
                        continue;
                    }
                    if let Ok(mut sink) = sink.lock() {
                        if sink.len() == STDERR_TAIL_LINES {
                            sink.pop_front();
                        }
                        sink.push_back(line);
                    }
                }
            });
        }

        let mut server = Self {
            child,
            stdin,
            stdout: BufReader::new(stdout).lines(),
            next_id: 1,
            running: true,
            named_permissions: false,
            session_defaults: super::controls::SessionOptions {
                model: config.model.clone(),
                reasoning_effort: config.reasoning_effort.clone(),
            },
            config,
            #[cfg(test)]
            test_catalog: None,
        };
        if let Err(error) = server.initialize().await {
            let detail = collect_stderr_tail(&stderr_tail);
            return Err(if detail.is_empty() {
                error
            } else {
                error.context(format!("Codex app-server 输出：{detail}"))
            });
        }
        Ok(server)
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    pub fn set_session_config(&mut self, config: &AppServerConfig) {
        self.config = config.clone();
    }

    pub async fn api_request(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        self.request(method, params, REQUEST_TIMEOUT).await
    }

    async fn session_catalog(&self) -> Value {
        #[cfg(test)]
        if let Some(catalog) = &self.test_catalog {
            return catalog.clone();
        }
        crate::model_catalog::read_codex_model_catalog().await
    }

    pub fn set_session_options(&mut self, model: &str, effort: &str) {
        self.config.model = model.to_string();
        self.config.reasoning_effort = effort.to_string();
    }

    pub fn set_session_defaults(&mut self, model: &str, effort: &str) {
        self.session_defaults = super::controls::SessionOptions {
            model: model.to_string(),
            reasoning_effort: effort.to_string(),
        };
    }

    pub async fn prepare_thread(&mut self, thread_id: Option<&str>) -> anyhow::Result<String> {
        self.prepare_thread_in_project(thread_id, None).await
    }

    pub async fn prepare_thread_in_project(
        &mut self,
        thread_id: Option<&str>,
        project_id: Option<&str>,
    ) -> anyhow::Result<String> {
        let mut params = self.thread_params();
        let method = if let Some(thread_id) = thread_id.filter(|id| !id.trim().is_empty()) {
            params["threadId"] = Value::String(thread_id.trim().to_string());
            params.as_object_mut().unwrap().remove("dynamicTools");
            "thread/resume"
        } else {
            if let Some(project_id) = project_id {
                params["projectId"] = json!(project_id);
            }
            "thread/start"
        };
        let result = self.request(method, params, REQUEST_TIMEOUT).await?;
        verify_sandbox_policy(&result, &self.config.sandbox)?;
        let result_id = extract_thread_id(&result)
            .with_context(|| format!("Codex app-server {method} 未返回 thread id"))?;
        if let Some(expected_id) = thread_id.filter(|id| !id.trim().is_empty())
            && result_id != expected_id.trim()
        {
            bail!("Codex app-server 权限协商恢复了不同的线程");
        }
        // 已加载线程的 resume 响应可能仍返回旧档位，不能覆盖待发给 turn/start 的显式选择。
        if self.config.model.is_empty()
            && let Some(model) = result.get("model").and_then(Value::as_str)
        {
            self.config.model = model.to_string();
        }
        if self.config.reasoning_effort.is_empty()
            && let Some(effort) = result.get("reasoningEffort").and_then(Value::as_str)
        {
            self.config.reasoning_effort = effort.to_string();
        }
        Ok(result_id)
    }

    pub async fn recent_thread_context(&mut self, thread_id: &str) -> anyhow::Result<String> {
        let result = self
            .request(
                "thread/read",
                json!({"threadId": thread_id, "includeTurns": false}),
                REQUEST_TIMEOUT,
            )
            .await?;
        let thread = result.get("thread").context("旧会话元数据为空")?;
        if thread.get("id").and_then(Value::as_str) != Some(thread_id) {
            bail!("旧会话元数据 ID 不符");
        }
        let path = thread
            .get("path")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .context("旧会话缺少本地历史文件位置")?;
        let home = crate::codex_home::default_codex_home_dir();
        let thread_id = thread_id.to_owned();
        tokio::task::spawn_blocking(move || read_recent_rollout_context(&home, &path, &thread_id))
            .await
            .context("读取旧微信会话历史任务失败")?
    }

    pub async fn run_turn(
        &mut self,
        thread_id: &str,
        prompt: &str,
    ) -> anyhow::Result<AppServerTurnResult> {
        self.run_turn_with_attachments(thread_id, prompt, &[], None)
            .await
    }

    pub async fn run_turn_with_attachments(
        &mut self,
        thread_id: &str,
        prompt: &str,
        image_paths: &[PathBuf],
        reply_channel: Option<WeixinReplyChannel<'_>>,
    ) -> anyhow::Result<AppServerTurnResult> {
        let started = std::time::Instant::now();
        let id = self.take_request_id();
        let mut params = json!({
            "threadId": thread_id,
            "input": turn_inputs(prompt, image_paths),
            "approvalPolicy": "never"
        });
        if !self.config.model.trim().is_empty() {
            params["model"] = Value::String(self.config.model.trim().to_string());
        }
        if !self.config.reasoning_effort.is_empty() {
            params["effort"] = json!(self.config.reasoning_effort);
        }
        if self.named_permissions {
            params["permissions"] = json!(permission_profile(&self.config.sandbox));
        }
        self.write_json(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "turn/start",
            "params": params
        }))
        .await?;

        let mut response_received = false;
        let mut turn_id = String::new();
        let mut pending_messages = VecDeque::new();
        let mut turn_completed = false;
        let mut reply_parts = Vec::new();
        let mut model = self.config.model.trim().to_string();
        let mut usage = TurnUsage::default();
        let mut attachment_results = HashMap::new();
        let mut session_results = HashMap::new();
        let mut sent_attachments = 0;
        let deadline = tokio::time::Instant::now() + TURN_TIMEOUT;
        while !response_received || !turn_completed {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                bail!("等待 Codex 回复超时");
            }
            let message = if response_received {
                match pending_messages.pop_front() {
                    Some(message) => message,
                    None => self.read_message(remaining).await?,
                }
            } else {
                self.read_message(remaining).await?
            };
            if response_id(&message) == Some(id) {
                if let Some(error) = rpc_error(&message) {
                    bail!("Codex turn/start 失败：{error}");
                }
                let result = message.get("result").cloned().unwrap_or(Value::Null);
                turn_id = extract_turn_id(&result).context("Codex turn/start 未返回 turn id")?;
                if model.is_empty() {
                    model = extract_model(&result).unwrap_or_default();
                }
                if let Some(reported_usage) = extract_turn_usage(&result) {
                    usage = reported_usage;
                }
                response_received = true;
                continue;
            }
            if !response_received {
                // 拿到回合身份后再回放，不能提前执行工具或接受完成通知。
                if pending_messages.len() >= MAX_PENDING_TURN_MESSAGES {
                    bail!("Codex turn/start 响应前的消息数量超过限制");
                }
                pending_messages.push_back(message);
                continue;
            }
            if is_server_request(&message) {
                if message.get("method").and_then(Value::as_str) == Some("item/tool/call")
                    && message_belongs_to_turn(&message, thread_id, &turn_id)
                    && let Some(channel) = reply_channel.as_ref()
                {
                    if message["params"]["tool"].as_str() == Some("weixin_session_settings") {
                        self.handle_session_tool(
                            &message,
                            thread_id,
                            channel,
                            &mut session_results,
                        )
                        .await?;
                    } else {
                        self.handle_attachment_tool(
                            &message,
                            thread_id,
                            channel,
                            &mut attachment_results,
                            &mut sent_attachments,
                        )
                        .await?;
                    }
                } else {
                    self.reject_server_request(&message).await?;
                }
                continue;
            }
            if !message_belongs_to_turn(&message, thread_id, &turn_id) {
                continue;
            }

            if model.is_empty() {
                model = extract_model(&message).unwrap_or_default();
            }
            if let Some(reported_usage) = extract_turn_usage(&message) {
                usage = reported_usage;
            }

            match message.get("method").and_then(Value::as_str) {
                Some("item/completed") => {
                    if let Some(text) = extract_completed_agent_text(&message) {
                        if !reply_parts.iter().any(|part| part == &text) {
                            reply_parts.push(text);
                        }
                    }
                }
                Some("turn/completed") => {
                    if let Some(turn) = message.get("params").and_then(|params| params.get("turn"))
                        && matches!(
                            turn.get("status").and_then(Value::as_str),
                            Some("failed" | "interrupted")
                        )
                    {
                        let reason = deep_string(turn.get("error"), &["message"])
                            .unwrap_or_else(|| "回合未完成".to_string());
                        bail!("Codex 处理失败：{reason}");
                    }
                    turn_completed = true;
                }
                Some("error") => {
                    let error = deep_string(message.get("params"), &["message", "error"])
                        .unwrap_or_else(|| "Codex app-server 返回未知错误".to_string());
                    bail!("{error}");
                }
                _ => {}
            }
        }
        Ok(AppServerTurnResult {
            reply: reply_parts.join("\n\n"),
            model,
            usage,
            elapsed_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
        })
    }

    pub async fn close(&mut self) {
        self.running = false;
        let _ = self.stdin.shutdown().await;
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }

    async fn initialize(&mut self) -> anyhow::Result<()> {
        self.request(
            "initialize",
            json!({
                "clientInfo": {
                    "name": "codex-plus-weixin",
                    "title": "Codex++ Weixin Connect",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "capabilities": {
                    "experimentalApi": true,
                    "optOutNotificationMethods": [
                        "command/exec/outputDelta",
                        "item/agentMessage/delta",
                        "item/plan/delta",
                        "item/fileChange/outputDelta",
                        "item/reasoning/summaryTextDelta",
                        "item/reasoning/textDelta"
                    ]
                }
            }),
            REQUEST_TIMEOUT,
        )
        .await
        .context("初始化 Codex app-server 失败")?;
        self.write_json(&json!({
            "jsonrpc": "2.0",
            "method": "initialized"
        }))
        .await?;
        // legacy 空线程首条消息前没有 rollout；创建前选定权限，不能靠立即 resume 重协商。
        if let Some(result) = self
            .request_optional(
                "permissionProfile/list",
                json!({"cwd": self.config.work_dir}),
                REQUEST_TIMEOUT,
            )
            .await?
        {
            let profiles = result
                .get("data")
                .and_then(Value::as_array)
                .context("Codex app-server 未返回可确认的权限配置列表")?;
            let expected = permission_profile(&self.config.sandbox);
            if !profiles.iter().any(|profile| {
                profile["id"].as_str() == Some(expected)
                    && profile["allowed"].as_bool() == Some(true)
            }) {
                bail!("Codex app-server 不允许微信连接请求的权限配置：{expected}");
            }
            self.named_permissions = true;
        }
        Ok(())
    }

    fn thread_params(&self) -> Value {
        let mut params = json!({
            "cwd": self.config.work_dir.to_string_lossy(),
            "experimentalRawEvents": false,
            "historyMode": "legacy",
            "ephemeral": false,
            "approvalPolicy": "never",
            "sandbox": normalize_sandbox(&self.config.sandbox),
            "dynamicTools": [super::controls::session_tool_spec()]
        });
        if self.config.allow_outbound_attachments {
            params["dynamicTools"]
                .as_array_mut()
                .expect("dynamicTools is initialized as an array")
                .insert(0, attachment_tool_spec());
        }
        if !self.config.model.trim().is_empty() {
            params["model"] = Value::String(self.config.model.trim().to_string());
        }
        if !self.config.reasoning_effort.is_empty() {
            params["config"] = json!({"model_reasoning_effort":self.config.reasoning_effort});
        }
        if self.named_permissions {
            apply_named_permissions(&mut params, &self.config);
        }
        params
    }

    async fn handle_session_tool(
        &mut self,
        message: &Value,
        thread_id: &str,
        channel: &WeixinReplyChannel<'_>,
        results: &mut HashMap<String, Value>,
    ) -> anyhow::Result<()> {
        let call_id = message["params"]["callId"]
            .as_str()
            .filter(|id| !id.is_empty());
        let valid_tool = message["params"]["tool"].as_str() == Some("weixin_session_settings")
            && message["params"]["namespace"].is_null();
        let same_thread = message["params"]["threadId"].as_str() == Some(thread_id);
        if valid_tool
            && same_thread
            && let Some(cached) = call_id.and_then(|id| results.get(id))
        {
            return self
                .write_json(&json!({"jsonrpc":"2.0","id":message["id"],"result":cached}))
                .await;
        }
        let result = async {
            if call_id.is_none() || !same_thread || !valid_tool {
                bail!("设置工具不属于当前微信会话");
            }
            let store = channel.options_store.context("当前会话不支持设置持久化")?;
            let args = message["params"]["arguments"].as_object().context("设置参数必须是对象")?;
            if args.keys().any(|key| !["action","model","reasoningEffort"].contains(&key.as_str())) {
                bail!("不支持的设置参数");
            }
            let catalog = self.session_catalog().await;
            let previous = store.load()?;
            let action = args.get("action").and_then(Value::as_str).context("缺少 action")?;
            let mut next = previous.clone();
            if action == "set" {
                let defaults = channel.defaults.unwrap_or(&self.session_defaults);
                next = super::controls::updated_options(&catalog, defaults, &previous, args)?;
                store.save(next.clone())?;
            } else if action != "get" {
                bail!("未知设置操作");
            }
            Ok::<_, anyhow::Error>(json!({
                "currentTurn": {"model":self.config.model,"reasoningEffort":self.config.reasoning_effort},
                "savedOverrides": next, "appliesFrom":"next-message",
                "connectionDefaults":channel.defaults,
                "models":catalog["models"],"modelMetadata":catalog["modelMetadata"]
            }))
        }.await;
        let result = match result {
            Ok(value) => {
                json!({"success":true,"contentItems":[{"type":"inputText","text":value.to_string()}]})
            }
            Err(error) => {
                json!({"success":false,"contentItems":[{"type":"inputText","text":error.to_string()}]})
            }
        };
        if valid_tool
            && same_thread
            && let Some(call_id) = call_id
        {
            results.insert(call_id.to_string(), result.clone());
        }
        self.write_json(&json!({"jsonrpc":"2.0","id":message["id"],"result":result}))
            .await
    }

    async fn handle_attachment_tool(
        &mut self,
        message: &Value,
        thread_id: &str,
        channel: &WeixinReplyChannel<'_>,
        results: &mut HashMap<String, Value>,
        sent: &mut usize,
    ) -> anyhow::Result<()> {
        let call_id = message
            .get("params")
            .and_then(|params| params.get("callId"))
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty());
        let valid_tool = message["params"]["tool"].as_str() == Some(ATTACHMENT_TOOL)
            && message["params"]["namespace"].is_null();
        let same_thread = message["params"]["threadId"].as_str() == Some(thread_id);
        if valid_tool
            && same_thread
            && let Some(cached) = call_id.and_then(|id| results.get(id))
        {
            return self
                .write_json(&json!({
                    "jsonrpc": "2.0", "id": message["id"], "result": cached
                }))
                .await;
        }
        let result = async {
            if call_id.is_none() {
                bail!("附件工具请求缺少调用标识");
            }
            if !same_thread {
                bail!("附件工具请求不属于当前微信会话");
            }
            if !valid_tool {
                bail!("不支持此附件工具");
            }
            if !self.config.allow_outbound_attachments {
                bail!("微信附件外发未启用");
            }
            let (path, kind) = attachment_tool_arguments(message, &self.config.work_dir)?;
            if *sent >= MAX_ATTACHMENTS_PER_TURN {
                bail!("每回合最多发送 {MAX_ATTACHMENTS_PER_TURN} 个附件");
            }
            channel
                .client
                .send_workspace_attachment(
                    channel.peer,
                    &path,
                    kind,
                    channel.context_token,
                    &self.config.work_dir,
                )
                .await?;
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            let text = format!("附件已成功发送到当前微信会话：{name}");
            *sent += 1;
            Ok::<_, anyhow::Error>(text)
        }
        .await;
        let (success, text) = match result {
            Ok(text) => (true, text),
            Err(error) => (false, format!("附件未发送：{error}")),
        };
        let result = json!({
            "success": success,
            "contentItems": [{"type": "inputText", "text": text}]
        });
        if valid_tool
            && same_thread
            && let Some(call_id) = call_id
        {
            results.insert(call_id.to_string(), result.clone());
        }
        self.write_json(&json!({
            "jsonrpc": "2.0",
            "id": message["id"],
            "result": result
        }))
        .await
    }

    async fn request(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> anyhow::Result<Value> {
        self.request_optional(method, params, timeout)
            .await?
            .with_context(|| format!("Codex app-server 不支持 {method}"))
    }

    async fn request_optional(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> anyhow::Result<Option<Value>> {
        let id = self.take_request_id();
        self.write_json(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }))
        .await?;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                bail!("Codex app-server {method} 请求超时");
            }
            let message = self.read_message(remaining).await?;
            if is_server_request(&message) {
                self.reject_server_request(&message).await?;
                continue;
            }
            if response_id(&message) != Some(id) {
                continue;
            }
            if let Some(error) = rpc_error(&message) {
                // 只对明确的方法缺失降级；权限拒绝、无效参数和传输失败仍然中止。
                let code = message["error"]["code"].as_i64();
                if code == Some(-32601)
                    || (code == Some(-32600)
                        && error.contains("unknown variant")
                        && error.contains(method))
                {
                    return Ok(None);
                }
                bail!("Codex app-server {method} 失败：{error}");
            }
            return Ok(Some(message.get("result").cloned().unwrap_or(Value::Null)));
        }
    }

    async fn read_message(&mut self, timeout: Duration) -> anyhow::Result<Value> {
        loop {
            let line = tokio::time::timeout(timeout, self.stdout.next_line())
                .await
                .context("等待 Codex app-server 响应超时")??;
            let Some(line) = line else {
                self.running = false;
                let exit = self.child.try_wait().ok().flatten();
                bail!(
                    "Codex app-server 已关闭{}",
                    exit.map(|status| format!("：{status}")).unwrap_or_default()
                );
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str(line) {
                Ok(message) => return Ok(message),
                Err(_) => continue,
            }
        }
    }

    async fn reject_server_request(&mut self, message: &Value) -> anyhow::Result<()> {
        let Some(id) = message.get("id") else {
            return Ok(());
        };
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let result = match method {
            "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
                json!({ "decision": "decline" })
            }
            "item/permissions/requestApproval" => json!({ "permissions": {} }),
            "item/tool/requestUserInput" => json!({ "answers": {} }),
            "item/tool/call" => json!({
                "success": false,
                "contentItems": [{
                    "type": "inputText",
                    "text": "tool not available on this client"
                }]
            }),
            _ => {
                return self
                    .write_json(&json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32601, "message": "method not found" }
                    }))
                    .await;
            }
        };
        self.write_json(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result
        }))
        .await
    }

    async fn write_json(&mut self, value: &Value) -> anyhow::Result<()> {
        let mut bytes = serde_json::to_vec(value)?;
        bytes.push(b'\n');
        self.stdin
            .write_all(&bytes)
            .await
            .context("写入 Codex app-server 失败")?;
        self.stdin.flush().await?;
        Ok(())
    }

    fn take_request_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        id
    }
}

fn turn_inputs(prompt: &str, images: &[PathBuf]) -> Vec<Value> {
    let mut inputs = vec![json!({
        "type": "text", "text": prompt, "text_elements": []
    })];
    inputs.extend(images.iter().map(|path| {
        json!({
            "type": "localImage", "path": path.to_string_lossy()
        })
    }));
    inputs
}

fn message_belongs_to_turn(message: &Value, thread_id: &str, turn_id: &str) -> bool {
    let params = &message["params"];
    params["threadId"].as_str() == Some(thread_id)
        && params["turnId"]
            .as_str()
            .or_else(|| params["turn"]["id"].as_str())
            == Some(turn_id)
}

fn attachment_tool_spec() -> Value {
    json!({
        "type": "function",
        "name": ATTACHMENT_TOOL,
        "description": "将本地工作目录内的图片或文件发送给当前微信用户。用户要求返回、发送、提供图片或文件时，应调用本工具实际发送附件，不能只回复本地路径。path 可以是工作目录内的绝对或相对路径；kind=image 直接显示图片，kind=file 作为文件发送。发送成功后无需再次发送同一文件。",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "工作目录内的现有文件路径"},
                "kind": {"type": "string", "enum": ["image", "file"]}
            },
            "required": ["path", "kind"],
            "additionalProperties": false
        }
    })
}

fn attachment_tool_arguments(
    message: &Value,
    work_dir: &Path,
) -> anyhow::Result<(PathBuf, AttachmentKind)> {
    let params = message.get("params").context("工具请求缺少参数")?;
    if params.get("tool").and_then(Value::as_str) != Some(ATTACHMENT_TOOL) {
        bail!("未知微信工具");
    }
    if params
        .get("namespace")
        .is_some_and(|namespace| !namespace.is_null())
    {
        bail!("未知微信工具命名空间");
    }
    let arguments = params.get("arguments").context("工具请求缺少 arguments")?;
    let path = arguments
        .get("path")
        .and_then(Value::as_str)
        .filter(|path| !path.trim().is_empty())
        .context("缺少附件路径")?;
    let kind = match arguments.get("kind").and_then(Value::as_str) {
        Some("image") => AttachmentKind::Image,
        Some("file") => AttachmentKind::File,
        _ => bail!("附件类型必须为 image 或 file"),
    };
    let root = work_dir.canonicalize().context("无法读取工作目录")?;
    validate_outbound_workspace(&root)?;
    let candidate = Path::new(path);
    let candidate = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    };
    let candidate = candidate.canonicalize().context("附件不存在或无法读取")?;
    if !candidate.starts_with(&root) || !candidate.is_file() {
        bail!("只能发送当前工作目录内的文件");
    }
    if has_sensitive_path_component(&candidate) {
        bail!("不能外发敏感配置或版本控制目录中的文件");
    }
    Ok((candidate, kind))
}

fn validate_outbound_workspace(root: &Path) -> anyhow::Result<()> {
    if root.parent().is_none() {
        bail!("不能从文件系统根目录外发附件");
    }
    for variable in ["USERPROFILE", "HOME"] {
        if let Some(home) = std::env::var_os(variable)
            && let Ok(home) = PathBuf::from(home).canonicalize()
            && home == root
        {
            bail!("不能从用户目录外发附件");
        }
    }
    if has_sensitive_path_component(root) {
        bail!("不能从敏感目录外发附件");
    }
    Ok(())
}

fn has_sensitive_path_component(path: &Path) -> bool {
    path.components().any(|component| {
        let std::path::Component::Normal(name) = component else {
            return false;
        };
        let name = name.to_string_lossy().to_ascii_lowercase();
        name == ".git"
            || name == ".ssh"
            || name == "auth.json"
            || name == "config.toml"
            || name == ".env"
            || name.starts_with(".env.")
    })
}

fn recent_conversation_text(text: &str) -> String {
    let start = text
        .char_indices()
        .rev()
        .nth(24_000)
        .map(|(index, _)| index)
        .unwrap_or(0);
    text[start..].to_string()
}

fn read_recent_rollout_context(
    home: &Path,
    path: &Path,
    thread_id: &str,
) -> anyhow::Result<String> {
    use std::io::{BufRead, Read};
    const MAX_ROLLOUT_BYTES: u64 = 16 * 1024 * 1024;
    const MAX_LINE_BYTES: u64 = 1024 * 1024;
    let home = home.canonicalize().context("无法解析会话根目录")?;
    // 压缩历史只回退同一逻辑文件，不扫描其他会话。
    let path = if !path.exists() && path.extension().is_some_and(|ext| ext == "jsonl") {
        let mut compressed = path.as_os_str().to_os_string();
        compressed.push(".zst");
        PathBuf::from(compressed)
    } else {
        path.to_owned()
    };
    let compressed = path.to_string_lossy().ends_with(".jsonl.zst");
    if !compressed && path.extension().is_none_or(|ext| ext != "jsonl") {
        bail!("旧会话历史文件类型无效");
    }
    let path = path.canonicalize().context("旧会话历史文件不存在")?;
    let root = ["sessions", "archived_sessions"]
        .iter()
        .filter_map(|name| home.join(name).canonicalize().ok())
        .find(|root| root.starts_with(&home) && path.starts_with(root))
        .context("旧会话历史文件不在当前 CODEX_HOME 的历史目录内")?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(&path).context("无法打开旧会话历史文件")?;
    if !file.metadata()?.is_file() {
        bail!("旧会话历史必须是普通文件");
    }
    super::media::verify_opened_workspace_file(&file, &path, &root)?;
    let source: Box<dyn Read + '_> = if compressed {
        Box::new(zstd::stream::read::Decoder::new(&file).context("无法解压旧会话历史")?)
    } else {
        Box::new(&file)
    };
    let mut reader = std::io::BufReader::new(source.take(MAX_ROLLOUT_BYTES + 1));
    let mut total = 0u64;
    let mut matched_session = false;
    let mut inherited_from = Vec::new();
    let mut own_history_start = None;
    let mut context = String::new();
    loop {
        let mut line = Vec::new();
        let size = (&mut reader)
            .take(MAX_LINE_BYTES + 1)
            .read_until(b'\n', &mut line)?;
        if size == 0 {
            break;
        }
        total += size as u64;
        if total > MAX_ROLLOUT_BYTES || size as u64 > MAX_LINE_BYTES {
            bail!("旧会话历史超过安全读取限制");
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let record: Value = serde_json::from_slice(&line).context("旧会话历史包含无效 JSON")?;
        let payload = &record["payload"];
        if record["type"] == "session_meta" {
            let id = payload["id"].as_str().context("旧会话历史缺少会话 ID")?;
            if !matched_session {
                if id != thread_id {
                    bail!("旧会话历史 ID 不符");
                }
                matched_session = true;
                own_history_start = payload["subagent_history_start_ordinal"].as_u64();
                for key in ["parent_thread_id", "forked_from_id"] {
                    if let Some(parent) = payload[key].as_str() {
                        inherited_from.push(parent.to_string());
                    }
                }
            } else if id != thread_id
                && !(inherited_from.iter().any(|parent| parent == id)
                    && own_history_start.is_some_and(|start| {
                        record["ordinal"]
                            .as_u64()
                            .is_some_and(|ordinal| ordinal < start)
                    }))
            {
                bail!("旧会话历史混入其他会话");
            }
        }
        if record["type"] != "response_item" || payload["type"] != "message" {
            continue;
        }
        let role = match payload["role"].as_str() {
            Some("user") => "用户",
            Some("assistant") => "助手",
            _ => continue,
        };
        let text = payload["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|item| matches!(item["type"].as_str(), Some("input_text" | "output_text")))
            .filter_map(|item| item["text"].as_str())
            .filter(|text| {
                payload["role"] != "user"
                    || ![
                        "<codex_internal_context",
                        "<environment_context",
                        "<permissions instructions>",
                        "# AGENTS.md instructions",
                        "<INSTRUCTIONS>",
                    ]
                    .iter()
                    .any(|prefix| text.trim_start().starts_with(prefix))
            })
            .collect::<Vec<_>>()
            .join("\n");
        if !text.is_empty() {
            context.push_str(&format!("\n\n{role}：{text}"));
            context = recent_conversation_text(&context);
        }
    }
    if !matched_session {
        bail!("旧会话历史缺少会话身份");
    }
    super::media::verify_opened_workspace_file(&file, &path, &root)?;
    Ok(context.trim().to_string())
}

impl Drop for CodexAppServer {
    fn drop(&mut self) {
        if self.running {
            let _ = self.child.start_kill();
        }
    }
}

fn normalize_sandbox(value: &str) -> &'static str {
    match value.trim() {
        "workspace-write" => "workspace-write",
        "danger-full-access" => "danger-full-access",
        _ => "read-only",
    }
}

fn permission_profile(sandbox: &str) -> &'static str {
    match normalize_sandbox(sandbox) {
        "workspace-write" => ":workspace",
        "danger-full-access" => ":danger-full-access",
        _ => ":read-only",
    }
}

fn apply_named_permissions(params: &mut Value, config: &AppServerConfig) {
    params.as_object_mut().unwrap().remove("sandbox");
    params["permissions"] = json!(permission_profile(&config.sandbox));
    params["runtimeWorkspaceRoots"] = json!([config.work_dir]);
}

fn verify_sandbox_policy(result: &Value, expected: &str) -> anyhow::Result<()> {
    let actual = result
        .get("sandbox")
        .and_then(|value| value.get("type"))
        .and_then(Value::as_str)
        .context("Codex app-server 未返回可确认的沙箱权限")?;
    let actual = match actual {
        "readOnly" | "read-only" => "read-only",
        "workspaceWrite" | "workspace-write" => "workspace-write",
        "dangerFullAccess" | "danger-full-access" => "danger-full-access",
        _ => bail!("Codex app-server 返回了无法识别的沙箱权限"),
    };
    if actual != normalize_sandbox(expected) {
        bail!("Codex app-server 实际沙箱权限与微信连接配置不符");
    }
    Ok(())
}

fn response_id(message: &Value) -> Option<u64> {
    message.get("id").and_then(Value::as_u64)
}

fn rpc_error(message: &Value) -> Option<String> {
    let error = message.get("error")?;
    deep_string(Some(error), &["message", "error"]).or_else(|| Some(error.to_string()))
}

fn is_server_request(message: &Value) -> bool {
    message.get("id").is_some() && message.get("method").and_then(Value::as_str).is_some()
}

fn extract_thread_id(result: &Value) -> Option<String> {
    deep_string(Some(result), &["threadId", "id"]).or_else(|| {
        result
            .get("thread")
            .and_then(|thread| deep_string(Some(thread), &["id", "threadId"]))
    })
}

fn extract_turn_id(result: &Value) -> Option<String> {
    deep_string(Some(result), &["turnId", "id"]).or_else(|| {
        result
            .get("turn")
            .and_then(|turn| deep_string(Some(turn), &["id", "turnId"]))
    })
}

fn extract_model(value: &Value) -> Option<String> {
    deep_string(
        Some(value),
        &["model", "modelSlug", "model_slug", "modelId", "model_id"],
    )
    .map(|model| model.trim().to_string())
    .filter(|model| !model.is_empty())
}

fn extract_turn_usage(value: &Value) -> Option<TurnUsage> {
    if let Some(usage) = value.get("tokenUsage").or_else(|| value.get("token_usage")) {
        if let Some(parsed) = parse_turn_usage(usage) {
            return Some(parsed);
        }
    }
    if value.get("method").and_then(Value::as_str) == Some("thread/tokenUsage/updated") {
        return value.get("params").and_then(extract_turn_usage);
    }
    value
        .as_object()
        .and_then(|object| object.values().find_map(extract_turn_usage))
}

fn parse_turn_usage(value: &Value) -> Option<TurnUsage> {
    let object = value.as_object()?;
    let window = object
        .get("modelContextWindow")
        .or_else(|| object.get("model_context_window"))
        .and_then(value_as_u64);
    let last = object
        .get("last")
        .or_else(|| object.get("last_token_usage"));
    let context_used = last
        .and_then(|last| {
            last.get("totalTokens")
                .or_else(|| last.get("total_tokens"))
                .and_then(value_as_u64)
        })
        .or_else(|| {
            object
                .get("contextUsed")
                .or_else(|| object.get("context_used"))
                .and_then(value_as_u64)
        });
    if context_used.is_none() && window.is_none() {
        return None;
    }
    Some(TurnUsage {
        context_used,
        context_window: window,
    })
}

fn value_as_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|value| u64::try_from(value).ok()))
}

fn extract_completed_agent_text(message: &Value) -> Option<String> {
    let item = message.get("params")?.get("item")?;
    let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
    if !matches!(
        item_type,
        "agentMessage" | "assistantMessage" | "output_text"
    ) {
        return None;
    }
    deep_string(Some(item), &["text", "content", "output_text"])
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

fn deep_string(value: Option<&Value>, keys: &[&str]) -> Option<String> {
    let value = value?;
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }
    if let Some(items) = value.as_array() {
        let parts = items
            .iter()
            .filter_map(|item| deep_string(Some(item), keys))
            .collect::<Vec<_>>();
        if !parts.is_empty() {
            return Some(parts.join("\n"));
        }
    }
    let object = value.as_object()?;
    for key in keys {
        if let Some(text) = object
            .get(*key)
            .and_then(|value| deep_string(Some(value), keys))
        {
            return Some(text);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_server(variable: &str, mode: &str, root: &Path) -> CodexAppServer {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "connect::app_server::tests::protection_stdio_fixture",
                "--nocapture",
            ])
            .env(variable, mode)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(crate::windows_integration::CREATE_NO_WINDOW);
        let mut child = command.spawn().unwrap();
        CodexAppServer {
            stdin: child.stdin.take().unwrap(),
            stdout: BufReader::new(child.stdout.take().unwrap()).lines(),
            child,
            next_id: 1,
            running: true,
            named_permissions: false,
            config: AppServerConfig {
                executable: String::new(),
                work_dir: root.to_path_buf(),
                model: "fixture-model".into(),
                reasoning_effort: "low".into(),
                allow_outbound_attachments: true,
                sandbox: "workspace-write".into(),
            },
            test_catalog: None,
            session_defaults: super::super::controls::SessionOptions::default(),
        }
    }

    #[test]
    fn protection_stdio_fixture() {
        use std::io::{BufRead, Write};
        let Ok(mode) = std::env::var("CODEX_PLUS_WEIXIN_PROTECTION_FIXTURE") else {
            return;
        };
        fn emit(value: Value) {
            println!("\n{value}");
            std::io::stdout().flush().unwrap();
        }
        let mut starts = 0;
        let mut resumes = 0;
        let mut turn_completed = false;
        for line in std::io::stdin().lock().lines() {
            let request: Value = serde_json::from_str(&line.unwrap()).unwrap();
            let params = &request["params"];
            match request["method"].as_str() {
                Some("initialize") => emit(json!({"id":request["id"],"result":{}})),
                Some("initialized") => {}
                Some("permissionProfile/list") => {
                    let response = match mode.as_str() {
                        "legacy" | "legacy-wrong-policy" => {
                            json!({"error":{"code":-32601,"message":"Method not found"}})
                        }
                        "legacy-unknown-variant" => json!({"error":{"code":-32600,
                            "message":"unknown variant `permissionProfile/list`"}}),
                        "list-error" => {
                            json!({"error":{"code":-32600,"message":"permission denied"}})
                        }
                        "malformed-profiles" => json!({"result":{}}),
                        _ => json!({"result":{"data":[
                            {"id":":workspace","allowed":mode != "denied"},
                            {"id":":read-only","allowed":true}
                        ]}}),
                    };
                    let mut response = response;
                    response["id"] = request["id"].clone();
                    emit(response);
                }
                Some("thread/start") => {
                    starts += 1;
                    assert_eq!(starts, 1);
                    assert_eq!(params["projectId"], "project-fixture");
                    assert_eq!(params["dynamicTools"].as_array().unwrap().len(), 2);
                    if mode.starts_with("legacy") {
                        assert_eq!(params["sandbox"], "workspace-write");
                        assert!(params.get("permissions").is_none());
                    } else {
                        assert!(params.get("sandbox").is_none());
                        assert_eq!(
                            params["permissions"],
                            if mode == "named-read-only" {
                                ":read-only"
                            } else {
                                ":workspace"
                            }
                        );
                        assert_eq!(
                            params["runtimeWorkspaceRoots"][0],
                            std::env::current_dir().unwrap().to_string_lossy().as_ref()
                        );
                    }
                    let result = match mode.as_str() {
                        "missing-policy" => json!({"thread":{"id":"permission-thread"}}),
                        "wrong-policy" | "legacy-wrong-policy" | "named-read-only" => {
                            json!({"thread":{"id":"permission-thread","path":null},
                                "activePermissionProfile":null,"sandbox":{"type":"readOnly"}})
                        }
                        _ => {
                            json!({"thread":{"id":"permission-thread","path":null},
                                "activePermissionProfile":null,"sandbox":{"type":"workspaceWrite"}})
                        }
                    };
                    emit(json!({"id":request["id"],"result":result}));
                }
                Some("thread/resume") => {
                    resumes += 1;
                    assert_eq!(resumes, 1);
                    assert!(params.get("dynamicTools").is_none());
                    assert_eq!(params["threadId"], "permission-thread");
                    if mode.starts_with("legacy") {
                        assert_eq!(params["sandbox"], "workspace-write");
                        assert!(params.get("permissions").is_none());
                    } else {
                        assert!(params.get("sandbox").is_none());
                        assert_eq!(
                            params["permissions"],
                            if mode == "named-read-only" {
                                ":read-only"
                            } else {
                                ":workspace"
                            }
                        );
                    }
                    if mode == "wrong-thread" {
                        assert_eq!(starts, 0);
                        emit(json!({"id":request["id"],"result":{
                            "thread":{"id":"other-thread"},"sandbox":{"type":"workspaceWrite"}
                        }}));
                    } else if !turn_completed {
                        emit(json!({"id":request["id"],"error":{"code":-32603,
                            "message":"no rollout found for thread id permission-thread"}}));
                    } else {
                        emit(json!({"id":request["id"],"result":{
                            "thread":{"id":"permission-thread"},"sandbox":{
                                "type":if mode == "named-read-only" { "readOnly" } else { "workspaceWrite" }
                            }
                        }}));
                    }
                }
                Some("turn/start") if params["threadId"] == "permission-thread" => {
                    assert_eq!(starts, 1);
                    assert_eq!(resumes, 0, "空线程不能先 resume 再发送首条消息");
                    if !mode.starts_with("legacy") {
                        assert_eq!(
                            params["permissions"],
                            if mode == "named-read-only" {
                                ":read-only"
                            } else {
                                ":workspace"
                            }
                        );
                    }
                    turn_completed = true;
                    emit(json!({"id":request["id"],"result":{"turn":{"id":"permission-turn"}}}));
                    emit(json!({"method":"turn/completed","params":{
                        "threadId":"permission-thread",
                        "turn":{"id":"permission-turn","status":"completed"}
                    }}));
                }
                Some("turn/start") => {
                    assert_eq!(params["threadId"], "queue-thread");
                    let count = if mode == "overflow" { 257 } else { 256 };
                    for index in 0..count {
                        emit(json!({"method":"item/completed","params":{
                            "threadId":"queue-thread","turnId":"queue-turn",
                            "item":{"type":"agentMessage","text":format!("early-{index}")}
                        }}));
                    }
                    emit(json!({"id":request["id"],"result":{"turn":{"id":"queue-turn"}}}));
                    emit(json!({"method":"turn/completed","params":{
                        "threadId":"queue-thread","turn":{"id":"queue-turn","status":"completed"}
                    }}));
                }
                _ => panic!("Unexpected protection fixture request: {request}"),
            }
        }
    }

    #[tokio::test]
    async fn permissions_are_selected_before_thread_creation_and_reject_unconfirmed_scope() {
        let root = tempfile::tempdir().unwrap();
        for mode in [
            "legacy",
            "legacy-unknown-variant",
            "legacy-wrong-policy",
            "named",
            "named-read-only",
            "missing-policy",
            "wrong-thread",
            "wrong-policy",
            "denied",
            "list-error",
            "malformed-profiles",
        ] {
            let mut server =
                fixture_server("CODEX_PLUS_WEIXIN_PROTECTION_FIXTURE", mode, root.path());
            if mode == "named-read-only" {
                server.config.sandbox = "read-only".into();
            }
            let result = async {
                server.initialize().await?;
                if mode == "wrong-thread" {
                    return server.prepare_thread(Some("permission-thread")).await;
                }
                let thread = server
                    .prepare_thread_in_project(None, Some("project-fixture"))
                    .await?;
                server.run_turn(&thread, "first message").await?;
                assert_eq!(server.prepare_thread(Some(&thread)).await?, thread);
                Ok(thread)
            }
            .await;
            let named = server.named_permissions;
            server.close().await;
            match mode {
                "legacy" | "legacy-unknown-variant" | "named" | "named-read-only" => {
                    assert_eq!(result.unwrap(), "permission-thread");
                    assert_eq!(named, !mode.starts_with("legacy"));
                }
                _ => {
                    let error = result.unwrap_err().to_string();
                    let expected = match mode {
                        "missing-policy" => "未返回可确认的沙箱权限",
                        "wrong-thread" => "恢复了不同的线程",
                        "wrong-policy" | "legacy-wrong-policy" => "实际沙箱权限",
                        "denied" => "不允许微信连接请求的权限配置",
                        "list-error" => "permission denied",
                        "malformed-profiles" => "未返回可确认的权限配置列表",
                        _ => unreachable!(),
                    };
                    assert!(error.contains(expected), "{mode}: {error}");
                }
            }
        }
    }

    #[tokio::test]
    async fn early_turn_messages_are_replayed_in_fifo_order() {
        let root = tempfile::tempdir().unwrap();
        let mut server =
            fixture_server("CODEX_PLUS_WEIXIN_PROTECTION_FIXTURE", "queue", root.path());
        let result = server.run_turn("queue-thread", "test").await;
        server.close().await;
        let turn = result.unwrap();
        assert_eq!(
            turn.reply,
            (0..256)
                .map(|index| format!("early-{index}"))
                .collect::<Vec<_>>()
                .join("\n\n")
        );
    }

    #[tokio::test]
    async fn early_turn_message_buffer_rejects_the_257th_message() {
        let root = tempfile::tempdir().unwrap();
        let mut server = fixture_server(
            "CODEX_PLUS_WEIXIN_PROTECTION_FIXTURE",
            "overflow",
            root.path(),
        );
        let result = server.run_turn("queue-thread", "test").await;
        server.close().await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("响应前的消息数量超过限制")
        );
    }

    #[test]
    fn media_notifications_require_both_thread_and_turn_identity() {
        for message in [
            json!({"params":{"threadId":"thread","turnId":"turn"}}),
            json!({"params":{"threadId":"thread","turn":{"id":"turn"}}}),
        ] {
            assert!(message_belongs_to_turn(&message, "thread", "turn"));
            assert!(!message_belongs_to_turn(&message, "other", "turn"));
            assert!(!message_belongs_to_turn(&message, "thread", "old"));
        }
        assert!(!message_belongs_to_turn(
            &json!({"params":{}}),
            "thread",
            "turn"
        ));
    }

    #[test]
    fn weixin_stdio_fixture() {
        use std::io::{BufRead, Write};
        if std::env::var("CODEX_PLUS_WEIXIN_RPC_FIXTURE").as_deref() != Ok("1") {
            return;
        }
        fn emit(value: Value) {
            println!("\n{value}");
            std::io::stdout().flush().unwrap();
        }
        fn call(id: &str, call_id: &str, path: &str) {
            emit(json!({
                "jsonrpc": "2.0", "id": id, "method": "item/tool/call",
                "params": {
                    "threadId": "thread-fixture", "turnId": "turn-1",
                    "callId": call_id, "tool": ATTACHMENT_TOOL, "namespace": null,
                    "arguments": {"path": path, "kind": "file"}
                }
            }));
        }
        let mut turn = 0;
        let mut starts = 0;
        for line in std::io::stdin().lock().lines() {
            let message: Value = serde_json::from_str(&line.unwrap()).unwrap();
            match message.get("method").and_then(Value::as_str) {
                Some("initialize") => emit(json!({"id": message["id"], "result": {}})),
                Some("initialized") => {}
                Some("permissionProfile/list") => emit(json!({"id":message["id"],"result":{
                    "data":[{"id":":workspace","allowed":true}]
                }})),
                Some("thread/start") => {
                    starts += 1;
                    assert_eq!(starts, 1, "权限协商不能额外创建线程");
                    assert_eq!(message["params"]["dynamicTools"][0]["type"], "function");
                    assert!(message["params"].get("sandbox").is_none());
                    assert_eq!(message["params"]["permissions"], ":workspace");
                    assert_eq!(
                        message["params"]["runtimeWorkspaceRoots"][0],
                        std::env::current_dir().unwrap().to_string_lossy().as_ref()
                    );
                    emit(json!({"id": message["id"], "result": {
                        "thread": {"id": "thread-fixture"},
                        "activePermissionProfile": {"id":":workspace"},
                        "sandbox": {"type":"workspaceWrite"}
                    }}));
                }
                Some("thread/resume") => {
                    assert!(turn > 0, "空线程没有可用于 resume 的 rollout");
                    assert!(message["params"].get("dynamicTools").is_none());
                    assert!(message["params"].get("sandbox").is_none());
                    assert_eq!(message["params"]["threadId"], "thread-fixture");
                    assert_eq!(message["params"]["permissions"], ":workspace");
                    assert_eq!(
                        message["params"]["runtimeWorkspaceRoots"][0],
                        std::env::current_dir().unwrap().to_string_lossy().as_ref()
                    );
                    emit(json!({"id": message["id"], "result": {
                        "thread": {"id": "thread-fixture"}, "sandbox": {"type": "workspaceWrite"}
                    }}));
                }
                Some("thread/read") => {
                    assert_eq!(message["params"]["includeTurns"], false);
                    emit(json!({"id": message["id"], "result": {"thread": {
                        "id": "thread-fixture",
                        "path": std::env::current_dir().unwrap().join("sessions/legacy.jsonl")
                    }}}));
                }
                Some("turn/start") => {
                    turn += 1;
                    assert_eq!(message["params"]["permissions"], ":workspace");
                    // 上一回合残留的状态通知不能结束当前回合。
                    emit(
                        json!({"method": "thread/status/changed", "params": {"status": {"type": "idle"}}}),
                    );
                    emit(json!({"method":"turn/completed","params":{
                        "threadId":"thread-fixture","turn":{"id":"old-turn","status":"completed"}
                    }}));
                    emit(json!({"method":"item/completed","params":{
                        "threadId":"other-thread","turnId":format!("turn-{turn}"),
                        "item":{"type":"agentMessage","text":"foreign reply"}
                    }}));
                    if turn == 1 {
                        emit(json!({"method":"item/completed","params":{
                            "threadId":"thread-fixture","turnId":"turn-1",
                            "item":{"type":"agentMessage","text":"early reply"}
                        }}));
                        call("outside", "outside", "../private.txt");
                    }
                    emit(
                        json!({"id": message["id"], "result": {"turn": {"id": format!("turn-{turn}")}}}),
                    );
                    if turn == 1 {
                        assert_eq!(message["params"]["model"], "fixture-model");
                        assert_eq!(message["params"]["effort"], "low");
                        assert_eq!(message["params"]["input"][1]["type"], "localImage");
                    } else {
                        assert_eq!(message["params"]["model"], "next-model");
                        assert_eq!(message["params"]["effort"], "high");
                        emit(
                            json!({"method": "turn/completed", "params": {"threadId":"thread-fixture","turn": {
                                "id": "turn-2", "status": "failed", "error": {"message": "fixture model failure"}
                            }}}),
                        );
                    }
                }
                _ => match message.get("id").and_then(Value::as_str) {
                    Some("outside") => {
                        assert_eq!(message["result"]["success"], false);
                        call("first", "first", "result.txt");
                    }
                    Some("first") => {
                        assert_eq!(message["result"]["success"], true, "{message}");
                        std::fs::write("result.txt", "updated-result").unwrap();
                        emit(
                            json!({"id":"foreign-turn","method":"item/tool/call","params":{
                                "threadId":"thread-fixture","turnId":"old-turn","callId":"first",
                                "tool":ATTACHMENT_TOOL,"arguments":{"path":"result.txt","kind":"file"}
                            }}),
                        );
                    }
                    Some("foreign-turn") => {
                        assert_eq!(message["result"]["success"], false);
                        emit(
                            json!({"id":"foreign-tool","method":"item/tool/call","params":{
                                "threadId":"thread-fixture","turnId":"turn-1","callId":"first",
                                "tool":"other-tool","arguments":{"path":"result.txt","kind":"file"}
                            }}),
                        );
                    }
                    Some("foreign-tool") => {
                        assert_eq!(message["result"]["success"], false);
                        emit(
                            json!({"id":"foreign-namespace","method":"item/tool/call","params":{
                                "threadId":"thread-fixture","turnId":"turn-1","callId":"first",
                                "tool":ATTACHMENT_TOOL,"namespace":"other",
                                "arguments":{"path":"result.txt","kind":"file"}
                            }}),
                        );
                    }
                    Some("foreign-namespace") => {
                        assert_eq!(message["result"]["success"], false);
                        call("replay", "first", "result.txt");
                    }
                    Some("replay") => {
                        assert_eq!(message["result"]["success"], true);
                        call("updated", "second", "result.txt");
                    }
                    Some("updated") => {
                        assert_eq!(message["result"]["success"], true, "{message}");
                        emit(
                            json!({"method": "item/completed", "params": {"threadId":"thread-fixture","turnId":"turn-1","item": {
                                "type": "agentMessage", "text": "file delivered"
                            }}}),
                        );
                        emit(
                            json!({"method": "turn/completed", "params": {"threadId":"thread-fixture","turn": {
                                "id": "turn-1", "status": "completed"
                            }}}),
                        );
                    }
                    _ => panic!("Unexpected fixture message"),
                },
            }
        }
    }

    #[tokio::test]
    async fn stdio_turn_waits_for_completion_and_delivers_real_tool_requests_to_media_transport() {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{body_partial_json, method, path},
        };
        let gateway = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/ilink/bot/getuploadurl"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "upload_full_url": format!("{}/upload", gateway.uri())
            })))
            .expect(2)
            .mount(&gateway)
            .await;
        Mock::given(method("POST"))
            .and(path("/upload"))
            .respond_with(ResponseTemplate::new(200).insert_header("x-encrypted-param", "download"))
            .expect(2)
            .mount(&gateway)
            .await;
        Mock::given(method("POST"))
            .and(path("/ilink/bot/sendmessage"))
            .and(body_partial_json(json!({"msg": {
                "to_user_id": "fixture-peer", "context_token": "fixture-context",
                "item_list": [{"type": 4, "file_item": {"file_name": "result.txt"}}]
            }})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ret": 0})))
            .expect(2)
            .mount(&gateway)
            .await;
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("result.txt"), "first-result").unwrap();
        std::fs::create_dir(temp.path().join("sessions")).unwrap();
        std::fs::write(temp.path().join("sessions/legacy.jsonl"),
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"thread-fixture\"}}\n\
             {\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"legacy question\"}]}}\n").unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "connect::app_server::tests::weixin_stdio_fixture",
                "--nocapture",
            ])
            .env("CODEX_PLUS_WEIXIN_RPC_FIXTURE", "1")
            .current_dir(temp.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(crate::windows_integration::CREATE_NO_WINDOW);
        let mut child = command.spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut server = CodexAppServer {
            child,
            stdin,
            stdout: BufReader::new(stdout).lines(),
            next_id: 1,
            running: true,
            named_permissions: false,
            config: AppServerConfig {
                executable: String::new(),
                work_dir: temp.path().to_path_buf(),
                model: "fixture-model".to_string(),
                reasoning_effort: "low".to_string(),
                allow_outbound_attachments: true,
                sandbox: "workspace-write".to_string(),
            },
            test_catalog: None,
            session_defaults: super::super::controls::SessionOptions {
                model: "fixture-model".to_string(),
                reasoning_effort: "low".to_string(),
            },
        };
        let client = WeixinClient::new(&gateway.uri(), "fixture-token", "")
            .unwrap()
            .with_media_base_url(&gateway.uri())
            .unwrap();
        let result = async {
            server.initialize().await?;
            let metadata = server
                .request(
                    "thread/read",
                    json!({"threadId": "thread-fixture", "includeTurns": false}),
                    REQUEST_TIMEOUT,
                )
                .await?;
            assert_eq!(metadata["thread"]["id"], "thread-fixture");
            assert!(
                read_recent_rollout_context(
                    temp.path(),
                    Path::new(metadata["thread"]["path"].as_str().unwrap()),
                    "thread-fixture"
                )?
                .contains("legacy question")
            );
            let thread = server.prepare_thread(None).await?;
            let turn = server
                .run_turn_with_attachments(
                    &thread,
                    "process image and return result",
                    &[temp.path().join("photo.png")],
                    Some(WeixinReplyChannel {
                        client: &client,
                        peer: "fixture-peer",
                        context_token: "fixture-context",
                        options_store: None,
                        defaults: None,
                    }),
                )
                .await?;
            assert_eq!(turn.reply, "early reply\n\nfile delivered");
            server.set_session_options("next-model", "high");
            assert_eq!(server.session_defaults.model, "fixture-model");
            assert_eq!(server.session_defaults.reasoning_effort, "low");
            server.prepare_thread(Some(&thread)).await?;
            let error = server.run_turn(&thread, "fail").await.unwrap_err();
            assert!(error.to_string().contains("fixture model failure"));
            Ok::<_, anyhow::Error>(())
        }
        .await;
        server.close().await;
        result.unwrap();
        gateway.verify().await;
        let requests = gateway.received_requests().await.unwrap();
        let uploads = requests
            .iter()
            .filter(|request| request.url.path() == "/ilink/bot/getuploadurl")
            .map(|request| serde_json::from_slice::<Value>(&request.body).unwrap())
            .collect::<Vec<_>>();
        assert_ne!(uploads[0]["rawfilemd5"], uploads[1]["rawfilemd5"]);
    }

    #[test]
    fn weixin_settings_stdio_fixture() {
        use std::io::{BufRead, Write};
        if std::env::var("CODEX_PLUS_WEIXIN_SETTINGS_FIXTURE").as_deref() != Ok("1") {
            return;
        }
        fn emit(value: Value) {
            println!("\n{value}");
            std::io::stdout().flush().unwrap();
        }
        fn call(id: &str, thread: &str, args: Value) {
            emit(json!({"id":id,"method":"item/tool/call","params":{
                "threadId":thread,"turnId":"t1","callId":id,"tool":"weixin_session_settings","arguments":args
            }}));
        }
        fn complete(turn: u32) {
            emit(
                json!({"method":"item/completed","params":{"threadId":"settings-thread","turnId":format!("t{turn}"),"item":{"type":"agentMessage","text":"settings checked"}}}),
            );
            emit(
                json!({"method":"turn/completed","params":{"threadId":"settings-thread","turn":{"id":format!("t{turn}"),"status":"completed"}}}),
            );
        }
        let mut turn = 0;
        for line in std::io::stdin().lock().lines() {
            let message: Value = serde_json::from_str(&line.unwrap()).unwrap();
            match message["method"].as_str() {
                Some("initialize") => emit(json!({"id":message["id"],"result":{}})),
                Some("initialized") => {}
                Some("permissionProfile/list") => emit(json!({"id":message["id"],
                    "error":{"code":-32601,"message":"Method not found"}
                })),
                Some("thread/start" | "thread/resume") => {
                    let expected = if turn == 0 { "high" } else { "low" };
                    assert_eq!(
                        message["params"]["config"]["model_reasoning_effort"],
                        expected
                    );
                    emit(json!({"id":message["id"],"result":{
                        "thread":{"id":"settings-thread"},"model":"fixture-model",
                        "reasoningEffort":"high","sandbox":{"type":"readOnly"}
                    }}));
                }
                Some("turn/start") => {
                    turn += 1;
                    assert_eq!(message["params"]["model"], "fixture-model");
                    assert_eq!(
                        message["params"]["effort"],
                        if turn == 1 { "high" } else { "low" }
                    );
                    emit(json!({"id":message["id"],"result":{"turn":{"id":format!("t{turn}")}}}));
                    if turn == 1 {
                        call("get", "settings-thread", json!({"action":"get"}));
                    } else {
                        complete(turn);
                    }
                }
                _ => match message["id"].as_str() {
                    Some("get") => {
                        assert_eq!(message["result"]["success"], true);
                        let text: Value = serde_json::from_str(
                            message["result"]["contentItems"][0]["text"]
                                .as_str()
                                .unwrap(),
                        )
                        .unwrap();
                        assert_eq!(text["currentTurn"]["reasoningEffort"], "high");
                        emit(
                            json!({"id":"foreign-namespace","method":"item/tool/call","params":{
                                "threadId":"settings-thread","turnId":"t1","callId":"get",
                                "tool":"weixin_session_settings","namespace":"other",
                                "arguments":{"action":"set","reasoningEffort":"low"}
                            }}),
                        );
                    }
                    Some("foreign-namespace") => {
                        assert_eq!(message["result"]["success"], false);
                        emit(
                            json!({"id":"foreign-turn","method":"item/tool/call","params":{
                                "threadId":"settings-thread","turnId":"old","callId":"get",
                                "tool":"weixin_session_settings",
                                "arguments":{"action":"set","reasoningEffort":"low"}
                            }}),
                        );
                    }
                    Some("foreign-turn") => {
                        assert_eq!(message["result"]["success"], false);
                        emit(
                            json!({"id":"attachment-collision","method":"item/tool/call","params":{
                                "threadId":"settings-thread","turnId":"t1","callId":"get",
                                "tool":ATTACHMENT_TOOL,"arguments":{"path":"missing.txt","kind":"file"}
                            }}),
                        );
                    }
                    Some("attachment-collision") => {
                        assert_eq!(message["result"]["success"], false);
                        emit(
                            json!({"id":"get-replay","method":"item/tool/call","params":{
                                "threadId":"settings-thread","turnId":"t1","callId":"get",
                                "tool":"weixin_session_settings","arguments":{"action":"get"}
                            }}),
                        );
                    }
                    Some("get-replay") => {
                        assert_eq!(message["result"]["success"], true);
                        let text: Value = serde_json::from_str(
                            message["result"]["contentItems"][0]["text"]
                                .as_str()
                                .unwrap(),
                        )
                        .unwrap();
                        assert_eq!(
                            text["savedOverrides"],
                            json!({"model":"","reasoningEffort":""})
                        );
                        call(
                            "set",
                            "settings-thread",
                            json!({"action":"set","reasoningEffort":"low"}),
                        );
                    }
                    Some("set") => {
                        assert_eq!(message["result"]["success"], true);
                        let text: Value = serde_json::from_str(
                            message["result"]["contentItems"][0]["text"]
                                .as_str()
                                .unwrap(),
                        )
                        .unwrap();
                        assert_eq!(text["currentTurn"]["reasoningEffort"], "high");
                        assert_eq!(text["savedOverrides"]["reasoningEffort"], "low");
                        assert_eq!(text["appliesFrom"], "next-message");
                        call(
                            "invalid",
                            "settings-thread",
                            json!({"action":"set","model":"unknown"}),
                        );
                    }
                    Some("invalid") => {
                        assert_eq!(message["result"]["success"], false);
                        call(
                            "cross-thread",
                            "other-thread",
                            json!({"action":"set","reasoningEffort":"high"}),
                        );
                    }
                    Some("cross-thread") => {
                        assert_eq!(message["result"]["success"], false);
                        complete(turn);
                    }
                    _ => panic!("Unexpected settings fixture message: {message}"),
                },
            }
        }
    }

    #[tokio::test]
    async fn remote_settings_persist_and_change_only_the_next_turn() {
        use super::super::controls::{SessionOptions, SessionOptionsStore};
        let temp = tempfile::tempdir().unwrap();
        let store = SessionOptionsStore::test_store(temp.path().join("settings.json"), "peer-a");
        let other = SessionOptionsStore::test_store(temp.path().join("settings.json"), "peer-b");
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "connect::app_server::tests::weixin_settings_stdio_fixture",
                "--nocapture",
            ])
            .env("CODEX_PLUS_WEIXIN_SETTINGS_FIXTURE", "1")
            .current_dir(temp.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(crate::windows_integration::CREATE_NO_WINDOW);
        let mut child = command.spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let config = AppServerConfig {
            executable: String::new(),
            work_dir: temp.path().to_path_buf(),
            model: "fixture-model".into(),
            reasoning_effort: "high".into(),
            allow_outbound_attachments: true,
            sandbox: "read-only".into(),
        };
        let mut server = CodexAppServer {
            child,
            stdin,
            stdout: BufReader::new(stdout).lines(),
            next_id: 1,
            running: true,
            named_permissions: false,
            config: config.clone(),
            session_defaults: SessionOptions {
                model: config.model.clone(),
                reasoning_effort: config.reasoning_effort.clone(),
            },
            test_catalog: Some(json!({"models":["fixture-model"],"modelMetadata":{
                "fixture-model":{"supportedReasoningEfforts":[{"reasoningEffort":"high"},{"reasoningEffort":"low"}]}
            }})),
        };
        // 设置工具不发送微信请求；不可达地址能揭示误发附件的回归。
        let client = WeixinClient::new("http://127.0.0.1:1", "fixture", "").unwrap();
        let defaults = SessionOptions {
            model: config.model.clone(),
            reasoning_effort: config.reasoning_effort.clone(),
        };
        let result = async {
            server.initialize().await?;
            let thread = server.prepare_thread(None).await?;
            let turn = server
                .run_turn_with_attachments(
                    &thread,
                    "set low",
                    &[],
                    Some(WeixinReplyChannel {
                        client: &client,
                        peer: "peer-a",
                        context_token: "test",
                        options_store: Some(&store),
                        defaults: Some(&defaults),
                    }),
                )
                .await?;
            assert_eq!(turn.reply, "settings checked");
            assert_eq!(server.config.reasoning_effort, "high");
            assert_eq!(other.load()?, SessionOptions::default());
            let reloaded =
                SessionOptionsStore::test_store(temp.path().join("settings.json"), "peer-a");
            let effective = reloaded.load()?.with_defaults(&defaults);
            assert_eq!(effective.reasoning_effort, "low");
            let mut next_config = config.clone();
            next_config.model = effective.model;
            next_config.reasoning_effort = effective.reasoning_effort;
            server.set_session_config(&next_config);
            server.prepare_thread(Some(&thread)).await?;
            server.run_turn(&thread, "verify").await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        server.close().await;
        result.unwrap();
    }

    #[test]
    fn image_turn_uses_local_image_inputs_alongside_text() {
        let images = vec![PathBuf::from("C:/workspace/inbox/photo.png")];
        let input = turn_inputs("查看附件", &images);
        assert_eq!(input[0]["type"], "text");
        assert_eq!(input[0]["text"], "查看附件");
        assert_eq!(
            input[1],
            json!({"type": "localImage", "path": "C:/workspace/inbox/photo.png"})
        );
        assert_eq!(turn_inputs("hello", &[]).len(), 1);
    }

    #[test]
    fn attachment_tool_is_registered_for_new_and_resumed_weixin_threads() {
        let spec = attachment_tool_spec();
        assert_eq!(spec["type"], "function");
        assert_eq!(spec["name"], ATTACHMENT_TOOL);
        assert_eq!(spec["inputSchema"]["required"], json!(["path", "kind"]));
        assert_eq!(spec["inputSchema"]["additionalProperties"], false);
    }

    #[test]
    fn permissions_preserve_requested_scope_and_reject_silent_sandbox_changes() {
        assert_eq!(permission_profile("workspace-write"), ":workspace");
        assert_eq!(
            permission_profile("danger-full-access"),
            ":danger-full-access"
        );
        assert_eq!(permission_profile("unknown"), ":read-only");
        for invalid in [
            json!({}),
            json!({"sandbox":null}),
            json!({"sandbox":{}}),
            json!({"sandbox":{"type":false}}),
        ] {
            assert!(verify_sandbox_policy(&invalid, "workspace-write").is_err());
        }
        for (kind, expected) in [
            ("readOnly", "read-only"),
            ("workspaceWrite", "workspace-write"),
            ("dangerFullAccess", "danger-full-access"),
        ] {
            assert!(verify_sandbox_policy(&json!({"sandbox":{"type":kind}}), expected).is_ok());
        }
        assert!(
            verify_sandbox_policy(&json!({"sandbox":{"type":"readOnly"}}), "workspace-write")
                .is_err()
        );
        assert!(
            verify_sandbox_policy(&json!({"sandbox":{"type":"dangerFullAccess"}}), "read-only")
                .is_err()
        );
        assert!(
            verify_sandbox_policy(&json!({"sandbox":{"type":"unexpected"}}), "read-only").is_err()
        );
    }

    #[test]
    fn legacy_context_copies_only_recent_conversation_text() {
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        std::fs::create_dir(&sessions).unwrap();
        let file = sessions.join("legacy.jsonl");
        let records = [
            json!({"type":"session_meta","payload":{"id":"legacy"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[
                {"type":"input_text","text":"分析这张图"},{"type":"input_image","image_url":"private"}
            ]}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[
                {"type":"input_text","text":format!("<environment_context>{}</environment_context>", "private".repeat(5000))}
            ]}}),
            json!({"type":"response_item","payload":{"type":"function_call_output","output":"private"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[
                {"type":"output_text","text":"分析完成"}
            ]}}),
        ];
        let bytes = records
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&file, &bytes).unwrap();
        let context = read_recent_rollout_context(temp.path(), &file, "legacy").unwrap();
        assert!(context.contains("用户：分析这张图"));
        assert!(context.contains("助手：分析完成"));
        assert!(!context.contains("private"));
        assert!(
            recent_conversation_text(&"中".repeat(30_000))
                .chars()
                .count()
                <= 24_001
        );
        let compressed = sessions.join("compressed.jsonl.zst");
        std::fs::write(
            &compressed,
            zstd::stream::encode_all(bytes.as_bytes(), 1).unwrap(),
        )
        .unwrap();
        assert_eq!(
            read_recent_rollout_context(temp.path(), &sessions.join("compressed.jsonl"), "legacy")
                .unwrap(),
            context
        );
        assert!(read_recent_rollout_context(temp.path(), &file, "wrong").is_err());
        let outside = temp.path().join("outside.jsonl");
        std::fs::write(&outside, &bytes).unwrap();
        assert!(read_recent_rollout_context(temp.path(), &outside, "legacy").is_err());
        std::fs::write(&file, "{broken").unwrap();
        assert!(read_recent_rollout_context(temp.path(), &file, "legacy").is_err());
        std::fs::write(&file, "x".repeat(1024 * 1024 + 1)).unwrap();
        assert!(read_recent_rollout_context(temp.path(), &file, "legacy").is_err());
    }

    #[test]
    fn legacy_context_accepts_only_declared_inherited_parent_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        std::fs::create_dir(&sessions).unwrap();
        let file = sessions.join("fork.jsonl");
        let mut records = [
            json!({"type":"session_meta","ordinal":0,"payload":{"id":"child","parent_thread_id":"parent","subagent_history_start_ordinal":5}}),
            json!({"type":"session_meta","ordinal":1,"payload":{"id":"parent"}}),
            json!({"type":"response_item","ordinal":5,"payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"child question"}]}}),
        ];
        let write = |records: &[Value]| {
            std::fs::write(
                &file,
                records
                    .iter()
                    .map(Value::to_string)
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
            .unwrap()
        };
        write(&records);
        assert!(
            read_recent_rollout_context(temp.path(), &file, "child")
                .unwrap()
                .contains("child question")
        );
        records[1]["ordinal"] = json!(5);
        write(&records);
        assert!(read_recent_rollout_context(temp.path(), &file, "child").is_err());
        records[1]["ordinal"] = json!(1);
        records[1]["payload"]["id"] = json!("foreign");
        write(&records);
        assert!(read_recent_rollout_context(temp.path(), &file, "child").is_err());
    }

    #[test]
    fn attachment_tool_rejects_traversal_missing_files_and_other_tools() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("result.txt"), "result").unwrap();
        std::fs::write(temp.path().join("private.txt"), "private").unwrap();
        let request = |path: &str, kind: &str| {
            json!({
                "params": {"tool": ATTACHMENT_TOOL, "arguments": {"path": path, "kind": kind}}
            })
        };
        let (path, kind) =
            attachment_tool_arguments(&request("result.txt", "file"), &root).unwrap();
        assert_eq!(path, root.join("result.txt").canonicalize().unwrap());
        assert_eq!(kind, AttachmentKind::File);
        assert!(attachment_tool_arguments(&request("../private.txt", "file"), &root).is_err());
        assert!(attachment_tool_arguments(&request(".", "file"), &root).is_err());
        assert!(attachment_tool_arguments(&request("missing.txt", "file"), &root).is_err());
        assert!(attachment_tool_arguments(&request("result.txt", "unknown"), &root).is_err());
        let mut other = request("result.txt", "file");
        other["params"]["tool"] = json!("other");
        assert!(attachment_tool_arguments(&other, &root).is_err());
    }

    #[test]
    fn outbound_attachment_workspace_rejects_sensitive_paths() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join(".ssh")).unwrap();
        for name in [".env", ".env.local", "auth.json", "config.toml"] {
            std::fs::write(root.join(name), "secret").unwrap();
        }
        for path in [
            root.join(".git").join("index"),
            root.join(".ssh").join("id"),
            root.join(".env"),
            root.join(".env.local"),
            root.join("auth.json"),
            root.join("config.toml"),
        ] {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            if !path.exists() {
                std::fs::write(&path, "secret").unwrap();
            }
            let request = json!({
                "params": {"tool": ATTACHMENT_TOOL, "arguments": {
                    "path": path.strip_prefix(&root).unwrap().to_string_lossy(),
                    "kind": "file"
                }}
            });
            assert!(attachment_tool_arguments(&request, &root).is_err(), "{path:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn attachment_tool_resolves_links_before_enforcing_workspace_boundary() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(temp.path().join("private.txt"), "private").unwrap();
        std::os::unix::fs::symlink(temp.path().join("private.txt"), root.join("link.txt")).unwrap();
        let request = json!({
            "params": {"tool": ATTACHMENT_TOOL, "arguments": {"path": "link.txt", "kind": "file"}}
        });
        assert!(attachment_tool_arguments(&request, &root).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn expired_managed_executable_resolves_after_desktop_update() {
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("OpenAI").join("Codex").join("bin");
        let old = bin.join("0123456789abcdef").join("codex.exe");
        let current = bin.join("abcdef0123456789").join("codex.exe");
        std::fs::create_dir_all(current.parent().unwrap()).unwrap();
        std::fs::write(&current, "cli").unwrap();
        let resolve = |path: &str| resolve_windows_startup_executable(path, Some(temp.path()));

        assert_eq!(resolve(&old.to_string_lossy()), current.to_string_lossy());
        assert_eq!(
            resolve(&bin.join("codex.exe").to_string_lossy()),
            current.to_string_lossy()
        );
        assert_eq!(
            resolve(
                &old.to_string_lossy()
                    .replace('\\', "/")
                    .to_ascii_uppercase()
            ),
            current.to_string_lossy()
        );
        assert_eq!(resolve(""), current.to_string_lossy());
        assert!(validate_codex_executable(&resolve(&old.to_string_lossy())).is_ok());

        // 仍存在的旧版本必须继续使用，不能强行切换用户选择。
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(&old, "old cli").unwrap();
        assert_eq!(resolve(&old.to_string_lossy()), old.to_string_lossy());
    }

    #[cfg(windows)]
    #[test]
    fn managed_recovery_preserves_custom_paths_and_missing_cli_errors() {
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("OpenAI/Codex/bin");
        let expired = bin.join("0123456789abcdef/codex.exe");
        let expired = expired.to_string_lossy().into_owned();
        assert_eq!(
            resolve_windows_startup_executable(&expired, Some(temp.path())),
            expired
        );
        assert!(validate_codex_executable(&expired).is_err());

        let current = bin.join("codex.exe");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(&current, "cli").unwrap();
        for custom in [
            temp.path()
                .join("custom/codex.exe")
                .to_string_lossy()
                .into_owned(),
            bin.join("custom/codex.exe").to_string_lossy().into_owned(),
            temp.path()
                .join("OpenAI/Codex/bin-other/0123456789abcdef/codex.exe")
                .to_string_lossy()
                .into_owned(),
            bin.join("0123456789abcdef/other.exe")
                .to_string_lossy()
                .into_owned(),
            "codex".to_string(),
        ] {
            assert_eq!(
                resolve_windows_startup_executable(&custom, Some(temp.path())),
                custom
            );
        }
        assert_eq!(resolve_windows_startup_executable(&expired, None), expired);
    }

    #[test]
    fn extracts_ids_from_current_app_server_shapes() {
        assert_eq!(
            extract_thread_id(&json!({"thread": {"id": "thread-1"}})).as_deref(),
            Some("thread-1")
        );
        assert_eq!(
            extract_turn_id(&json!({"turn": {"id": "turn-1"}})).as_deref(),
            Some("turn-1")
        );
    }

    #[test]
    fn extracts_completed_agent_message_only() {
        let message = json!({
            "method": "item/completed",
            "params": {"item": {"type": "agentMessage", "text": "done"}}
        });
        assert_eq!(
            extract_completed_agent_text(&message).as_deref(),
            Some("done")
        );
        let user = json!({
            "method": "item/completed",
            "params": {"item": {"type": "userMessage", "text": "input"}}
        });
        assert_eq!(extract_completed_agent_text(&user), None);
    }

    #[test]
    fn extracts_current_context_usage_event() {
        let message = json!({
            "method": "thread/tokenUsage/updated",
            "params": {
                "tokenUsage": {
                    "last": {"totalTokens": 41772},
                    "total": {"totalTokens": 50000},
                    "modelContextWindow": 1000000
                }
            }
        });
        assert_eq!(
            extract_turn_usage(&message),
            Some(TurnUsage {
                context_used: Some(41772),
                context_window: Some(1000000)
            })
        );
    }

    /// #1879：以前不管什么原因失败都只说「请检查 Codex CLI 路径」，
    /// 用户填了目录、填了不存在的路径、权限不对，看到的都是同一句话。
    #[test]
    fn executable_validation_names_the_actual_problem() {
        let temp = tempfile::tempdir().unwrap();

        // 目录而不是可执行文件——issue 里多人贴的正是带尾斜杠的目录路径
        let error = validate_codex_executable(&temp.path().display().to_string()).unwrap_err();
        assert!(error.to_string().contains("目录"), "{error}");

        // 路径不存在
        let missing = temp.path().join("nope").join("codex");
        let error = validate_codex_executable(&missing.display().to_string()).unwrap_err();
        assert!(error.to_string().contains("不存在"), "{error}");

        // 裸命令名交给 PATH 解析，不该在这里被拦下
        assert!(validate_codex_executable("codex").is_ok());
    }

    /// #2028：系统保护目录（WindowsApps）路径在启动前就被拦下，
    /// 并直接告诉用户怎么改，而不是等 spawn 失败后一句「没有执行权限」。
    #[test]
    fn windows_store_cli_path_is_rejected_with_actionable_message() {
        let error = validate_codex_executable(
            r"C:\Program Files\WindowsApps\OpenAI.Codex_26.915.4065.0_x64__2p2nqsd0c76g0\app\resources\codex.exe",
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("系统保护"), "{message}");
        assert!(message.contains("留空"), "{message}");
        assert!(message.contains("使用桌面版内置 CLI"), "{message}");
    }

    /// 留空即自动选用：显式填写的路径原样透传；空值在 Windows 上优先
    /// 桌面版维护的标准 CLI，取不到时退回 PATH（#2028 的零配置路径）。
    #[test]
    fn empty_executable_resolves_to_standard_cli_or_path() {
        assert_eq!(
            resolve_startup_executable("C:/x/codex.exe"),
            "C:/x/codex.exe"
        );
        let fallback = resolve_startup_executable("");
        assert!(
            fallback == "codex" || fallback.to_ascii_lowercase().ends_with("codex.exe"),
            "{fallback}"
        );
    }

    #[test]
    fn spawn_hints_distinguish_windows_store_and_missing_cli_cases() {
        let store = spawn_failure_hint(
            r"C:\Program Files\WindowsApps\OpenAI.Codex_1.0\app\resources\codex.exe",
            std::io::ErrorKind::PermissionDenied,
        );
        assert!(store.contains("系统保护"), "{store}");
        assert!(store.contains("清空"), "{store}");

        let plain = spawn_failure_hint(r"C:\codex\codex.exe", std::io::ErrorKind::PermissionDenied);
        assert_eq!(plain, "：没有执行权限");

        let missing = spawn_failure_hint("codex", std::io::ErrorKind::NotFound);
        if cfg!(windows) {
            assert!(missing.contains("使用桌面版内置 CLI"), "{missing}");
        } else {
            assert!(missing.contains("PATH"), "{missing}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn executable_validation_flags_a_file_without_exec_permission() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("codex");
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let error = validate_codex_executable(&path.display().to_string()).unwrap_err();
        assert!(error.to_string().contains("执行权限"), "{error}");

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(validate_codex_executable(&path.display().to_string()).is_ok());
    }

    /// app-server 起不来时，真正的原因只在 stderr 里；以前被读了就扔。
    #[test]
    fn stderr_tail_is_kept_and_bounded() {
        let sink = Arc::new(Mutex::new(VecDeque::<String>::new()));
        {
            let mut guard = sink.lock().unwrap();
            for i in 0..(STDERR_TAIL_LINES + 4) {
                if guard.len() == STDERR_TAIL_LINES {
                    guard.pop_front();
                }
                guard.push_back(format!("line-{i}"));
            }
        }
        let tail = collect_stderr_tail(&sink);
        assert!(tail.contains(&format!("line-{}", STDERR_TAIL_LINES + 3)));
        // 只留最近若干行，不能无限涨
        assert!(!tail.contains("line-0"));
        assert_eq!(tail.split('；').count(), STDERR_TAIL_LINES);
    }
}
