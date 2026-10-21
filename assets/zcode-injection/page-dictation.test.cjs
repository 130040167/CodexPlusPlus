/**
 * assets/zcode-injection/page-dictation.js 的行为测试。
 *
 * 用 node:vm 跑页面脚本，配一个够用的迷你 DOM（不引入 jsdom）：
 * - 选择器只支持 [attr] / [attr="value"]，脚本里的选择器都在这个子集内；
 * - MutationObserver / rAF / setTimeout 都是可手动驱动的假实现，避免真等待；
 * - fetch 用假实现按路径返回，方便断言 URL、method、credentials、无自定义 header。
 *
 * 运行：node --test assets/zcode-injection/page-dictation.test.cjs
 */

"use strict";

const assert = require("node:assert/strict");
const { readFileSync } = require("node:fs");
const path = require("node:path");
const test = require("node:test");
const vm = require("node:vm");

const SOURCE = readFileSync(path.join(__dirname, "page-dictation.js"), "utf8");
const flush = () => new Promise((resolve) => setImmediate(resolve));
const settle = async () => {
  await flush();
  await flush();
};

// ---------------------------------------------------------------------------
// 迷你 DOM
// ---------------------------------------------------------------------------

let elementSeq = 0;

function parseSelectorPart(part) {
  const match = /^\[([^=\]]+)(?:="([^"]*)")?\]$/.exec(part);
  if (!match) throw new Error(`测试用的山寨选择器只支持 [attr] / [attr="value"]，收到：${part}`);
  return { name: match[1], value: match[2] };
}

function matches(element, selector) {
  if (!element || element.nodeType !== 1) return false;
  const parts = String(selector).split(",");
  for (const raw of parts) {
    const part = raw.trim();
    if (!part) continue;
    const { name, value } = parseSelectorPart(part);
    if (value === undefined) {
      if (element.hasAttribute(name)) return true;
    } else if (element.getAttribute(name) === value) {
      return true;
    }
  }
  return false;
}

class FakeElement {
  constructor(tagName, ownerDocument) {
    this.tagName = String(tagName).toUpperCase();
    this.nodeType = 1;
    this.ownerDocument = ownerDocument || null;
    this.children = [];
    this.parentNode = null;
    this.attributes = new Map();
    this.style = {};
    this.listeners = new Map();
    this.disabled = false;
    this._textContent = "";
    this._seq = ++elementSeq;
  }

  get parentElement() {
    return this.parentNode && this.parentNode.nodeType === 1 ? this.parentNode : null;
  }

  get firstChild() {
    return this.children.length ? this.children[0] : null;
  }

  get isConnected() {
    let node = this;
    while (node.parentNode) node = node.parentNode;
    return !!(this.ownerDocument && node === this.ownerDocument.documentElement);
  }

  get id() {
    const value = this.getAttribute("id");
    return value === null ? "" : value;
  }

  set id(value) {
    this.setAttribute("id", String(value));
  }

  get className() {
    const value = this.getAttribute("class");
    return value === null ? "" : value;
  }

  set className(value) {
    this.setAttribute("class", String(value));
  }

  get textContent() {
    if (this._textContent) return this._textContent;
    return this.children.map((child) => (child && child.textContent) || "").join("");
  }

  set textContent(value) {
    this._textContent = String(value);
    for (const child of this.children) child.parentNode = null;
    this.children = [];
  }

  getAttribute(name) {
    return this.attributes.has(name) ? this.attributes.get(name) : null;
  }

  setAttribute(name, value) {
    this.attributes.set(name, String(value));
  }

  hasAttribute(name) {
    return this.attributes.has(name);
  }

  removeAttribute(name) {
    this.attributes.delete(name);
  }

  appendChild(node) {
    if (!node) throw new Error("appendChild(null)");
    if (node.parentNode) node.parentNode.removeChild(node);
    node.parentNode = this;
    this.children.push(node);
    return node;
  }

  insertBefore(node, reference) {
    if (!node) throw new Error("insertBefore(null)");
    if (node.parentNode) node.parentNode.removeChild(node);
    const index = reference ? this.children.indexOf(reference) : -1;
    if (index < 0) this.children.push(node);
    else this.children.splice(index, 0, node);
    node.parentNode = this;
    return node;
  }

  removeChild(node) {
    const index = this.children.indexOf(node);
    if (index >= 0) this.children.splice(index, 1);
    node.parentNode = null;
    return node;
  }

  replaceChildren() {
    const nodes = Array.prototype.slice.call(arguments);
    for (const child of this.children) child.parentNode = null;
    this.children = [];
    for (const node of nodes) this.appendChild(node);
  }

  append() {
    const nodes = Array.prototype.slice.call(arguments);
    for (const node of nodes) this.appendChild(node);
  }

  remove() {
    if (this.parentNode) this.parentNode.removeChild(this);
  }

  contains(node) {
    let current = node;
    while (current) {
      if (current === this) return true;
      current = current.parentNode;
    }
    return false;
  }

  querySelectorAll(selector) {
    const found = [];
    const walk = (element) => {
      for (const child of element.children) {
        if (matches(child, selector)) found.push(child);
        walk(child);
      }
    };
    walk(this);
    return found;
  }

  querySelector(selector) {
    return this.querySelectorAll(selector)[0] || null;
  }

  matches(selector) {
    return matches(this, selector);
  }

  closest(selector) {
    let current = this;
    while (current && current.nodeType === 1) {
      if (matches(current, selector)) return current;
      current = current.parentNode;
    }
    return null;
  }

  addEventListener(type, listener) {
    if (!this.listeners.has(type)) this.listeners.set(type, []);
    this.listeners.get(type).push(listener);
  }

