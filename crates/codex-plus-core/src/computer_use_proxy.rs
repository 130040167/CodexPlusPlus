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
//! 2. **补声明 elicitation**：Claude Code 不声明该能力，而原生 server 无论如何都会
//!    发 `elicitation/create` 并阻塞（实测），所以 `initialize` 时替客户端补上。
//! 3. **接管审批**：拦下 `elicitation/create` 并就地应答，同时留痕。
//!
//! 当前阶段就地 `accept`；审批卡与持久化在 M2 接进来（见设计文档 §5）。

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};

use anyhow::Context;
use serde_json::{Value, json};

use crate::computer_use::{RuntimeLocation, locate_runtime};

/// 单帧上限，与原生 CU 的 8 MiB 对齐。
const MAX_FRAME: usize = 8 * 1024 * 1024;
const ELICITATION_LOG: &str = "cu/elicitation.log";

enum Event {
    /// 客户端发来的原始帧（含结尾换行）。
    Client(Vec<u8>),
    /// 子进程发来的原始帧（含结尾换行）。
    Child(Vec<u8>),
    ChildClosed,
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
    };
    let outcome = forwarder.run(receiver);
    let _ = child.kill();
    let _ = child.wait();
    outcome
}

/// 拉起 OpenAI 签名的 node 执行 shim，由 shim 直接 spawn 原生 CU server。
///
/// 中间不经过 shell：服务端校验的是**直接父进程**的签名，多插一层就会失效。
fn spawn_child(runtime: &RuntimeLocation, shim: &std::path::Path) -> anyhow::Result<Child> {
    let mut command = Command::new(&runtime.node);
    command
        .arg(shim)
        .env("CODEX_PLUS_CU_CLIENT", &runtime.client)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(crate::windows_create_no_window());
    }
    command
        .spawn()
        .with_context(|| format!("无法启动 {}", runtime.node))
}

fn spawn_client_reader(sender: Sender<Event>) {
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut reader = BufReader::new(stdin.lock());
        loop {
            let mut frame = Vec::new();
            match read_frame(&mut reader, &mut frame) {
                Ok(0) => break,
                Ok(_) => {
                    if sender.send(Event::Client(frame)).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
}

fn spawn_child_reader(stdout: std::process::ChildStdout, sender: Sender<Event>) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut frame = Vec::new();
            match read_frame(&mut reader, &mut frame) {
                Ok(0) => break,
                Ok(_) => {
                    if sender.send(Event::Child(frame)).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = sender.send(Event::ChildClosed);
    });
}

/// 读一整帧（含结尾换行）。返回读到的字节数，0 表示 EOF。
///
/// 用 `read_until` 而不是 `lines()`：后者做 UTF-8 校验并吃掉换行，
/// 而直通层的目标是**字节不变**。
fn read_frame(reader: &mut impl BufRead, frame: &mut Vec<u8>) -> std::io::Result<usize> {
    let read = reader.read_until(b'\n', frame)?;
    if read > MAX_FRAME {
        frame.clear();
        return Ok(0);
    }
    Ok(read)
}

struct Forwarder {
    child_stdin: ChildStdin,
    /// 是否还需要在 `initialize` 里替客户端补 elicitation 能力。
    declare_elicitation: bool,
}

impl Forwarder {
    fn run(&mut self, receiver: Receiver<Event>) -> anyhow::Result<()> {
        while let Ok(event) = receiver.recv() {
            match event {
                Event::Client(frame) => {
                    if let Err(error) = self.on_client(frame) {
                        eprintln!("[codex-plus-cu] 处理客户端消息失败：{error:#}");
                        return Ok(());
                    }
                }
                Event::Child(frame) => self.on_child(frame)?,
                Event::ChildClosed => return Ok(()),
            }
        }
        Ok(())
    }

    /// 客户端 → 子进程。只有 `initialize` 会被改写（补 elicitation 能力），
    /// 其余帧原样转发。
    fn on_client(&mut self, frame: Vec<u8>) -> anyhow::Result<()> {
        if self.declare_elicitation
            && mentions(&frame, b"\"method\"")
            && mentions(&frame, b"\"initialize\"")
            && let Ok(mut message) = serde_json::from_slice::<Value>(&frame)
            && message.get("method").and_then(Value::as_str) == Some("initialize")
        {
            declare_elicitation(&mut message);
            self.declare_elicitation = false;
            let mut line = serde_json::to_vec(&message)?;
            line.push(b'\n');
            return self.write_child(&line);
        }
        self.write_child(&frame)
    }

    /// 子进程 → 客户端。唯一的例外是审批请求：就地应答，不转给客户端
    /// （Claude Code 不声明 elicitation，转过去没人答，只会挂住整条链路）。
    fn on_child(&mut self, frame: Vec<u8>) -> anyhow::Result<()> {
        if mentions(&frame, b"\"method\"") && mentions(&frame, b"elicitation/create") {
            if let Ok(message) = serde_json::from_slice::<Value>(&frame) {
                if message.get("method").and_then(Value::as_str) == Some("elicitation/create") {
                    record_elicitation(message.get("params"));
                    let reply = json!({
                        "jsonrpc": "2.0",
                        "id": message.get("id").cloned().unwrap_or(Value::Null),
                        "result": { "action": "accept", "content": {} }
                    });
                    let mut line = serde_json::to_vec(&reply)?;
                    line.push(b'\n');
                    return self.write_child(&line);
                }
            }
        }
        write_client(&frame)
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

/// 在客户端声明的能力里补上 `elicitation`，其余字段原样保留。
///
/// 早先的实现直接替换整个 `capabilities`（连 `clientInfo` 一起盖掉），
/// 那会让服务端看到的能力与客户端真实能力不符。
fn declare_elicitation(message: &mut Value) {
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

/// 审批被放行时留痕：哪条请求、什么标题、持久化策略。
fn record_elicitation(params: Option<&Value>) {
    let Some(params) = params else { return };
    let meta = params.get("_meta");
    let summary = json!({
        "at": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_secs())
            .unwrap_or(0),
        "kind": meta.and_then(|meta| meta.get("codex_approval_kind")).cloned(),
        "message": params.get("message").cloned().or_else(|| params.get("title").cloned()),
        "persist": meta.and_then(|meta| meta.get("persist")).cloned(),
    });
    let path = crate::paths::default_app_state_dir().join(ELICITATION_LOG);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(file, "{summary}");
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
        let mut frame = Vec::new();
        assert_eq!(read_frame(&mut reader, &mut frame).unwrap(), 8);
        assert_eq!(frame, b"{\"a\":1}\n");

        frame.clear();
        assert_eq!(read_frame(&mut reader, &mut frame).unwrap(), 8);
        assert_eq!(frame, b"{\"b\":2}\n");

        frame.clear();
        assert_eq!(read_frame(&mut reader, &mut frame).unwrap(), 0);
    }
}
