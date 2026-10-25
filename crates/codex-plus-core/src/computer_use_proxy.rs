//! Computer Use 的 MCP 直通层。
//!
//! # 为什么是「直通」而不是「代理」
//!
//! Codex 给模型的 CU 工具面来自 `SkyComputerUseClient mcp`（10 个离散工具），
//! 不是 `cua-repl` 的 code-mode。早先为了绕开它的签名校验，这条链路被改成
//! 「驱动 cua-repl + 在 Rust 里重新合成工具面」，代价是逐项偏差：信封、diff、
//! 截图分辨率、`App=` 行、`frontmost`、schema annotations 全靠手工对齐，
//! 任何一侧版本变化都会漂移。
//!
//! 实测（见 docs/plans/2026-10-23-codexpp-computer-use-native-passthrough-design.md）
//! 表明原生 server 只需要一个 OpenAI 签名的直接父进程：用 `cua_node/bin/node`
//! 执行 `assets/cu/shim.mjs` 拉起即可通过，`-10000 Sender process is not authenticated`
//! 不再出现。
//!
//! 所以本模块只做三件事：
//!
//! 1. **字节转发**：`tools/list` / `tools/call` / 结果 / 通知全部原样透传，
//!    不解析、不改写、不合成。id 也不重映射（1:1 会话，两端 id 天然一致）。
//! 2. **补声明 elicitation**：Claude Code 2.1.245 里虽然有 elicitation 的实现，
//!    但客户端能力里未必声明；原生 server 无论如何都会发该请求并阻塞（实测），
//!    所以 `initialize` 时替客户端补上，保证请求能被渲染。
//! 3. **审批**：按 `computer_use_approval` 设置决定就地放行还是转给客户端。

use std::collections::{BTreeSet, HashMap};
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use anyhow::Context;
use serde_json::{Value, json};

use crate::computer_use::{RuntimeLocation, locate_runtime};
use crate::computer_use_approval::{ApprovalMode, ApprovalStore, Outcome, subject_of};

/// 审批转给客户端后等多久。超过就回 `cancel`，不让会话永久挂住。
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);

/// 单帧上限，与原生 CU 的 8 MiB 对齐。超过的帧不转发，改回一个错误（见 [`Oversized`]）。
const MAX_FRAME: usize = 8 * 1024 * 1024;

/// 超长 JSON-RPC 错误码：服务端内部错误。
const OVERSIZED_ERROR_CODE: i64 = -32603;

enum Event {
    /// 客户端发来的原始帧（含结尾换行）。
    Client(Vec<u8>),
    /// 子进程发来的原始帧（含结尾换行）。
    Child(Vec<u8>),
    /// 客户端发来的帧超过上限，已丢弃。
    ClientOversized(Oversized),
    /// 子进程发来的帧超过上限，已丢弃。
    ChildOversized(Oversized),
    ClientClosed,
    ChildClosed,
}

/// 一条被丢弃的超长帧：只留下回错误所需的信息。
#[derive(Debug, Clone, PartialEq)]
struct Oversized {
    len: usize,
    id: Option<Value>,
    method: Option<String>,
}

impl Oversized {
    /// 从完整帧里只取 `id` 和 `method`。其余字段用 `IgnoredAny` 跳过，不会为截图分配内存；
    /// 帧不是合法 JSON 时两者都为空。
    fn of(frame: &[u8]) -> Self {
        #[derive(serde::Deserialize)]
        struct Envelope {
            #[serde(default)]
            id: Option<Value>,
            #[serde(default)]
            method: Option<String>,
        }
        let envelope = serde_json::from_slice::<Envelope>(frame).ok();
        Self {
            len: frame.len(),
            id: envelope.as_ref().and_then(|envelope| envelope.id.clone()).filter(|id| !id.is_null()),
            method: envelope.and_then(|envelope| envelope.method),
        }
    }

