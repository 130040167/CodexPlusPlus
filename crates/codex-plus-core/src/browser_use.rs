//! Chrome 浏览器的 MCP 服务。
//!
//! # 为什么不复用 CU 那条直通链路
//!
//! 原生的 `SkyComputerUseClient` **没有浏览器工具**——它只管桌面窗口。Codex 的
//! 浏览器面走的是另一条完全不同的链路：
//!
//! ```text
//! cua-repl(js) ──► @oai/browser-desktop ──► Chrome MV3 扩展 ──► native messaging
//! ```
//!
//! 这条链路有两个硬约束（实测）：
//!
//! 1. 每次 `tools/call` 都要带 `_meta["x-codex-turn-metadata"] = {session_id, turn_id}`，
//!    缺了直接报 `Missing required Codex turn metadata`。
//! 2. 浏览器能力必须跑在 `node_repl` 里（`setupBrowserRuntime` 检查 `nodeRepl.rpc`），
//!    裸 `node` 不行，所以起点只能是 `cua-repl`。
//!
//! # 工具面
//!
//! 对 Claude 暴露离散工具（而不是让模型写 JS）：标签页、导航、AX 快照、点击、输入、
//! 执行 JS、截图、标签分组。每个工具内部翻译成一段 `@oai/sky`/`cua` 的 JS，交给
//! `cua-repl` 的 `js` 工具执行，再把结果从 `@@B@@…@@E@@` 信封里解出来。
//!
//! # 与 Codex 的差异
//!
//! Codex 自己在 code-mode 里让模型直接写 JS；这里改成离散工具，是为了让 Claude 的
//! 工具调用更稳、审计更清楚。语义（`cua.*` 的调用形状、标签分组能力）保持一致。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};

use anyhow::Context;
use serde_json::{Value, json};

use crate::computer_use::RuntimeLocation;

const MAX_FRAME: usize = 8 * 1024 * 1024;
const PAYLOAD_OPEN: &str = "@@B@@";
const PAYLOAD_CLOSE: &str = "@@E@@";

enum Event {
    Client(Value),
    Child(Value),
    ChildClosed,
}

/// 浏览器能力的 MCP 服务入口。
pub fn serve() -> anyhow::Result<()> {
    let runtime = crate::computer_use::locate_runtime()?
        .context("未找到 Codex 运行时（ChatGPT.app），浏览器控制不可用")?;
    // 浏览器面必须走 cua-repl（`nodeRepl.rpc` 由它注入），与 CU 的直通路径不同。
    anyhow::ensure!(
        std::path::Path::new(&runtime.cua_repl).is_file(),
        "当前 ChatGPT.app 不再提供 cua-repl，浏览器控制不可用"
    );
    let mut child = spawn_child(&runtime)?;
    let child_stdin = child.stdin.take().context("cua-repl 的标准输入不可用")?;
    let child_stdout = child.stdout.take().context("cua-repl 的标准输出不可用")?;

    let (sender, receiver) = mpsc::channel::<Event>();
    spawn_client_reader(sender.clone());
    spawn_child_reader(child_stdout, sender.clone());
    drop(sender);

    let mut state = ProxyState::new(child_stdin);
    let outcome = state.run(receiver);
    let _ = child.kill();
    let _ = child.wait();
    outcome
}

fn spawn_child(runtime: &RuntimeLocation) -> anyhow::Result<Child> {
    let mut command = Command::new(&runtime.node);
    command
        .arg(&runtime.cua_repl)
        .envs(runtime.browser_env())
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
        .with_context(|| format!("无法启动 {}", runtime.cua_repl))
}

fn spawn_client_reader(sender: Sender<Event>) {
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            if line.len() > MAX_FRAME {
                continue;
            }
            if let Ok(value) = serde_json::from_str::<Value>(&line) {
                if sender.send(Event::Client(value)).is_err() {
                    break;
                }
            }
        }
    });
}

fn spawn_child_reader(stdout: std::process::ChildStdout, sender: Sender<Event>) {
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() || line.len() > MAX_FRAME {
                continue;
            }
            if let Ok(value) = serde_json::from_str::<Value>(&line) {
                if sender.send(Event::Child(value)).is_err() {
                    break;
                }
            }
        }
        let _ = sender.send(Event::ChildClosed);
    });
}