  removeEventListener(type, listener) {
    const list = this.listeners.get(type);
    if (!list) return;
    const index = list.indexOf(listener);
    if (index >= 0) list.splice(index, 1);
  }

  dispatchEvent(event) {
    const fired = event || { type: "" };
    fired.target = fired.target || this;
    fired.currentTarget = this;
    if (typeof fired.preventDefault !== "function") {
      fired.defaultPrevented = false;
      fired.preventDefault = () => {
        fired.defaultPrevented = true;
      };
    }
    if (typeof fired.stopPropagation !== "function") fired.stopPropagation = () => {};
    const list = (this.listeners.get(fired.type) || []).slice();
    for (const listener of list) listener.call(this, fired);
    return !fired.defaultPrevented;
  }

  focus() {
    if (this.ownerDocument) this.ownerDocument.activeElement = this;
  }

  getBoundingClientRect() {
    return { x: 0, y: 0, top: 0, left: 0, right: 120, bottom: 32, width: 120, height: 32 };
  }
}

class FakeDocument {
  constructor() {
    this.nodeType = 9;
    this.documentElement = new FakeElement("html", this);
    this.head = new FakeElement("head", this);
    this.body = new FakeElement("body", this);
    this.documentElement.appendChild(this.head);
    this.documentElement.appendChild(this.body);
    this.activeElement = this.body;
    this.readyState = "complete";
    this.listeners = new Map();
    this.execCommandCalls = [];
    this.execCommandResult = true;
  }

  createElement(tagName) {
    return new FakeElement(tagName, this);
  }

  createElementNS(_namespace, tagName) {
    return new FakeElement(tagName, this);
  }

  createTextNode(text) {
    return { nodeType: 3, textContent: String(text), parentNode: null };
  }

  querySelectorAll(selector) {
    const found = [];
    const walk = (element) => {
      for (const child of element.children) {
        if (matches(child, selector)) found.push(child);
        walk(child);
      }
    };
    walk(this.documentElement);
    return found;
  }

  querySelector(selector) {
    return this.querySelectorAll(selector)[0] || null;
  }

  getElementById(id) {
    const value = String(id);
    const walk = (element) => {
      for (const child of element.children) {
        if (child.getAttribute("id") === value) return child;
        const found = walk(child);
        if (found) return found;
      }
      return null;
    };
    return walk(this.documentElement);
  }

  addEventListener(type, listener) {
    if (!this.listeners.has(type)) this.listeners.set(type, []);
    this.listeners.get(type).push(listener);
  }

  removeEventListener(type, listener) {
    const list = this.listeners.get(type);
    if (!list) return;
    const index = list.indexOf(listener);
    if (index >= 0) list.splice(index, 1);
  }

  dispatchEvent(event) {
    const fired = event || { type: "" };
    fired.target = fired.target || this;
    fired.currentTarget = this;
    if (typeof fired.preventDefault !== "function") fired.preventDefault = () => {};
    if (typeof fired.stopPropagation !== "function") fired.stopPropagation = () => {};
    const list = (this.listeners.get(fired.type) || []).slice();
    for (const listener of list) listener.call(this, fired);
    return true;
  }

  execCommand(command, showUI, value) {
    this.execCommandCalls.push([command, showUI, value]);
    return this.execCommandResult;
  }
}

// ---------------------------------------------------------------------------
// 假时钟 / rAF / MutationObserver
// ---------------------------------------------------------------------------

function createClock() {
  let now = 0;
  let seq = 0;
  const timers = new Map();

  function setTimeoutFake(fn, delay) {
    const wait = Number(delay) || 0;
    // 0ms 立即执行：让「启动补扫」在脚本加载时就排进 rAF 队列，测试无需等真时间。
    if (wait <= 0) {
      fn();
      return ++seq;
    }
    const id = ++seq;
    timers.set(id, { fn, at: now + wait, interval: 0 });
    return id;
  }

  function setIntervalFake(fn, delay) {
    const wait = Number(delay) || 16;
    const id = ++seq;
    timers.set(id, { fn, at: now + wait, interval: wait });
    return id;
  }

  function clearTimer(id) {
    timers.delete(id);
  }

  function advance(ms) {
    const target = now + (Number(ms) || 0);
    for (let guard = 0; guard < 20000; guard += 1) {
      let dueId = null;
      let dueAt = Infinity;
      timers.forEach((timer, id) => {
        if (timer.at <= target && timer.at < dueAt) {
          dueAt = timer.at;
          dueId = id;
        }
      });
      if (dueId === null) break;
      const timer = timers.get(dueId);
      now = timer.at;
      if (timer.interval > 0) timer.at = now + timer.interval;
      else timers.delete(dueId);
      timer.fn();
    }
    now = target;
  }

  return {
    setTimeout: setTimeoutFake,
    setInterval: setIntervalFake,
    clearTimeout: clearTimer,
    clearInterval: clearTimer,
    advance,
    now: () => now,
    pending: () => timers.size,
  };
}

// ---------------------------------------------------------------------------
// 测试夹具
// ---------------------------------------------------------------------------

function textNode(text) {
  return { detail: 0, format: 0, mode: "normal", style: "", text, type: "text", version: 1 };
}

const MENTION_NODE = {
  type: "prompt-mention",
  version: 1,
  mentionId: "mention-1",
  category: "file",
  text: "review.md",
  value: "review.md",
  markdown: "[@review.md](plugin://file/mention-1)",
  description: null,
  data: { id: "mention-1" },
};