    /// 回给发送方的错误：只有带 id 的**请求**才需要答复（响应和通知不能回）。
    fn error_reply(&self, origin: &str) -> Option<Vec<u8>> {
        let id = self.id.as_ref()?;
        self.method.as_ref()?;
        let reply = json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": OVERSIZED_ERROR_CODE,
                "message": format!(
                    "{origin}超过 {} MiB 上限（{:.1} MiB），已丢弃，会话不受影响。",
                    MAX_FRAME / 1024 / 1024,
                    self.len as f64 / 1024.0 / 1024.0
                ),
            }
        });
        let mut line = serde_json::to_vec(&reply).ok()?;
        line.push(b'\n');
        Some(line)
    }

    /// 子进程超长帧回给客户端的错误：它是对客户端某个请求的**响应**，按那个 id 回。
    fn error_for_client(&self) -> Option<Vec<u8>> {
        let id = self.id.as_ref()?;
        if self.method.is_some() {
            return None;
        }
        let reply = json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": OVERSIZED_ERROR_CODE,
                "message": format!(
                    "Computer Use 返回内容超过 {} MiB 上限（{:.1} MiB），已丢弃，会话不受影响。\
                     可以缩小窗口或换个方式重试。",
                    MAX_FRAME / 1024 / 1024,
                    self.len as f64 / 1024.0 / 1024.0
                ),
            }
        });
        let mut line = serde_json::to_vec(&reply).ok()?;
        line.push(b'\n');
        Some(line)
    }
}

/// 读一帧的结果。
#[derive(Debug, PartialEq)]
enum Frame {
    /// 完整的一帧（含结尾换行）。
    Line(Vec<u8>),
    /// 超过上限的一帧，已丢弃。和 EOF 分开：早先两者共用返回值 0，
    /// 一条超长消息就被当成断开，整个 CU 会话随之结束。
    Oversized(Oversized),
    Eof,
}

/// 以 stdio MCP server 的身份运行，直到客户端断开。
pub fn serve() -> anyhow::Result<()> {
    let runtime =
        locate_runtime()?.context("未找到 Codex 运行时（ChatGPT.app），Computer Use 不可用")?;
    let shim = crate::computer_use::ensure_shim()?;
    let mut child = spawn_child(&runtime, &shim)?;
    let child_stdin = child.stdin.take().context("CU 子进程的标准输入不可用")?;
    let child_stdout = child.stdout.take().context("CU 子进程的标准输出不可用")?;

    let (sender, receiver) = mpsc::channel::<Event>();
    spawn_client_reader(sender.clone());
    spawn_child_reader(child_stdout, sender.clone());
    drop(sender);

    let mut forwarder = Forwarder {
        child_stdin,
        declare_elicitation: true,
        client_supports_elicitation: None,
        mode: ApprovalMode::current(),
        pending: HashMap::new(),
        session_allowed: BTreeSet::new(),
        next_token: 0,
    };
    let outcome = forwarder.run(receiver);
    child.kill();
    child.wait();
    outcome
}

/// 拉起 OpenAI 签名的 node 执行 shim，由 shim 直接 spawn 原生 CU server。
///
/// 三条约束：
/// - 环境只给原生 server 需要的那几项，不继承调用方的（Claude 的环境里有用户的 Key）。
/// - 中间不经过 shell：服务端校验**直接父进程**的签名，多插一层就会失效。
/// - 必须放弃责任归属（disclaim），让 node 成为自己的 responsible process。否则
///   Codex++ 被 Claude 的 `disclaimer` 或管理器 GUI 拉起时，责任进程是 ad-hoc 签名的
///   Codex++，原生客户端报 `bootstrapTimedOut`。见 `disclaimed_spawn` 顶部的实测表。
fn spawn_child(
    runtime: &RuntimeLocation,
    shim: &std::path::Path,
) -> anyhow::Result<crate::disclaimed_spawn::DisclaimedChild> {
    // 不继承调用方环境：见 `native_client_env` 的说明。
    let mut envs: Vec<(String, String)> = runtime.native_client_env().into_iter().collect();
    envs.push(("CODEX_PLUS_CU_CLIENT".into(), runtime.client.clone()));
    crate::disclaimed_spawn::spawn_clean(
        std::path::Path::new(&runtime.node),
        &[OsStr::new(shim)],
        &envs,
    )
}

