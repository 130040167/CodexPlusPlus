/**
 * 旧版系统 WebKit 兜底（#2753）。
 *
 * Tauri 在 macOS 上用的是系统自带的 WKWebView，版本跟着系统走：最低支持的 macOS 12 出厂带的是 15.0 版 WebKit，
 * 不少旧 Intel Mac 一直停在 15.x。构建目标（vite.config.ts 的 build.target）只能降级语法，降不了运行时 API，
 * 所以这里给界面里实际用到、又是 WebKit 15.4 才有的几个 API 补上最小实现。
 *
 * 必须在 main.tsx 里第一个 import，保证在其它模块求值之前执行。只在缺失时补，新系统上什么也不做。
 */

type AnyRecord = Record<PropertyKey, unknown>;

function define(target: object, name: string, value: unknown) {
  Object.defineProperty(target, name, { value, configurable: true, writable: true, enumerable: false });
}

function relativeIndex(length: number, index: unknown): number {
  const integer = Math.trunc(Number(index)) || 0;
  return integer < 0 ? length + integer : integer;
}

// Object.hasOwn：WebKit 15.4+。模型目录导入 / 推理等级校验在用。
if (typeof (Object as { hasOwn?: unknown }).hasOwn !== "function") {
  define(Object, "hasOwn", function hasOwn(object: unknown, key: PropertyKey) {
    if (object === null || object === undefined) throw new TypeError("Cannot convert undefined or null to object");
    return Object.prototype.hasOwnProperty.call(Object(object), key);
  });
}

// Array.prototype.at / String.prototype.at：WebKit 15.4+。会话分组、更新记录等在用 `.at(-1)`。
if (typeof (Array.prototype as { at?: unknown }).at !== "function") {
  define(Array.prototype, "at", function at(this: ArrayLike<unknown>, index: unknown) {
    const length = this.length >>> 0;
    const position = relativeIndex(length, index);
    return position < 0 || position >= length ? undefined : this[position];
  });
}
if (typeof (String.prototype as { at?: unknown }).at !== "function") {
  define(String.prototype, "at", function at(this: string, index: unknown) {
    const text = String(this);
    const position = relativeIndex(text.length, index);
    return position < 0 || position >= text.length ? undefined : text.charAt(position);
  });
}

// <dialog>：WebKit 15.4 才默认开启。之前 `<dialog>` 只是个未知元素，没有 showModal / close / open，
// 更新提示弹窗一打开就抛 TypeError，React 会把整棵树卸掉。这里补一个够用的版本：
// 打开时加 open 属性并固定居中、用大阴影充当遮罩；Esc 触发 cancel，没被拦下就关闭；关闭时派发 close 事件。
const dialogMissing = typeof window !== "undefined"
  && typeof (window as unknown as { HTMLDialogElement?: unknown }).HTMLDialogElement === "undefined"
  && typeof HTMLUnknownElement !== "undefined";

if (dialogMissing) {
  const proto = HTMLUnknownElement.prototype as unknown as AnyRecord;
  const MODAL_ATTR = "data-codex-plus-dialog";
  const isDialog = (element: Element) => element.tagName === "DIALOG";

  Object.defineProperty(proto, "open", {
    configurable: true,
    get(this: Element) { return isDialog(this) && this.hasAttribute("open"); },
    set(this: Element, value: boolean) {
      if (!isDialog(this)) return;
      if (value) this.setAttribute("open", "");
      else this.removeAttribute("open");
    },
  });
  define(proto, "show", function show(this: Element) {
    if (isDialog(this)) this.setAttribute("open", "");
  });
  define(proto, "showModal", function showModal(this: Element) {
    if (!isDialog(this)) return;
    this.setAttribute("open", "");
    this.setAttribute(MODAL_ATTR, "modal");
  });
  define(proto, "close", function close(this: Element & AnyRecord, returnValue?: string) {
    if (!isDialog(this) || !this.hasAttribute("open")) return;
    if (returnValue !== undefined) this.returnValue = returnValue;
    this.removeAttribute("open");
    this.removeAttribute(MODAL_ATTR);
    this.dispatchEvent(new Event("close"));
  });

  document.addEventListener("keydown", (event) => {
    if (event.key !== "Escape") return;
    const dialogs = document.querySelectorAll(`dialog[${MODAL_ATTR}][open]`);
    const top = dialogs[dialogs.length - 1] as (Element & { close?: () => void }) | undefined;
    if (!top) return;
    const cancel = new Event("cancel", { cancelable: true });
    if (top.dispatchEvent(cancel)) top.close?.();
  });

  const style = document.createElement("style");
  style.setAttribute("data-codex-plus-legacy", "dialog");
  style.textContent = [
    "dialog:not([open]){display:none}",
    `dialog[${MODAL_ATTR}]{position:fixed;top:50%;left:50%;z-index:2147483000;margin:0!important;`
      + "max-height:calc(100vh - 32px);overflow:auto;transform:translate(-50%,-50%);"
      + "box-shadow:0 0 0 100vmax rgba(0,0,0,.45),0 24px 80px rgba(0,0,0,.36)}",
  ].join("");
  (document.head ?? document.documentElement).appendChild(style);
}

export {};