function paragraphNode(children) {
  return {
    children,
    direction: null,
    format: "",
    indent: 0,
    type: "paragraph",
    version: 1,
    textFormat: 0,
    textStyle: "",
  };
}

function editorState(paragraphChildren) {
  return { root: { children: [paragraphNode(paragraphChildren)] } };
}

function flattenEditorState(state) {
  let out = "";
  const walk = (node) => {
    if (!node) return;
    if (Array.isArray(node)) {
      node.forEach(walk);
      return;
    }
    if (node.type === "text") out += node.text || "";
    else if (node.type === "prompt-mention") out += node.markdown || node.text || "";
    if (node.children) walk(node.children);
    if (node.root) walk(node.root);
  };
  walk(state);
  return out;
}

/**
 * 给输入框挂一个假的 Lexical E2E 桥。
 * mode: "json"（完整，含 JSON 追加能力）/ "setTextOnly"（只有 setText）/ "none"。
 */
function attachBridge(input, mode, state, existingText) {
  const calls = { focus: 0, setText: null, setEditorStateJson: null, setTextWithPluginMentions: null };
  let json = state || { root: { children: [] } };
  let text = typeof existingText === "string" ? existingText : flattenEditorState(json);

  const bridge = {
    calls,
    input,
    getJson: () => json,
    getText: () => text,
  };

  if (mode === "none") {
    bridge.input = input;
    return bridge;
  }

  if (mode === "jsonNoReadback") {
    input.__zcodeLexicalInputE2E = {
      focus: () => {
        calls.focus += 1;
        input.focus();
      },
      getEditorState: () => ({ toJSON: () => JSON.parse(JSON.stringify(json)) }),
      setEditorStateJson: (next) => {
        calls.setEditorStateJson = next;
        json = JSON.parse(next);
        text = flattenEditorState(json);
      },
    };
    return bridge;
  }

  if (mode === "jsonBadReadback") {
    input.__zcodeLexicalInputE2E = {
      focus: () => {
        calls.focus += 1;
        input.focus();
      },
      getEditorState: () => ({ toJSON: () => JSON.parse(JSON.stringify(json)) }),
      // 故意回读不出刚写的内容，模拟 markdown 序列化对不上。
      getText: () => "",
      setText: (next) => {
        calls.setText = next;
        text = String(next);
      },
      setEditorStateJson: (next) => {
        calls.setEditorStateJson = next;
        json = JSON.parse(next);
      },
    };
    return bridge;
  }

  if (mode === "setTextOnly") {
    input.__zcodeLexicalInputE2E = {
      focus: () => {
        calls.focus += 1;
        input.focus();
      },
      getText: () => text,
      setText: (next) => {
        calls.setText = next;
        text = String(next);
      },
    };
    return bridge;
  }

  input.__zcodeLexicalInputE2E = {
    focus: () => {
      calls.focus += 1;
      input.focus();
    },
    getEditorState: () => ({ toJSON: () => JSON.parse(JSON.stringify(json)) }),
    getText: () => text,
    setText: (next) => {
      calls.setText = next;
      text = String(next);
    },
    setEditorStateJson: (next) => {
      calls.setEditorStateJson = next;
      json = JSON.parse(next);
      text = flattenEditorState(json);
    },
    setTextWithPluginMentions: (next) => {
      calls.setTextWithPluginMentions = next;
    },
  };
  return bridge;
}

function buildPane(document, options) {
  const opts = options || {};
  const pane = document.createElement("div");
  pane.setAttribute("data-pane-id", opts.paneId || "pane-1");
  if (opts.focused !== false) pane.setAttribute("data-focused", "true");

  const composer = document.createElement("div");
  composer.setAttribute("data-testid", "v4-composer");
  const form = document.createElement("div");
  const inputRow = document.createElement("div");
  const input = document.createElement("div");
  input.setAttribute("data-testid", "v4-composer-input");
  input.setAttribute("contenteditable", "true");
  input.setAttribute("data-lexical-editor", "true");
  input.setAttribute("role", "textbox");

  const toolbar = document.createElement("div");
  toolbar.setAttribute("data-composer-trailing-actions", "");
  const send = document.createElement("button");
  send.setAttribute("data-testid", "v4-composer-send");
  toolbar.appendChild(send);

  form.appendChild(inputRow);
  inputRow.appendChild(input);
  if (!opts.noToolbar) form.appendChild(toolbar);
  composer.appendChild(form);
  pane.appendChild(composer);
  document.body.appendChild(pane);

  const bridge = attachBridge(input, opts.bridge || "json", opts.bridgeState, opts.existingText);
  return { pane, composer, form, input, inputRow, toolbar, send, bridge };
}

