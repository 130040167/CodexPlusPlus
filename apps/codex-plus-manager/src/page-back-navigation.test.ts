import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";

const source = readFileSync(new URL("../../../assets/inject/renderer-inject/40-backend-settings.js", import.meta.url), "utf8");
const functions = [
  "codexPlusNativeBackButton", "restoreCodexPlusPageBackButton",
  "syncCodexPlusPageBackButton", "installCodexPlusPageNavigationCloseHandler",
].map((name) => {
  const match = source.match(new RegExp(`^  function ${name}\\([^]*?^  \\}`, "m"));
  assert.ok(match, name);
  return match[0];
}).join("\n");

class Element {
  attributes = new Map<string, string>();
  title = "";
  button: Element | null = null;
  thread = false;
  constructor(attributes: Record<string, string> = {}) {
    Object.entries(attributes).forEach(([key, value]) => this.attributes.set(key, value));
  }
  getAttribute(name: string) { return this.attributes.get(name) ?? null; }
  setAttribute(name: string, value: string) { this.attributes.set(name, value); }
  removeAttribute(name: string) { this.attributes.delete(name); }
  closest(selector: string) {
    if (selector === "button") return this.button;
    if (selector === "[data-thread]") return this.thread ? this : null;
    return null;
  }
}

function harness(disabled = false, label = "返回") {
  let pageOpen = true;
  let closes = 0;
  let navigations = 0;
  const button = new Element({
    "aria-label": label,
    ...(disabled ? { disabled: "", "aria-disabled": "true", tabindex: "-1", "data-disabled": "" } : {}),
  });
  button.button = button;
  let current: Element | null = button;
  const listeners = new Set<Function>();
  const context = vm.createContext({
    window: {
      addEventListener: (_: string, handler: Function) => listeners.add(handler),
      removeEventListener: (_: string, handler: Function) => listeners.delete(handler),
    }, Element, codexPlusPageClass: "page",
    selectors: { appHeader: ".header, .tint", sidebarThread: "[data-thread]" },
    document: {
      querySelector: () => pageOpen ? {} : null,
      querySelectorAll: (selector: string) => {
        assert.equal(selector, ".header button, .tint button");
        return current ? [current] : [];
      },
      addEventListener: (_: string, handler: Function) => listeners.add(handler),
      removeEventListener: (_: string, handler: Function) => listeners.delete(handler),
    },
    closeCodexPlusPage: () => {
      closes++;
      pageOpen = false;
      vm.runInContext("restoreCodexPlusPageBackButton()", context);
    },
    closeCodexPlusPageAfterNativeNavigation: () => { navigations++; },
  });
  vm.runInContext(functions, context);
  const install = () => vm.runInContext("installCodexPlusPageNavigationCloseHandler()", context);
  const click = (target: Element) => {
    const event = { target, prevented: false, stopped: false,
      preventDefault() { this.prevented = true; },
      stopImmediatePropagation() { this.stopped = true; },
    };
    listeners.forEach((listener) => listener(event));
    return event;
  };
  return { button, install, click, listeners, context,
    setPage: (open: boolean) => { pageOpen = open; },
    setButton: (next: Element | null) => { current = next; },
    counts: () => ({ closes, navigations }),
  };
}

test("Back closes the page without also executing native history navigation", () => {
  const h = harness();
  h.install();
  const icon = new Element();
  icon.button = h.button;
  const event = h.click(icon);
  assert.ok(event.prevented && event.stopped);
  assert.deepEqual(h.counts(), { closes: 1, navigations: 0 });
  assert.equal(h.click(icon).prevented, false);
});

test("a disabled native Back becomes available and restores all original attributes on close", () => {
  const h = harness(true, "Back");
  const initial = [...h.button.attributes];
  h.install();
  h.install();
  assert.equal(h.listeners.size, 1);
  assert.equal(h.button.getAttribute("disabled"), null);
  assert.equal(h.button.getAttribute("aria-disabled"), null);
  assert.equal(h.button.getAttribute("tabindex"), null);
  h.click(h.button);
  assert.deepEqual([...h.button.attributes], initial);
});

test("button replacement restores old state and reacquires the new native control", () => {
  const h = harness(true);
  h.install();
  const replacement = new Element({ "aria-label": "Go back" });
  replacement.button = replacement;
  h.setButton(replacement);
  h.install();
  assert.equal(h.button.getAttribute("disabled"), "");
  assert.ok(h.click(replacement).stopped);
  assert.equal(replacement.getAttribute("disabled"), null);
});

test("missing Back, unrelated clicks and thread navigation keep their existing behavior", () => {
  const h = harness();
  h.setButton(null);
  h.install();
  const other = new Element();
  assert.equal(h.click(other).prevented, false);
  other.thread = true;
  assert.equal(h.click(other).prevented, false);
  assert.deepEqual(h.counts(), { closes: 0, navigations: 1 });
});

test("all page implementations restore Back on close and synchronize it on opening", () => {
  const extension = readFileSync(new URL("../../../assets/inject/renderer-inject/92-extension-host.js", import.meta.url), "utf8");
  assert.match(extension.match(/^  function closeCodexPlusPage\([^]*?^  \}/m)?.[0] ?? "", /restoreCodexPlusPageBackButton\(\)/);
  assert.match(source.match(/^  function observeCodexPlusPageLayout\([^]*?^  \}/m)?.[0] ?? "", /syncCodexPlusPageBackButton\(\)/);
});