fn spawn_client_reader(sender: Sender<Event>) {
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut reader = BufReader::new(stdin.lock());
        pump(&mut reader, &sender, Event::Client, Event::ClientOversized);
        // 客户端断开要通知主循环：只靠 channel 断开不够，子进程那一侧的 sender 还活着。
        let _ = sender.send(Event::ClientClosed);
    });
}

fn spawn_child_reader(stdout: Box<dyn Read + Send>, sender: Sender<Event>) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        pump(&mut reader, &sender, Event::Child, Event::ChildOversized);
        let _ = sender.send(Event::ChildClosed);
    });
}

/// 逐帧读到 EOF 或出错为止；超长帧照常上报，不中断读取。
fn pump(
    reader: &mut impl BufRead,
    sender: &Sender<Event>,
    line: fn(Vec<u8>) -> Event,
    oversized: fn(Oversized) -> Event,
) {
    loop {
        let event = match read_frame(reader) {
            Ok(Frame::Line(frame)) => line(frame),
            Ok(Frame::Oversized(info)) => oversized(info),
            Ok(Frame::Eof) | Err(_) => break,
        };
        if sender.send(event).is_err() {
            break;
        }
    }
}

/// 读一整帧（含结尾换行）。
///
/// 用 `read_until` 而不是 `lines()`：后者做 UTF-8 校验并吃掉换行，
/// 而直通层的目标是**字节不变**。
fn read_frame(reader: &mut impl BufRead) -> std::io::Result<Frame> {
    let mut frame = Vec::new();
    let read = reader.read_until(b'\n', &mut frame)?;
    if read == 0 {
        return Ok(Frame::Eof);
    }
    if read > MAX_FRAME {
        return Ok(Frame::Oversized(Oversized::of(&frame)));
    }
    Ok(Frame::Line(frame))
}

/// 已转给客户端、正在等答复的审批。
struct PendingApproval {
    /// 子进程原本的请求 id。
    child_id: Value,
    /// 用于记忆与审计的键（bundle id，缺失时是显示名）。
    key: String,
    display: String,
    started: Instant,
}

struct Forwarder {
    child_stdin: Box<dyn Write + Send>,
    /// 是否还需要在 `initialize` 里替客户端补 elicitation 能力。
    declare_elicitation: bool,
    /// 客户端自己是否声明了 elicitation。
    ///
    /// `None` = 还没握手。这个值决定 `claude` 模式能不能真的转发：
    /// Claude Code 2.x 不声明（转发出去没人渲染，只会等到超时），
    /// 3.x 之后声明与否要以实际握手为准，不能按版本号猜。
    client_supports_elicitation: Option<bool>,
    mode: ApprovalMode,
    /// 我们发给客户端的审批请求 id（字符串，避免与两端数字 id 撞号）→ 待办。
    pending: HashMap<String, PendingApproval>,
    /// 本进程内已放行的 app（`session` 语义，不落盘）。
    session_allowed: BTreeSet<String>,
    next_token: u64,
}

impl Forwarder {
    fn run(&mut self, receiver: Receiver<Event>) -> anyhow::Result<()> {
        // 用 `recv_timeout` 而不是 `recv`：审批超时需要在**没有消息**时也能触发。
        loop {
            match receiver.recv_timeout(Duration::from_millis(500)) {
                Ok(Event::Client(frame)) => {
                    if let Err(error) = self.on_client(frame) {
                        eprintln!("[codex-plus-cu] 处理客户端消息失败：{error:#}");
                        return Ok(());
                    }
                }
                Ok(Event::Child(frame)) => self.on_child(frame)?,
                Ok(Event::ChildOversized(info)) => self.on_child_oversized(&info)?,
                Ok(Event::ClientOversized(info)) => self.on_client_oversized(&info)?,
                Ok(Event::ClientClosed) => {
                    self.cancel_all("客户端已断开");
                    return Ok(());
                }
                Ok(Event::ChildClosed) => {
                    self.cancel_all("CU 子进程已退出");
                    return Ok(());
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.cancel_all("客户端已断开");
                    return Ok(());
                }
            }
            self.expire_pending()?;
        }
    }