function createHarness() {
  const document = new FakeDocument();
  const clock = createClock();
  const frames = [];
  const observers = [];
  const fetchCalls = [];
  const responses = new Map();
  const winListeners = new Map();

  const defaultResponses = {
    "/zcode-voice/start": { payload: { status: "ok" } },
    "/zcode-voice/stop": { payload: { status: "ok", text: "默认识别文本" } },
    "/zcode-voice/cancel": { payload: { status: "ok" } },
  };

  function pathOf(url) {
    const match = /\/zcode-voice\/([a-z]+)/.exec(String(url));
    return match ? `/zcode-voice/${match[1]}` : null;
  }

  function makeResponse(spec) {
    const value = spec || {};
    return {
      ok: value.ok !== false,
      status: value.status || 200,
      json: () => Promise.resolve(value.payload === undefined ? { status: "ok" } : value.payload),
    };
  }

  function fetchFake(url, options) {
    fetchCalls.push({ url: String(url), options: options || {} });
    const key = pathOf(url);
    let spec = responses.has(key) ? responses.get(key) : defaultResponses[key];
    if (!spec) spec = { payload: { status: "ok" } };
    if (spec && spec.deferred) return spec.promise;
    if (spec && spec.reject) return Promise.reject(new TypeError("Failed to fetch"));
    return Promise.resolve(makeResponse(spec));
  }

  const window = {
    location: {
      href: "file:///Applications/ZCode.app/Contents/Resources/app.asar/out/renderer/index.html",
      protocol: "file:",
      hostname: "",
    },
    listeners: winListeners,
    fetch: fetchFake,
    getComputedStyle: () => ({ position: "static" }),
    addEventListener(type, listener) {
      if (!winListeners.has(type)) winListeners.set(type, []);
      winListeners.get(type).push(listener);
    },
    removeEventListener(type, listener) {
      const list = winListeners.get(type);
      if (!list) return;
      const index = list.indexOf(listener);
      if (index >= 0) list.splice(index, 1);
    },
    dispatchEvent(event) {
      const list = (winListeners.get(event && event.type) || []).slice();
      for (const listener of list) listener.call(window, event);
    },
  };
  window.__zcodePlusVoiceConfig = null;
  window.document = document;
  window.setTimeout = clock.setTimeout;
  window.clearTimeout = clock.clearTimeout;
  window.setInterval = clock.setInterval;
  window.clearInterval = clock.clearInterval;
  window.requestAnimationFrame = (cb) => frames.push(cb);

  class FakeMutationObserver {
    constructor(callback) {
      this.callback = callback;
      this.observing = false;
      this.disconnected = false;
      observers.push(this);
    }

    observe(target, options) {
      this.target = target;
      this.options = options || {};
      this.observing = true;
    }

    disconnect() {
      this.observing = false;
      this.disconnected = true;
    }
  }

  const context = vm.createContext({
    window,
    document,
    MutationObserver: FakeMutationObserver,
    AbortController,
    requestAnimationFrame: (cb) => frames.push(cb),
    cancelAnimationFrame: () => {},
    setTimeout: clock.setTimeout,
    clearTimeout: clock.clearTimeout,
    setInterval: clock.setInterval,
    clearInterval: clock.clearInterval,
    console,
  });

  function install() {
    return vm.runInContext(SOURCE, context);
  }

  function flushFrames() {
    for (let guard = 0; guard < 1000 && frames.length; guard += 1) {
      const queued = frames.splice(0, frames.length);
      for (const callback of queued) callback(clock.now());
    }
  }

  function fireMutations(records) {
    for (const observer of observers) {
      if (observer.observing) observer.callback(records, observer);
    }
  }

  function respond(route, spec) {
    responses.set(route, spec);
  }

  /** 让某条路由挂起，返回 { promise, resolve } 供测试控制时序。 */
  function hold(route) {
    const holder = {};
    holder.promise = new Promise((resolve) => {
      holder.resolve = (payload) => resolve(makeResponse({ payload: payload === undefined ? { status: "ok" } : payload }));
    });
    responses.set(route, { deferred: true, promise: holder.promise });
    return holder;
  }

  return {
    document,
    window,
    context,
    clock,
    observers,
    fetchCalls,
    frames,
    install,
    flushFrames,
    fireMutations,
    respond,
    hold,
  };
}

const DEFAULT_CONFIG = { endpoint: "http://127.0.0.1:53421", token: "test-token-123" };

function boot(options) {
  const opts = options || {};
  const harness = createHarness();
  const panes = opts.panes || [{}];
  const fixtures = panes.map((paneOptions) => buildPane(harness.document, paneOptions));
  if (opts.config !== null) {
    harness.window.__zcodePlusVoiceConfig = opts.config === undefined ? DEFAULT_CONFIG : opts.config;
  }
  const result = harness.install();
  harness.flushFrames();
  return { harness, fixtures, pane: fixtures[0], result };
}

function buttons(harness) {
  return harness.document.querySelectorAll('[data-zcode-plus-voice="true"]');
}

function toastNodes(harness) {
  return harness.document.querySelectorAll('[data-zcode-plus-voice="toast"]');
}

function click(node) {
  node.dispatchEvent({ type: "click" });
}

// ---------------------------------------------------------------------------
// 测试：安装 / 挂载
// ---------------------------------------------------------------------------

test("安装幂等：重复注入只保留一份按钮与样式，并暴露 insertVoiceText/dispose", () => {
  const { harness, pane, result } = boot({ panes: [{}] });

  assert.equal(result, "ok");
  assert.equal(harness.window.__zcodePlusVoiceInstalled, true);
  assert.equal(buttons(harness).length, 1);
  assert.ok(harness.document.getElementById("zcode-plus-voice-style"));
  assert.equal(typeof harness.window.insertVoiceText, "function");
  assert.equal(typeof harness.window.__zcodePlusVoiceDispose, "function");

  // 第二次注入（管理端补注入 / 页面 reload 前重复 evaluate）必须直接返回。
  const second = harness.install();
  harness.flushFrames();
  assert.equal(second, "ok");
  assert.equal(buttons(harness).length, 1);
  assert.equal(harness.document.querySelectorAll('[id="zcode-plus-voice-style"]').length, 1);
  assert.ok(pane.toolbar.contains(buttons(harness)[0]));
});