struct ProxyState {
    child_stdin: ChildStdin,
    next_id: i64,
    /// 内部 id → 客户端 id。
    pending: HashMap<i64, Value>,
    /// 内部 id → 本地工具名（结果要按它整形）。
    plans: HashMap<i64, String>,
    session_id: String,
    turn_seq: u64,
    /// `initialize` 的内部 id：它的结果要换掉 cua-repl 的 instructions。
    init_child_id: Option<i64>,
}

impl ProxyState {
    fn new(child_stdin: ChildStdin) -> Self {
        Self {
            child_stdin,
            next_id: 1000,
            pending: HashMap::new(),
            plans: HashMap::new(),
            session_id: uuid::Uuid::new_v4().to_string(),
            turn_seq: 0,
            init_child_id: None,
        }
    }

    fn run(&mut self, receiver: Receiver<Event>) -> anyhow::Result<()> {
        while let Ok(event) = receiver.recv() {
            match event {
                Event::Client(message) => {
                    if let Err(error) = self.on_client(message) {
                        eprintln!("[codex-plus-browser] 处理客户端消息失败：{error:#}");
                        return Ok(());
                    }
                }
                Event::Child(message) => self.on_child(message),
                Event::ChildClosed => return Ok(()),
            }
        }
        Ok(())
    }

    fn on_client(&mut self, mut message: Value) -> anyhow::Result<()> {
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        match method.as_str() {
            "initialize" => {
                // 只补 elicitation，不动客户端其余能力：cua-repl 遇到需要许可的
                // 浏览器操作时会发 elicitation/create，客户端没声明就直接抛
                // `createElicitation is unavailable`。请求由本 server 就地应答（见 on_child）。
                crate::computer_use_proxy::declare_elicitation(&mut message);
                self.init_child_id = self.remap(&mut message);
                return self.forward(message);
            }
            "tools/list" => {
                if let Some(id) = message.get("id").cloned() {
                    let _ = write_client(&json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "tools": tool_definitions() }
                    }));
                }
                return Ok(());
            }
            "tools/call" => {
                // 浏览器桥要求每次调用都带 turn 元数据，缺了直接拒绝。
                inject_turn_metadata(&mut message, &self.session_id, &mut self.turn_seq);
                if let Some((tool, code)) = build_js_call(&message) {
                    if let Some(params) = message.get_mut("params").and_then(Value::as_object_mut) {
                        params.insert("name".into(), json!("js"));
                        params.insert("arguments".into(), json!({ "code": code }));
                    }
                    if let Some(child_id) = self.remap(&mut message) {
                        self.plans.insert(child_id, tool);
                    }
                    return self.forward(message);
                }
            }
            _ => {}
        }
        self.remap(&mut message);
        self.forward(message)
    }

    fn on_child(&mut self, mut message: Value) {
        if message.get("method").is_some() {
            // 服务端 → 客户端的请求：elicitation 直接同意（浏览器面的审批由 Codex
            // 扩展侧负责，这里只需要不阻塞），其余回 method not found。
            if let Some(id) = message.get("id").cloned() {
                let method = message
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let reply = if method == "elicitation/create" {
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "action": "accept", "content": {} }
                    })
                } else {
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32601, "message": format!("codex-plus-browser 未实现 {method}") }
                    })
                };
                let _ = self.write_child(&reply);
                return;
            }
            let _ = write_client(&message);
            return;
        }
        if let Some(child_id) = message.get("id").and_then(Value::as_i64) {
            if let Some(client_id) = self.pending.remove(&child_id) {
                message["id"] = client_id;
                if let Some(tool) = self.plans.remove(&child_id) {
                    let reshaped = reshape_result(&tool, message);
                    let _ = write_client(&reshaped);
                    return;
                }
                if self.init_child_id == Some(child_id) {
                    replace_instructions(&mut message);
                }
            }
        }
        let _ = write_client(&message);
    }

    fn remap(&mut self, message: &mut Value) -> Option<i64> {
        let client_id = message.get("id").cloned()?;
        let child_id = self.next_id;
        self.next_id += 1;
        self.pending.insert(child_id, client_id);
        message["id"] = json!(child_id);
        Some(child_id)
    }

    fn forward(&mut self, message: Value) -> anyhow::Result<()> {
        self.write_child(&message)
    }

    fn write_child(&mut self, message: &Value) -> anyhow::Result<()> {
        let mut line = serde_json::to_string(message)?;
        line.push('\n');
        self.child_stdin
            .write_all(line.as_bytes())
            .context("无法写入 cua-repl 标准输入")?;
        self.child_stdin.flush().context("无法刷新 cua-repl 标准输入")?;
        Ok(())
    }
}