    /// 客户端 → 子进程。
    ///
    /// 两种帧需要处理：`initialize` 要补 elicitation 能力；对本代理发出的审批
    /// 请求的答复要换回子进程的原始 id。
    fn on_client(&mut self, frame: Vec<u8>) -> anyhow::Result<()> {
        if self.declare_elicitation
            && mentions(&frame, b"\"method\"")
            && mentions(&frame, b"\"initialize\"")
            && let Ok(mut message) = serde_json::from_slice::<Value>(&frame)
            && message.get("method").and_then(Value::as_str) == Some("initialize")
        {
            // 先记下客户端**自己**有没有声明，再补。补只是为了不让服务端拒绝，
            // 不代表客户端真的会渲染这个表单——`claude` 模式要看这个值决定能否转发。
            let supports = declares_capability(&message, "elicitation");
            self.client_supports_elicitation = Some(supports);
            crate::computer_use_approval::record_client(
                message.get("params").and_then(|params| params.get("clientInfo")),
                supports,
            );
            declare_elicitation(&mut message);
            self.declare_elicitation = false;
            let mut line = serde_json::to_vec(&message)?;
            line.push(b'\n');
            return self.write_child(&line);
        }
        // 审批答复：没有 method、id 是我们发的字符串。
        if !mentions(&frame, b"\"method\"")
            && mentions(&frame, b"\"result\"")
            && let Ok(message) = serde_json::from_slice::<Value>(&frame)
            && let Some(token) = message.get("id").and_then(Value::as_str)
            && let Some(pending) = self.pending.remove(token)
        {
            return self.relay_approval(&pending, &message);
        }
        self.write_child(&frame)
    }

    /// 子进程发来的超长帧：是对客户端请求的响应就按原 id 回错误；是子进程发的请求
    /// （例如超长的审批请求）就回子进程错误，免得它一直等；通知直接丢掉。
    fn on_child_oversized(&mut self, info: &Oversized) -> anyhow::Result<()> {
        eprintln!(
            "[codex-plus-cu] 丢弃 CU 发来的超长消息：{} 字节，method={:?}",
            info.len, info.method
        );
        if let Some(line) = info.error_for_client() {
            return write_client(&line);
        }
        if let Some(line) = info.error_reply("发给 CU 客户端的请求") {
            return self.write_child(&line);
        }
        Ok(())
    }

    /// 客户端发来的超长帧：是请求就回客户端错误，响应和通知直接丢掉。
    fn on_client_oversized(&mut self, info: &Oversized) -> anyhow::Result<()> {
        eprintln!(
            "[codex-plus-cu] 丢弃客户端发来的超长消息：{} 字节，method={:?}",
            info.len, info.method
        );
        match info.error_reply("请求") {
            Some(line) => write_client(&line),
            None => Ok(()),
        }
    }

    /// 逐条答复客户端对审批请求的决定。
    fn relay_approval(&mut self, pending: &PendingApproval, message: &Value) -> anyhow::Result<()> {
        let action = message
            .get("result")
            .and_then(|result| result.get("action"))
            .and_then(Value::as_str)
            .unwrap_or("cancel");
        let content = message
            .get("result")
            .and_then(|result| result.get("content"))
            .cloned()
            .unwrap_or_else(|| json!({}));
        let outcome = match action {
            "accept" => Outcome::ClientAccepted,
            "decline" => Outcome::ClientDeclined,
            _ => Outcome::ClientCanceled,
        };
        if outcome.accepted() {
            // `session` 语义在本进程内生效；跨会话的持久化交给 `remember`。
            self.session_allowed.insert(pending.key.clone());
        }
        let reply = json!({
            "jsonrpc": "2.0",
            "id": pending.child_id,
            "result": { "action": action, "content": content }
        });
        let mut line = serde_json::to_vec(&reply)?;
        line.push(b'\n');
        self.write_child(&line)?;
        self.audit(pending, outcome);
        Ok(())
    }