test("按钮挂在 pane 的 trailing-actions 里、位于发送键之前，宿主重建后自动重挂且不重复", () => {
  const { harness, pane } = boot({ panes: [{}] });

  assert.equal(buttons(harness).length, 1);
  const button = buttons(harness)[0];
  assert.equal(button.parentNode, pane.toolbar);
  assert.equal(pane.toolbar.children[0], button, "按钮插在发送键之前");
  assert.equal(button.getAttribute("aria-label"), "语音输入");
  assert.equal(button.getAttribute("title"), "语音输入");
  assert.equal(button.getAttribute("data-phase"), "idle");
  assert.equal(button.children.length, 1, "按钮内只有一个 inline SVG 图标");

  // 模拟宿主 React 重建工具条：旧工具条连按钮一起被换掉。
  const oldToolbar = pane.toolbar;
  const newToolbar = harness.document.createElement("div");
  newToolbar.setAttribute("data-composer-trailing-actions", "");
  const newSend = harness.document.createElement("button");
  newSend.setAttribute("data-testid", "v4-composer-send");
  newToolbar.appendChild(newSend);
  pane.form.removeChild(oldToolbar);
  pane.form.appendChild(newToolbar);
  pane.toolbar = newToolbar;

  harness.fireMutations([
    { type: "childList", target: pane.form, addedNodes: [newToolbar], removedNodes: [oldToolbar] },
  ]);
  harness.flushFrames();

  assert.equal(buttons(harness).length, 1, "宿主重建后仍只有一个按钮");
  assert.equal(newToolbar.querySelectorAll('[data-zcode-plus-voice="true"]').length, 1);
  assert.equal(newToolbar.children[0], buttons(harness)[0]);

  // 再触发一次普通宿主变更（多挂一个无关节点），不应产生重复按钮。
  const extra = harness.document.createElement("span");
  pane.form.appendChild(extra);
  harness.fireMutations([{ type: "childList", target: pane.form, addedNodes: [extra], removedNodes: [] }]);
  harness.flushFrames();
  assert.equal(buttons(harness).length, 1);
});

test("自身节点引起的 mutation 不触发重扫（避免自喂循环）", () => {
  const { harness, pane } = boot({ panes: [{}] });
  assert.equal(harness.frames.length, 0);

  const button = buttons(harness)[0];
  // 自己把按钮插进工具条、自己换图标，这些都是「增删节点全是自身节点」的记录。
  harness.fireMutations([{ type: "childList", target: pane.toolbar, addedNodes: [button], removedNodes: [] }]);
  harness.fireMutations([{ type: "childList", target: button, addedNodes: [button.children[0]], removedNodes: [] }]);
  assert.equal(harness.frames.length, 0, "自身节点变更不应排队重扫");
});

test("找不到工具条时退化成 composer 内浮动按钮；工具条回来后自动归位", () => {
  const { harness, pane } = boot({ panes: [{ noToolbar: true }] });

  assert.equal(buttons(harness).length, 1);
  const floating = buttons(harness)[0];
  assert.equal(floating.parentNode, pane.composer);
  assert.ok(floating.className.indexOf("zcode-plus-voice-floating") >= 0);
  assert.equal(pane.composer.style.position, "relative", "浮动按钮需要定位参照系");

  // 宿主之后补齐工具条。
  pane.form.appendChild(pane.toolbar);
  harness.fireMutations([{ type: "childList", target: pane.form, addedNodes: [pane.toolbar], removedNodes: [] }]);
  harness.flushFrames();

  assert.equal(buttons(harness).length, 1);
  assert.equal(buttons(harness)[0].parentNode, pane.toolbar);
  assert.equal(buttons(harness)[0].className, "zcode-plus-voice-button", "归位后去掉浮动样式");
});

// ---------------------------------------------------------------------------
// 测试：本机服务调用
// ---------------------------------------------------------------------------

test("点击 idle->start、recording->stop：URL/token/POST/credentials 正确且无自定义 header", async () => {
  const { harness, pane } = boot({
    config: { endpoint: "http://127.0.0.1:53421/", token: "tok/en 1" },
    panes: [{ existingText: "已有草稿", bridgeState: editorState([textNode("已有草稿")]) }],
  });
  const button = pane.toolbar.querySelectorAll('[data-zcode-plus-voice="true"]')[0];

  click(button);
  await settle();

  assert.equal(harness.fetchCalls.length, 1);
  const start = harness.fetchCalls[0];
  assert.equal(start.url, "http://127.0.0.1:53421/zcode-voice/start?token=tok%2Fen%201");
  assert.equal(start.options.method, "POST");
  assert.equal(start.options.credentials, "omit");
  assert.equal(start.options.headers, undefined, "不允许自定义 header（避免预检）");
  assert.equal(start.options.body, undefined);
  assert.equal(button.getAttribute("data-phase"), "recording");
  assert.equal(button.getAttribute("aria-label"), "停止录音");

  harness.respond("/zcode-voice/stop", { payload: { status: "ok", text: " 你好世界 " } });
  click(button);
  await settle();

  assert.equal(harness.fetchCalls.length, 2);
  const stop = harness.fetchCalls[1];
  assert.equal(stop.url, "http://127.0.0.1:53421/zcode-voice/stop?token=tok%2Fen%201");
  assert.equal(stop.options.method, "POST");
  assert.equal(stop.options.credentials, "omit");
  assert.equal(stop.options.headers, undefined);
  assert.equal(stop.options.body, undefined);
  assert.equal(button.getAttribute("data-phase"), "idle", "收尾后回到 idle");

  // 优先走 JSON 追加，而不是整段 setText。
  assert.ok(pane.bridge.calls.setEditorStateJson);
  assert.equal(pane.bridge.calls.setText, null, "桥同时具备两种能力时不走 setText");
  assert.ok(pane.bridge.calls.focus >= 1);
  const parsed = JSON.parse(pane.bridge.calls.setEditorStateJson);
  const children = parsed.root.children[0].children;
  assert.equal(children[0].text, "已有草稿", "原有内容保留");
  assert.equal(children[children.length - 1].text.trim(), "你好世界");
  assert.ok(pane.bridge.getText().indexOf("你好世界") >= 0, "写后能通过 getText 回读");
});