/// 每次 `tools/call` 都要带 Codex 的 turn 元数据，否则浏览器桥直接拒绝。
fn inject_turn_metadata(message: &mut Value, session_id: &str, seq: &mut u64) {
    *seq += 1;
    let Some(params) = message.get_mut("params").and_then(Value::as_object_mut) else {
        return;
    };
    let meta = params
        .entry("_meta")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if let Some(meta) = meta.as_object_mut() {
        meta.insert(
            "x-codex-turn-metadata".into(),
            json!({ "session_id": session_id, "turn_id": format!("{session_id}-{seq}") }),
        );
    }
}

/// 对外的使用说明。
///
/// cua-repl 的握手结果写的是「UI automation through cua_repl」，那是给 code-mode 用的；
/// 本 server 对外只有离散的 `browser_*` 工具，没有 `js`，原样透传会误导模型去找不存在的工具。
const INSTRUCTIONS: &str = "Control the user's real Google Chrome through the Codex browser extension.\n\
\n\
- Start with `browser_tabs` (or `browser_open` for a new page). Tab ids come from those results.\n\
- Read a page with `browser_state` before acting; prefer `element_index` from it over coordinates.\n\
- `browser_eval` runs a JavaScript expression in the page and returns its value.\n\
- Group tabs only when the user asks: `browser_group`, then `browser_group_update` to rename or recolour.\n\
- Tabs opened by another session cannot be driven here; open a fresh one with `browser_open` instead.";

fn replace_instructions(message: &mut Value) {
    if let Some(result) = message.get_mut("result").and_then(Value::as_object_mut) {
        result.insert("instructions".into(), Value::String(INSTRUCTIONS.into()));
    }
}