    /// 子进程 → 客户端。审批请求按策略处理，其余帧原样透传。
    fn on_child(&mut self, frame: Vec<u8>) -> anyhow::Result<()> {
        if mentions(&frame, b"\"method\"") && mentions(&frame, b"elicitation/create") {
            if let Ok(message) = serde_json::from_slice::<Value>(&frame) {
                if message.get("method").and_then(Value::as_str) == Some("elicitation/create") {
                    return self.on_elicitation(message);
                }
            }
        }
        write_client(&frame)
    }

    fn on_elicitation(&mut self, message: Value) -> anyhow::Result<()> {
        let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
        let (key, display) = subject_of(&params);
        let child_id = message.get("id").cloned().unwrap_or(Value::Null);

        // 1) 本进程已放行过 / 文件里记忆过 → 直接 accept。
        let remembered = if self.session_allowed.contains(&key) {
            true
        } else {
            ApprovalStore::load()
                .apps
                .get(&key)
                .map(|entry| entry.decision == "allow")
                .unwrap_or(false)
        };
        if remembered {
            let pending = PendingApproval {
                child_id: child_id.clone(),
                key,
                display,
                started: Instant::now(),
            };
            return self.accept_now(&pending, Outcome::Remembered);
        }

        // 2) `auto` 模式：就地放行，只留痕。
        if self.mode == ApprovalMode::Auto {
            let pending = PendingApproval {
                child_id,
                key,
                display,
                started: Instant::now(),
            };
            return self.accept_now(&pending, Outcome::AutoAccepted);
        }

        // 3) `claude` 模式：只有客户端**自己**声明了 elicitation 才转发。
        //
        // 客户端没声明时转发出去没人渲染，只能干等到超时——那比直接放行更糟：
        // 用户会看到「读应用列表正常、一点击就卡两分钟」。
        // 版本号不能作为依据（2.x 不声明、3.x 之后可能变），一律以握手时的实际能力为准。
        if self.client_supports_elicitation == Some(false) {
            let pending = PendingApproval {
                child_id,
                key,
                display,
                started: Instant::now(),
            };
            eprintln!(
                "[codex-plus-cu] 客户端未声明 elicitation，`claude` 审批模式退回放行；\
                 如需真正审批请改用支持 elicitation 的客户端"
            );
            return self.accept_now(&pending, Outcome::AutoAccepted);
        }
        self.next_token += 1;
        let token = format!("codexplus-cu-elicit-{}", self.next_token);
        let forwarded = json!({
            "jsonrpc": "2.0",
            "id": token,
            "method": "elicitation/create",
            "params": params,
        });
        let mut line = serde_json::to_vec(&forwarded)?;
        line.push(b'\n');
        self.pending.insert(
            token,
            PendingApproval {
                child_id,
                key,
                display,
                started: Instant::now(),
            },
        );
        write_client(&line)
    }

    fn accept_now(&mut self, pending: &PendingApproval, outcome: Outcome) -> anyhow::Result<()> {
        let reply = json!({
            "jsonrpc": "2.0",
            "id": pending.child_id,
            "result": { "action": "accept", "content": {} }
        });
        let mut line = serde_json::to_vec(&reply)?;
        line.push(b'\n');
        self.write_child(&line)?;
        if outcome == Outcome::AutoAccepted {
            self.session_allowed.insert(pending.key.clone());
        }
        self.audit(pending, outcome);
        Ok(())
    }