test("JSON 追加保留已有 @mention 节点，只在末尾追加文本节点", () => {
  const state = editorState([textNode("请总结"), JSON.parse(JSON.stringify(MENTION_NODE))]);
  const { harness, pane } = boot({
    panes: [{ bridgeState: state, existingText: flattenEditorState(state) }],
  });

  const ok = harness.window.insertVoiceText("这段语音");
  assert.equal(ok, true);

  const parsed = JSON.parse(pane.bridge.calls.setEditorStateJson);
  const children = parsed.root.children[0].children;
  assert.equal(children[0].text, "请总结");
  assert.deepEqual(children[1], MENTION_NODE, "mention 节点必须原样保留（深比较）");
  assert.equal(children[2].type, "text");
  assert.equal(children[2].text.trim(), "这段语音");
  assert.deepEqual(children[2], {
    detail: 0,
    format: 0,
    mode: "normal",
    style: "",
    text: children[2].text,
    type: "text",
    version: 1,
  });
  assert.ok(pane.bridge.calls.focus >= 1);
});

test("空输入框时新建 paragraph，字段符合 Lexical JSON 序列化格式", () => {
  const { harness, pane } = boot({
    panes: [{ bridgeState: { root: { children: [] } }, existingText: "" }],
  });

  const ok = harness.window.insertVoiceText("第一段语音");
  assert.equal(ok, true);

  const parsed = JSON.parse(pane.bridge.calls.setEditorStateJson);
  assert.equal(parsed.root.children.length, 1);
  const paragraph = parsed.root.children[0];
  assert.deepEqual(paragraph, {
    children: [{ detail: 0, format: 0, mode: "normal", style: "", text: "第一段语音", type: "text", version: 1 }],
    direction: null,
    format: "",
    indent: 0,
    type: "paragraph",
    version: 1,
    textFormat: 0,
    textStyle: "",
  });
  assert.ok(pane.bridge.getText().indexOf("第一段语音") >= 0);
});

test("JSON 写后回读不一致时不再叠加 setText（避免重复写入）", () => {
  const { harness, pane } = boot({ panes: [{ bridge: "jsonBadReadback" }] });
  const ok = harness.window.insertVoiceText("写不进去");
  assert.equal(ok, false, "回读确认不了时如实返回失败");
  assert.ok(pane.bridge.calls.setEditorStateJson, "JSON 方案已经执行");
  assert.equal(pane.bridge.calls.setText, null, "已落盘就不再走 setText，避免重复");
  assert.equal(harness.document.execCommandCalls.length, 0, "也不会再走 execCommand");
});

test("桥没有 getText 时无法回读，JSON 写入视为成功", () => {
  const { harness, pane } = boot({ panes: [{ bridge: "jsonNoReadback" }] });
  const ok = harness.window.insertVoiceText("无回读");
  assert.equal(ok, true);
  assert.ok(pane.bridge.calls.setEditorStateJson);
  assert.equal(pane.bridge.calls.setText, null);
});

test("桥只有 setText 时退化为整段 setText", () => {
  const { harness, pane } = boot({
    panes: [{ bridge: "setTextOnly", existingText: "existing text" }],
  });

  const ok = harness.window.insertVoiceText("补写");
  assert.equal(ok, true);
  assert.equal(pane.bridge.calls.setEditorStateJson, null);
  assert.equal(pane.bridge.calls.setText, "existing text补写");
  assert.ok(pane.bridge.calls.focus >= 1);
});

test("完全没有桥时退化为 focus + execCommand('insertText')", () => {
  const { harness, pane } = boot({ panes: [{ bridge: "none" }] });
  harness.document.execCommandResult = true;

  const ok = harness.window.insertVoiceText("兜底文本");
  assert.equal(ok, true);
  assert.deepEqual(harness.document.execCommandCalls[0], ["insertText", false, "兜底文本"]);
  assert.equal(harness.document.activeElement, pane.input, "插入前先聚焦输入框");

  harness.document.execCommandResult = false;
  assert.equal(harness.window.insertVoiceText("再来一次"), false, "execCommand 返回 false 时如实返回失败");
});

// ---------------------------------------------------------------------------
// 测试：错误处理与取消
// ---------------------------------------------------------------------------

test("缺少 __zcodePlusVoiceConfig：按钮仍可用，点击提示未就绪且不发请求", async () => {
  const { harness, pane } = boot({ config: null, panes: [{}] });
  const button = pane.toolbar.querySelectorAll('[data-zcode-plus-voice="true"]')[0];
  assert.ok(button, "配置缺失时按钮也要挂上");

  click(button);
  await settle();

  assert.equal(harness.fetchCalls.length, 0, "没有配置不能发请求");
  assert.equal(button.getAttribute("data-phase"), "idle");
  const toasts = toastNodes(harness);
  assert.equal(toasts.length, 1);
  assert.ok(toasts[0].textContent.indexOf("语音服务未就绪") >= 0);

  // toast 2~4 秒自动消失。
  harness.clock.advance(3200);
  assert.equal(toastNodes(harness).length, 0, "toast 自动消失");
});