fn write_client(message: &Value) -> anyhow::Result<()> {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let mut line = serde_json::to_string(message)?;
    line.push('\n');
    lock.write_all(line.as_bytes())?;
    lock.flush()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 工具面
// ---------------------------------------------------------------------------

fn tool_definitions() -> Vec<Value> {
    let tab = json!({ "type": "string", "description": "Tab id or provider tab id" });
    let browser = json!({
        "type": "string",
        "description": "Browser id or short name (chrome / edge / iab). Defaults to chrome"
    });
    vec![
        json!({
            "name": "browser_list",
            "description": "List the browsers this machine can control (Chrome via the Codex extension, plus any embedded ones).",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        }),
        json!({
            "name": "browser_tabs",
            "description": "List open tabs. Omit `browser` to list tabs across every available browser.",
            "inputSchema": {
                "type": "object",
                "properties": { "browser": browser },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "browser_open",
            "description": "Open a URL in a new background tab and return its tab id.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "Absolute URL to open" },
                    "browser": browser
                },
                "required": ["url"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "browser_close",
            "description": "Close a tab.",
            "inputSchema": {
                "type": "object",
                "properties": { "tab": tab, "browser": browser },
                "required": ["tab"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "browser_state",
            "description": "Read a tab's state: the accessibility tree with element indices when supported, otherwise a DOM snapshot.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab": tab,
                    "browser": browser,
                    "disable_diff": { "type": "boolean", "description": "Return the full tree instead of a diff. Defaults to true" }
                },
                "required": ["tab"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "browser_click",
            "description": "Click an element by its accessibility index, or by page coordinates.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab": tab,
                    "browser": browser,
                    "element_index": { "type": "integer", "description": "Element index from browser_state" },
                    "x": { "type": "number", "description": "X coordinate in page pixels" },
                    "y": { "type": "number", "description": "Y coordinate in page pixels" },
                    "click_count": { "type": "integer", "description": "Number of clicks. Defaults to 1" },
                    "mouse_button": { "type": "string", "enum": ["left", "right", "middle"] }
                },
                "required": ["tab"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "browser_type",
            "description": "Type text into an element, or into whatever has focus when `element_index` is omitted.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab": tab,
                    "browser": browser,
                    "text": { "type": "string", "description": "Literal text to type" },
                    "element_index": { "type": "integer", "description": "Target element index" }
                },
                "required": ["tab", "text"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "browser_key",
            "description": "Press a key or key combination (xdotool syntax: \"Return\", \"super+c\", \"Tab\").",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab": tab,
                    "browser": browser,
                    "key": { "type": "string", "description": "Key or key combination" }
                },
                "required": ["tab", "key"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "browser_eval",
            "description": "Evaluate a JavaScript expression in the page and return its serializable value.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab": tab,
                    "browser": browser,
                    "expression": { "type": "string", "description": "JavaScript expression, e.g. document.title" }
                },
                "required": ["tab", "expression"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "browser_screenshot",
            "description": "Capture the tab as a JPEG image.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab": tab,
                    "browser": browser,
                    "full_page": { "type": "boolean", "description": "Capture the whole page instead of the viewport" }
                },
                "required": ["tab"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "browser_group",
            "description": "Put tabs into a tab group, optionally naming and colouring it. All tabs must be in the same window.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tabs": { "type": "array", "items": { "type": "integer" }, "description": "Tab ids to group" },
                    "browser": browser,
                    "title": { "type": "string", "description": "Group title" },
                    "color": { "type": "string", "description": "Group colour (grey, blue, red, yellow, green, pink, purple, cyan, orange)" },
                    "collapsed": { "type": "boolean", "description": "Collapse the group" }
                },
                "required": ["tabs"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "browser_group_update",
            "description": "Rename, recolour or collapse an existing tab group.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "group": { "type": "integer", "description": "Tab group id" },
                    "browser": browser,
                    "title": { "type": "string" },
                    "color": { "type": "string" },
                    "collapsed": { "type": "boolean" }
                },
                "required": ["group"],
                "additionalProperties": false
            }
        }),
    ]
}

/// 把一次本地工具调用翻译成 `js` 代码。返回（工具名，代码）。
fn build_js_call(message: &Value) -> Option<(String, String)> {
    let params = message.get("params")?.as_object()?;
    let tool = params.get("name")?.as_str()?.to_string();
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let browser = args
        .get("browser")
        .and_then(Value::as_str)
        .map(str::to_string);
    let browser_arg = browser
        .as_deref()
        .map(|value| format!(", {{browser:{}}}", serde_json::to_string(value).unwrap()))
        .unwrap_or_default();
    let browser_id = browser
        .as_deref()
        .map(|value| serde_json::to_string(value).unwrap())
        .unwrap_or_else(|| "\"chrome\"".to_string());

    let body = match tool.as_str() {
        "browser_list" => "out.browsers = await cua.listBrowsers({emit:false});".to_string(),
        "browser_tabs" => format!(
            "out.tabs = await cua.listTabs({{emit:false{}}});",
            browser
                .as_deref()
                .map(|value| format!(", browser:{}", serde_json::to_string(value).unwrap()))
                .unwrap_or_default()
        ),
        "browser_open" => {
            let url = serde_json::to_string(args.get("url")?.as_str()?).ok()?;
            // `visible` / `sessionName` 后端不支持时**直接抛错**（不静默降级），
            // 所以先查能力再决定传不传。Chrome 扩展后端目前没有 visibility。
            format!(
                "var br = await cua.getBrowser({{id:{browser_id}}});\
                 var caps = (await br.capabilities.list()).map(function(c){{return c.id;}});\
                 var opts = {{}};\
                 if (caps.indexOf(\"visibility\") >= 0) opts.visible = false;\
                 var t = await cua.createBrowserTab({browser_id}, {url}, opts);\
                 out.tab = {{ id: t.id, url: await t.url(), title: await t.title() }};"
            )
        }
        "browser_close" => {
            let tab = serde_json::to_string(args.get("tab")?.as_str()?).ok()?;
            // 用 management 的 `tabs.remove`：按文档它「不需要认领标签」，
            // 因此能关掉上一个进程会话留下的 tab（`getTab` + `close()` 会因
            // 「already part of browser session …」直接失败）。
            format!(
                "var br = await cua.getBrowser({{id:{browser_id}}});\
                 var mgmt = await br.capabilities.get(\"management\");\
                 await mgmt.tabs.remove([Number({tab})]);\
                 out.closed = true;"
            )
        }
        "browser_state" => {
            let tab = serde_json::to_string(args.get("tab")?.as_str()?).ok()?;
            let disable = args
                .get("disable_diff")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            format!(
                "var t = await cua.getTab({tab}{browser_arg});\
                 if (t.ax) {{ out.state = await t.ax.get(\"state\", {{disableDiffing:{disable}}}); out.mode = \"ax\"; }}\
                 else {{ out.state = await t.playwright.domSnapshot(); out.mode = \"dom\"; }}\
                 out.url = await t.url(); out.title = await t.title();"
            )
        }
        "browser_click" => {
            let tab = serde_json::to_string(args.get("tab")?.as_str()?).ok()?;
            let target = if let Some(index) = args.get("element_index").and_then(Value::as_i64) {
                format!("{index}")
            } else {
                let x = args.get("x")?.as_f64()?;
                let y = args.get("y")?.as_f64()?;
                format!("[{x},{y}]")
            };
            let options = {
                let mut parts = Vec::new();
                if let Some(count) = args.get("click_count").and_then(Value::as_i64) {
                    parts.push(format!("clickCount:{count}"));
                }
                if let Some(button) = args.get("mouse_button").and_then(Value::as_str) {
                    parts.push(format!(
                        "mouseButton:{}",
                        serde_json::to_string(button).ok()?
                    ));
                }
                if parts.is_empty() {
                    String::new()
                } else {
                    format!(", {{{}}}", parts.join(","))
                }
            };
            format!(
                "var t = await cua.getTab({tab}{browser_arg});\
                 if (!t.ax) throw new Error(\"This tab does not expose accessibility input; use browser_eval or Playwright locators\");\
                 await t.ax.click({target}{options}); out.clicked = true;"
            )
        }
        "browser_type" => {
            let tab = serde_json::to_string(args.get("tab")?.as_str()?).ok()?;
            let text = serde_json::to_string(args.get("text")?.as_str()?).ok()?;
            let index = args
                .get("element_index")
                .and_then(Value::as_i64)
                .map(|value| value.to_string())
                .unwrap_or_else(|| "null".to_string());
            format!(
                "var t = await cua.getTab({tab}{browser_arg});\
                 if (!t.ax) throw new Error(\"This tab does not expose accessibility input; use browser_eval\");\
                 await t.ax.typeText({index}, {text}); out.typed = true;"
            )
        }
        "browser_key" => {
            let tab = serde_json::to_string(args.get("tab")?.as_str()?).ok()?;
            let key = serde_json::to_string(args.get("key")?.as_str()?).ok()?;
            format!(
                "var t = await cua.getTab({tab}{browser_arg});\
                 if (!t.ax) throw new Error(\"This tab does not expose accessibility input; use browser_eval\");\
                 await t.ax.pressKey(null, {key}); out.pressed = true;"
            )
        }
        "browser_eval" => {
            let tab = serde_json::to_string(args.get("tab")?.as_str()?).ok()?;
            // 页面上下文里没有 `eval`（Playwright 的沙箱把它拿掉了），
            // 直接把表达式源码交给 evaluate 求值——它自己会包一层函数。
            let expression = serde_json::to_string(args.get("expression")?.as_str()?).ok()?;
            format!(
                "var t = await cua.getTab({tab}{browser_arg});\
                 out.value = await t.playwright.evaluate({expression});"
            )
        }
        "browser_screenshot" => {
            let tab = serde_json::to_string(args.get("tab")?.as_str()?).ok()?;
            let full = args
                .get("full_page")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            format!(
                "var t = await cua.getTab({tab}{browser_arg});\
                 var bytes = await t.screenshot({{fullPage:{full}}});\
                 out.image = Buffer.from(bytes).toString(\"base64\");"
            )
        }
        "browser_group" => {
            let tabs = args.get("tabs")?.as_array()?;
            let ids: Vec<String> = tabs
                .iter()
                .filter_map(Value::as_i64)
                .map(|value| value.to_string())
                .collect();
            if ids.is_empty() {
                return None;
            }
            let mut update = Vec::new();
            if let Some(title) = args.get("title").and_then(Value::as_str) {
                update.push(format!("title:{}", serde_json::to_string(title).ok()?));
            }
            if let Some(color) = args.get("color").and_then(Value::as_str) {
                update.push(format!("color:{}", serde_json::to_string(color).ok()?));
            }
            if let Some(collapsed) = args.get("collapsed").and_then(Value::as_bool) {
                update.push(format!("collapsed:{collapsed}"));
            }
            let update_js = if update.is_empty() {
                String::new()
            } else {
                format!(
                    "await mgmt.tabGroups.update(out.group.id, {{{}}});",
                    update.join(",")
                )
            };
            // 实测：`tabs.group({tabIds})` 直接可用，扩展会按 tab 所在窗口自动建组；
            // 额外传 `createProperties.windowId` 反而报
            // 「Chrome API requires an explicit valid group ID」。
            format!(
                "var br = await cua.getBrowser({{id:{browser_id}}});\
                 var mgmt = await br.capabilities.get(\"management\");\
                 var groupId = await mgmt.tabs.group({{tabIds:[{ids}]}});\
                 out.group = {{ id: groupId }};\
                 {update_js}",
                ids = ids.join(",")
            )
        }
        "browser_group_update" => {
            let group = args.get("group")?.as_i64()?;
            let mut update = Vec::new();
            if let Some(title) = args.get("title").and_then(Value::as_str) {
                update.push(format!("title:{}", serde_json::to_string(title).ok()?));
            }
            if let Some(color) = args.get("color").and_then(Value::as_str) {
                update.push(format!("color:{}", serde_json::to_string(color).ok()?));
            }
            if let Some(collapsed) = args.get("collapsed").and_then(Value::as_bool) {
                update.push(format!("collapsed:{collapsed}"));
            }
            if update.is_empty() {
                return None;
            }
            format!(
                "var br = await cua.getBrowser({{id:{browser_id}}});\
                 var mgmt = await br.capabilities.get(\"management\");\
                 await mgmt.tabGroups.update({group}, {{{}}});\
                 out.updated = true;",
                update.join(",")
            )
        }
        _ => return None,
    };

    let code = format!(
        "var out = {{}};\
         try {{ {body} out.ok = true; }}\
         catch (e) {{ out.ok = false; out.error = String((e && e.message) || e); }}\
         nodeRepl.write('{PAYLOAD_OPEN}' + JSON.stringify(out) + '{PAYLOAD_CLOSE}');"
    );
    Some((tool, code))
}

/// 把 `js` 的结果还原成 MCP 的 content。
fn reshape_result(tool: &str, message: Value) -> Value {
    let id = message.get("id").cloned().unwrap_or(Value::Null);
    if let Some(error) = message.get("error") {
        return error_result(id, error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("浏览器操作失败"));
    }
    let text = message
        .get("result")
        .and_then(|result| result.get("content"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    let Some(payload) = extract_payload(&text) else {
        // 没有信封说明 REPL 侧报错（例如内核重置），原样带回。
        let clipped: String = text.chars().take(2000).collect();
        return error_result(id, &if clipped.trim().is_empty() {
            "浏览器调用没有返回结果".to_string()
        } else {
            clipped
        });
    };
    if payload.get("ok").and_then(Value::as_bool) != Some(true) {
        let detail = payload
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("浏览器操作失败");
        return error_result(id, detail);
    }
    let mut content = Vec::new();
    if tool == "browser_screenshot" {
        if let Some(image) = payload.get("image").and_then(Value::as_str) {
            content.push(json!({ "type": "image", "data": image, "mimeType": "image/jpeg" }));
        }
    }
    let summary = if tool == "browser_screenshot" {
        json!("ok")
    } else {
        payload.clone()
    };
    content.push(json!({ "type": "text", "text": render_payload(&summary) }));
    json!({ "jsonrpc": "2.0", "id": id, "result": { "content": content } })
}

fn error_result(id: Value, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": { "content": [{ "type": "text", "text": message }], "isError": true }
    })
}

fn extract_payload(text: &str) -> Option<Value> {
    let start = text.find(PAYLOAD_OPEN)? + PAYLOAD_OPEN.len();
    let rest = &text[start..];
    let end = rest.find(PAYLOAD_CLOSE)?;
    serde_json::from_str(rest[..end].trim()).ok()
}

/// 把 JSON 结果压成模型好读的文本：能一行一个时就不输出一大坨。
fn render_payload(payload: &Value) -> String {
    let mut object = payload.as_object().cloned().unwrap_or_default();
    object.remove("ok");
    if let Some(tabs) = object.get("tabs").and_then(Value::as_array) {
        let lines: Vec<String> = tabs
            .iter()
            .map(|tab| {
                format!(
                    "[{}] {} — {} ({})",
                    tab.get("id").and_then(Value::as_str).unwrap_or("?"),
                    tab.get("title").and_then(Value::as_str).unwrap_or(""),
                    tab.get("url").and_then(Value::as_str).unwrap_or(""),
                    tab.get("browserId").and_then(Value::as_str).unwrap_or("")
                )
            })
            .collect();
        return lines.join("\n");
    }
    if let Some(browsers) = object.get("browsers").and_then(Value::as_array) {
        let lines: Vec<String> = browsers
            .iter()
            .map(|browser| {
                format!(
                    "[{}] {} ({}, {} tabs)",
                    browser.get("id").and_then(Value::as_str).unwrap_or("?"),
                    browser.get("name").and_then(Value::as_str).unwrap_or(""),
                    browser
                        .get("family")
                        .or_else(|| browser.get("type"))
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                    browser
                        .get("tabs")
                        .and_then(Value::as_array)
                        .map(Vec::len)
                        .unwrap_or(0)
                )
            })
            .collect();
        if !lines.is_empty() {
            return lines.join("\n");
        }
        return "没有可用的浏览器。请确认 Chrome 正在运行且已启用 Codex 浏览器扩展。".into();
    }
    serde_json::to_string_pretty(&object).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(tool: &str, args: Value) -> Option<(String, String)> {
        build_js_call(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": tool, "arguments": args }
        }))
    }

    #[test]
    fn builds_list_and_tabs_calls() {
        let (_, code) = call("browser_list", json!({})).unwrap();
        assert!(code.contains("cua.listBrowsers"));
        let (_, code) = call("browser_tabs", json!({ "browser": "chrome" })).unwrap();
        assert!(code.contains("cua.listTabs"));
        assert!(code.contains("browser:\"chrome\""));
    }

    #[test]
    fn click_requires_index_or_coordinates() {
        assert!(call("browser_click", json!({ "tab": "7" })).is_none());
        let (_, code) = call("browser_click", json!({ "tab": "7", "element_index": 42 })).unwrap();
        assert!(code.contains("t.ax.click(42"));
        let (_, code) = call("browser_click", json!({ "tab": "7", "x": 10, "y": 20 })).unwrap();
        assert!(code.contains("t.ax.click([10,20]"));
    }

    #[test]
    fn group_call_uses_management_capability() {
        let (_, code) = call(
            "browser_group",
            json!({ "tabs": [1, 2], "title": "🔎 Task", "color": "blue" }),
        )
        .unwrap();
        assert!(code.contains("capabilities.get(\"management\")"));
        assert!(code.contains("mgmt.tabs.group({tabIds:[1,2]})"));
        assert!(code.contains("tabGroups.update"));
    }

    #[test]
    fn turn_metadata_is_injected_into_every_call() {
        let mut message = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {} });
        let mut seq = 0;
        inject_turn_metadata(&mut message, "s1", &mut seq);
        let meta = &message["params"]["_meta"]["x-codex-turn-metadata"];
        assert_eq!(meta["session_id"], "s1");
        assert_eq!(meta["turn_id"], "s1-1");
    }

    #[test]
    fn payload_round_trips_through_envelope() {
        let text = format!("{PAYLOAD_OPEN}{{\"ok\":true,\"tabs\":[]}}{PAYLOAD_CLOSE}");
        let payload = extract_payload(&text).unwrap();
        assert_eq!(payload["ok"], true);
    }

    #[test]
    fn renders_tabs_one_per_line() {
        let payload = json!({
            "ok": true,
            "tabs": [{ "id": "3", "title": "Example", "url": "https://example.com", "browserId": "chrome" }]
        });
        assert_eq!(
            render_payload(&payload),
            "[3] Example — https://example.com (chrome)"
        );
    }

    #[test]
    fn empty_browser_list_explains_what_to_check() {
        let payload = json!({ "ok": true, "browsers": [] });
        assert!(render_payload(&payload).contains("Chrome"));
    }
}