    /// 超时的审批一律回 `cancel`，避免客户端不答就把整条链路吊死。
    fn expire_pending(&mut self) -> anyhow::Result<()> {
        let expired: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, pending)| pending.started.elapsed() >= APPROVAL_TIMEOUT)
            .map(|(token, _)| token.clone())
            .collect();
        for token in expired {
            if let Some(pending) = self.pending.remove(&token) {
                let reply = json!({
                    "jsonrpc": "2.0",
                    "id": pending.child_id,
                    "result": { "action": "cancel", "content": {} }
                });
                let mut line = serde_json::to_vec(&reply)?;
                line.push(b'\n');
                self.write_child(&line)?;
                self.audit(&pending, Outcome::ClientCanceled);
            }
        }
        Ok(())
    }

    fn cancel_all(&mut self, _reason: &str) {
        // 先把待办取出来再逐个答复：直接 drain 会同时可变借出 self.pending 与 self。
        let pending: Vec<(String, PendingApproval)> = self.pending.drain().collect();
        for (_, pending) in pending {
            let reply = json!({
                "jsonrpc": "2.0",
                "id": pending.child_id,
                "result": { "action": "cancel", "content": {} }
            });
            if let Ok(mut line) = serde_json::to_vec(&reply) {
                line.push(b'\n');
                let _ = self.write_child(&line);
            }
            self.audit(&pending, Outcome::ClientCanceled);
        }
    }

    fn audit(&self, pending: &PendingApproval, outcome: Outcome) {
        let params = json!({
            "message": format!("Allow Computer Use to use \"{}\"?", pending.display),
            "_meta": { "tool_params": { "app": pending.key } }
        });
        crate::computer_use_approval::audit(&params, self.mode, outcome);
    }

    fn write_child(&mut self, frame: &[u8]) -> anyhow::Result<()> {
        self.child_stdin
            .write_all(frame)
            .context("无法写入 CU 子进程的标准输入")?;
        self.child_stdin
            .flush()
            .context("无法刷新 CU 子进程的标准输入")?;
        Ok(())
    }
}

/// 子串预筛：避免把几百 KB 的截图帧反复做 JSON 解析。
fn mentions(frame: &[u8], needle: &[u8]) -> bool {
    frame.windows(needle.len()).any(|window| window == needle)
}

fn write_client(frame: &[u8]) -> anyhow::Result<()> {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    lock.write_all(frame)?;
    lock.flush()?;
    Ok(())
}

/// 客户端自己有没有声明某个能力（`params.capabilities.<name>` 存在且非 null）。
pub(crate) fn declares_capability(message: &Value, name: &str) -> bool {
    message
        .get("params")
        .and_then(|params| params.get("capabilities"))
        .and_then(|capabilities| capabilities.get(name))
        .map(|value| !value.is_null())
        .unwrap_or(false)
}