test("start 返回 error：toast 展示服务端 message 并复位", async () => {
  const { harness, pane } = boot({ panes: [{}] });
  harness.respond("/zcode-voice/start", { payload: { status: "error", message: "麦克风被占用" } });
  const button = pane.toolbar.querySelectorAll('[data-zcode-plus-voice="true"]')[0];

  click(button);
  await settle();

  assert.equal(button.getAttribute("data-phase"), "idle");
  assert.equal(harness.fetchCalls.length, 1);
  const toasts = toastNodes(harness);
  assert.equal(toasts.length, 1);
  assert.ok(toasts[0].textContent.indexOf("麦克风被占用") >= 0);
});

test("HTTP 非 2xx 时给友好错误并复位", async () => {
  const { harness, pane } = boot({ panes: [{}] });
  harness.respond("/zcode-voice/start", { ok: false, status: 500 });
  const button = pane.toolbar.querySelectorAll('[data-zcode-plus-voice="true"]')[0];

  click(button);
  await settle();

  assert.equal(button.getAttribute("data-phase"), "idle");
  assert.equal(harness.fetchCalls.length, 1);
  assert.equal(toastNodes(harness).length, 1);
  assert.ok(toastNodes(harness)[0].textContent.indexOf("HTTP 500") >= 0);
});

test("stop 返回 error：显示 toast、复位，并且不写输入框", async () => {
  const { harness, pane } = boot({ panes: [{}] });
  const button = pane.toolbar.querySelectorAll('[data-zcode-plus-voice="true"]')[0];

  click(button);
  await settle();
  assert.equal(button.getAttribute("data-phase"), "recording");

  harness.respond("/zcode-voice/stop", { payload: { status: "error", message: "识别失败" } });
  click(button);
  await settle();

  assert.equal(button.getAttribute("data-phase"), "idle");
  assert.equal(pane.bridge.calls.setEditorStateJson, null);
  assert.equal(toastNodes(harness).length, 1);
  assert.ok(toastNodes(harness)[0].textContent.indexOf("识别失败") >= 0);
});

test("识别结果为空时提示重试并不写输入框", async () => {
  const { harness, pane } = boot({ panes: [{}] });
  const button = pane.toolbar.querySelectorAll('[data-zcode-plus-voice="true"]')[0];
  click(button);
  await settle();

  harness.respond("/zcode-voice/stop", { payload: { status: "ok", text: "   " } });
  click(button);
  await settle();

  assert.equal(button.getAttribute("data-phase"), "idle");
  assert.equal(pane.bridge.calls.setEditorStateJson, null);
  assert.ok(toastNodes(harness)[0].textContent.indexOf("没有识别到文字") >= 0);
});

test("Esc 取消录音：POST cancel、不插入文本、复位", async () => {
  const { harness, pane } = boot({ panes: [{}] });
  const button = pane.toolbar.querySelectorAll('[data-zcode-plus-voice="true"]')[0];

  click(button);
  await settle();
  assert.equal(button.getAttribute("data-phase"), "recording");

  harness.document.dispatchEvent({ type: "keydown", key: "Escape" });
  await settle();

  const cancelCall = harness.fetchCalls[harness.fetchCalls.length - 1];
  assert.ok(cancelCall.url.indexOf("/zcode-voice/cancel?token=test-token-123") >= 0);
  assert.equal(button.getAttribute("data-phase"), "idle");
  assert.equal(pane.bridge.calls.setEditorStateJson, null);
  assert.ok(toastNodes(harness)[0].textContent.indexOf("已取消录音") >= 0);
});

test("并发点击只发一个 start 请求；stopping 期间点击被忽略", async () => {
  const { harness, pane } = boot({ panes: [{}] });
  const button = pane.toolbar.querySelectorAll('[data-zcode-plus-voice="true"]')[0];
  const startHold = harness.hold("/zcode-voice/start");

  click(button);
  click(button);
  click(button);
  await flush();
  assert.equal(harness.fetchCalls.length, 1, "start 在途时重复点击必须被忽略");

  startHold.resolve({ status: "ok" });
  await settle();
  assert.equal(button.getAttribute("data-phase"), "recording");

  const stopHold = harness.hold("/zcode-voice/stop");
  click(button);
  click(button);
  await flush();
  assert.equal(harness.fetchCalls.filter((call) => call.url.indexOf("/stop") >= 0).length, 1);
  assert.equal(button.getAttribute("data-phase"), "stopping");
  assert.equal(button.disabled, true);

  stopHold.resolve({ status: "ok", text: "并发文本" });
  await settle();
  assert.equal(button.getAttribute("data-phase"), "idle");
  assert.ok(pane.bridge.getText().indexOf("并发文本") >= 0);
});

test("最长录音 5 分钟自动 stop 并把文本写回输入框", async () => {
  const { harness, pane } = boot({ panes: [{}] });
  const button = pane.toolbar.querySelectorAll('[data-zcode-plus-voice="true"]')[0];

  click(button);
  await settle();
  assert.equal(button.getAttribute("data-phase"), "recording");

  harness.clock.advance(5 * 60 * 1000);
  harness.flushFrames();
  await settle();

  assert.equal(harness.fetchCalls.filter((call) => call.url.indexOf("/stop") >= 0).length, 1);
  assert.equal(button.getAttribute("data-phase"), "idle");
  assert.ok(pane.bridge.getText().indexOf("默认识别文本") >= 0);
});

// ---------------------------------------------------------------------------
// 测试：dispose 与多 pane
// ---------------------------------------------------------------------------