/// 在客户端声明的能力里补上 `elicitation`，其余字段原样保留。
///
/// 早先的实现直接替换整个 `capabilities`（连 `clientInfo` 一起盖掉），
/// 那会让服务端看到的能力与客户端真实能力不符。
pub(crate) fn declare_elicitation(message: &mut Value) {
    let Some(params) = message.get_mut("params").and_then(Value::as_object_mut) else {
        return;
    };
    let capabilities = params
        .entry("capabilities")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if !capabilities.is_object() {
        *capabilities = Value::Object(serde_json::Map::new());
    }
    if let Some(object) = capabilities.as_object_mut() {
        object
            .entry("elicitation")
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declare_elicitation_keeps_existing_capabilities() {
        let mut message = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": { "roots": { "listChanged": true } },
                "clientInfo": { "name": "claude-code", "version": "2.1.245" }
            }
        });
        declare_elicitation(&mut message);
        let params = &message["params"];
        assert!(params["capabilities"]["elicitation"].is_object());
        assert_eq!(params["capabilities"]["roots"]["listChanged"], true);
        // clientInfo 不能被盖掉：服务端会据此判断调用方。
        assert_eq!(params["clientInfo"]["name"], "claude-code");
    }

    #[test]
    fn declare_elicitation_creates_capabilities_when_missing() {
        let mut message = json!({ "method": "initialize", "params": {} });
        declare_elicitation(&mut message);
        assert!(message["params"]["capabilities"]["elicitation"].is_object());
    }

    #[test]
    fn mentions_works_as_cheap_prefilter() {
        let frame = br#"{"jsonrpc":"2.0","method":"elicitation/create","params":{}}"#;
        assert!(mentions(frame, b"\"method\""));
        assert!(mentions(frame, b"elicitation/create"));
        assert!(!mentions(frame, b"\"tools/list\""));
    }

    #[test]
    fn read_frame_preserves_bytes_and_newline() {
        let mut reader = BufReader::new(&b"{\"a\":1}\n{\"b\":2}\n"[..]);
        assert_eq!(read_frame(&mut reader).unwrap(), Frame::Line(b"{\"a\":1}\n".to_vec()));
        assert_eq!(read_frame(&mut reader).unwrap(), Frame::Line(b"{\"b\":2}\n".to_vec()));
        assert_eq!(read_frame(&mut reader).unwrap(), Frame::Eof);
    }

    /// 一个超过上限的 tools/call 响应：截图塞在 result 里，id 在前。
    fn huge_response(id: Value) -> Vec<u8> {
        let image = "A".repeat(MAX_FRAME + 16);
        let mut frame = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "content": [{ "type": "image", "data": image }] }
        }))
        .unwrap();
        frame.push(b'\n');
        frame
    }

    #[test]
    fn oversized_frame_is_skipped_and_reading_continues() {
        let mut bytes = huge_response(json!(7));
        bytes.extend_from_slice(b"{\"jsonrpc\":\"2.0\",\"id\":8,\"result\":{}}\n");
        let mut reader = BufReader::new(&bytes[..]);

        let Frame::Oversized(info) = read_frame(&mut reader).unwrap() else {
            panic!("expected oversized frame");
        };
        assert_eq!(info.id, Some(json!(7)));
        assert_eq!(info.method, None);
        assert!(info.len > MAX_FRAME);
        // 超长帧之后的消息照常读到，不再被当成 EOF。
        assert!(matches!(read_frame(&mut reader).unwrap(), Frame::Line(_)));
        assert_eq!(read_frame(&mut reader).unwrap(), Frame::Eof);
    }

    #[test]
    fn oversized_child_response_becomes_error_with_same_id() {
        let info = Oversized::of(&huge_response(json!("req-1")));
        let reply: Value = serde_json::from_slice(&info.error_for_client().unwrap()).unwrap();
        assert_eq!(reply["id"], "req-1");
        assert_eq!(reply["error"]["code"], OVERSIZED_ERROR_CODE);
        assert!(reply["error"]["message"].as_str().unwrap().contains("已丢弃"));
        // 响应不是请求，不能再回给子进程。
        assert!(info.error_reply("x").is_none());
    }

    #[test]
    fn oversized_request_is_answered_and_notifications_are_dropped() {
        let request = Oversized { len: MAX_FRAME + 1, id: Some(json!(3)), method: Some("tools/call".into()) };
        let reply: Value = serde_json::from_slice(&request.error_reply("请求").unwrap()).unwrap();
        assert_eq!(reply["id"], 3);
        assert!(request.error_for_client().is_none());

        let notification = Oversized { len: MAX_FRAME + 1, id: None, method: Some("notifications/x".into()) };
        assert!(notification.error_reply("请求").is_none());
        assert!(notification.error_for_client().is_none());

        // 截断或非法 JSON：拿不到 id，只能丢弃。
        let garbage = Oversized::of(b"{\"id\":1, oops");
        assert_eq!(garbage.id, None);
        assert!(garbage.error_for_client().is_none());
    }
}