test("__zcodePlusVoiceDispose 清理按钮/样式/observer/监听器并复位 flag，可重新安装", async () => {
  const { harness, pane } = boot({ panes: [{}] });
  const button = pane.toolbar.querySelectorAll('[data-zcode-plus-voice="true"]')[0];

  // 先制造一个 toast，确认连同 toast 一起清掉。
  harness.respond("/zcode-voice/start", { payload: { status: "error", message: "失败" } });
  click(button);
  await settle();
  assert.equal(toastNodes(harness).length, 1);

  const dispose = harness.window.__zcodePlusVoiceDispose;
  assert.equal(typeof dispose, "function");
  assert.equal(dispose(), "ok");

  assert.equal(buttons(harness).length, 0, "按钮已移除");
  assert.equal(toastNodes(harness).length, 0, "toast 已移除");
  assert.equal(harness.document.getElementById("zcode-plus-voice-style"), null, "样式已移除");
  assert.equal(harness.window.__zcodePlusVoiceInstalled, false, "flag 已复位");
  assert.equal(harness.window.insertVoiceText, undefined, "insertVoiceText 已移除");
  assert.equal(harness.observers[0].disconnected, true, "observer 已断开");
  assert.equal((harness.document.listeners.get("keydown") || []).length, 0, "keydown 监听器已移除");
  assert.equal((harness.window.listeners.get("resize") || []).length, 0, "resize 监听器已移除");

  // observer 断开后，宿主变化不再触发重挂。
  const extra = harness.document.createElement("div");
  pane.form.appendChild(extra);
  harness.fireMutations([{ type: "childList", target: pane.form, addedNodes: [extra], removedNodes: [] }]);
  harness.flushFrames();
  assert.equal(buttons(harness).length, 0);

  // 再次注入可以重新装上（管理端重新打开语音输入）。
  harness.install();
  harness.flushFrames();
  assert.equal(buttons(harness).length, 1);
  assert.equal(harness.window.__zcodePlusVoiceInstalled, true);
});

test("dispose 时若还在录音，会尽力 POST cancel", async () => {
  const { harness, pane } = boot({ panes: [{}] });
  const button = pane.toolbar.querySelectorAll('[data-zcode-plus-voice="true"]')[0];
  click(button);
  await settle();
  assert.equal(button.getAttribute("data-phase"), "recording");

  harness.window.__zcodePlusVoiceDispose();
  await settle();
  assert.ok(
    harness.fetchCalls.some((call) => call.url.indexOf("/zcode-voice/cancel") >= 0),
    "dispose 应通知服务端丢弃录音",
  );
});

test("dispose 时若 start 请求还在途，也会尽力 POST cancel", async () => {
  const { harness, pane } = boot({ panes: [{}] });
  const button = pane.toolbar.querySelectorAll('[data-zcode-plus-voice="true"]')[0];
  harness.hold("/zcode-voice/start");

  click(button);
  await flush();
  assert.equal(harness.fetchCalls.filter((call) => call.url.indexOf("/start") >= 0).length, 1);

  harness.window.__zcodePlusVoiceDispose();
  await settle();
  assert.ok(
    harness.fetchCalls.some((call) => call.url.indexOf("/zcode-voice/cancel") >= 0),
    "start 在途时 dispose 也要取消，避免服务端一直录音",
  );
});

test("网络层失败（服务未启动）时给中文兜底提示", async () => {
  const { harness, pane } = boot({ panes: [{}] });
  harness.respond("/zcode-voice/start", { reject: true });
  const button = pane.toolbar.querySelectorAll('[data-zcode-plus-voice="true"]')[0];

  click(button);
  await settle();

  assert.equal(button.getAttribute("data-phase"), "idle");
  const toasts = toastNodes(harness);
  assert.equal(toasts.length, 1);
  assert.ok(toasts[0].textContent.indexOf("无法连接语音服务") >= 0, "英文 Failed to fetch 要换成中文");
});

test("多 pane：每个 pane 各挂一份，focused pane 优先写入；录音状态同步到所有按钮", async () => {
  const { harness, fixtures } = boot({
    panes: [
      { paneId: "pane-a", focused: false, bridgeState: editorState([textNode("A 窗格")]) },
      { paneId: "pane-b", focused: true, bridgeState: editorState([textNode("B 窗格")]) },
    ],
  });
  const [paneA, paneB] = fixtures;

  assert.equal(buttons(harness).length, 2, "每个 pane 各一份");
  assert.equal(paneA.toolbar.querySelectorAll('[data-zcode-plus-voice="true"]').length, 1);
  assert.equal(paneB.toolbar.querySelectorAll('[data-zcode-plus-voice="true"]').length, 1);

  // 直接插入：没有指定 pane，走 focused pane（pane-b）。
  assert.equal(harness.window.insertVoiceText("写到聚焦窗格"), true);
  assert.equal(paneB.bridge.calls.setEditorStateJson == null, false);
  assert.equal(paneA.bridge.calls.setEditorStateJson, null);

  // 从 pane-a 的按钮开始录音，所有 pane 的按钮状态同步为 recording，文本写回 pane-a。
  const buttonA = paneA.toolbar.querySelectorAll('[data-zcode-plus-voice="true"]')[0];
  click(buttonA);
  await settle();
  assert.equal(buttonA.getAttribute("data-phase"), "recording");
  assert.equal(paneB.toolbar.querySelectorAll('[data-zcode-plus-voice="true"]')[0].getAttribute("data-phase"), "recording");

  paneA.bridge.calls.setEditorStateJson = null;
  paneB.bridge.calls.setEditorStateJson = null;
  click(buttonA);
  await settle();

  assert.ok(paneA.bridge.calls.setEditorStateJson, "文本应写回发起录音的 pane-a");
  assert.equal(paneB.bridge.calls.setEditorStateJson, null);
});
