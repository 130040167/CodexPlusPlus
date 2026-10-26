/** 本地网关页各标签共用的小组件与类型。 */
import type { CSSProperties, ReactNode } from "react";
import { providerAvatarText } from "./provider-name";

export type Status = "ok" | "failed" | "warning" | "info";
export type Notify = (title: string, message: string, status?: Status) => Promise<void>;
export type Run = (key: string, command: string, args: Record<string, unknown> | undefined, title: string, silent?: boolean) => Promise<boolean>;

/** 品牌图标随应用打包，来源和许可见 assets/agents/LICENSE.txt。 */
export const AGENT_ICONS: Record<string, string> = {
  claude: new URL("./assets/agents/claude.svg", import.meta.url).href,
  grok: new URL("./assets/agents/grok.svg", import.meta.url).href,
  codex: new URL("./assets/agents/chatgpt.svg", import.meta.url).href,
  codexApp: new URL("./assets/agents/codex.svg", import.meta.url).href,
};

export const APP_LOGO_URL = new URL("./assets/codex-plus-logo.png", import.meta.url).href;

function hueOf(name: string): number {
  let hash = 0;
  for (const char of name) hash = (hash * 31 + (char.codePointAt(0) ?? 0)) % 360;
  return hash;
}

export function avatarOf(name: string): string {
  return providerAvatarText(name, "P");
}

/** 已知品牌用打包的图标，其它供应商用按名字固定配色的字母头像。 */
function brandIcon(name: string): string | null {
  const lower = name.toLowerCase();
  if (lower.includes("chatgpt") || lower.includes("openai") || lower.includes("官方登录")) return AGENT_ICONS.codex;
  if (lower.includes("claude") || lower.includes("anthropic")) return AGENT_ICONS.claude;
  if (lower.includes("grok") || lower === "xai") return AGENT_ICONS.grok;
  return null;
}

export function Avatar({ name, size = "md", plain = false }: { name: string; size?: "sm" | "md" | "lg"; plain?: boolean }) {
  const icon = plain ? null : brandIcon(name);
  if (icon) {
    return (
      <span className="gw-avatar gw-avatar-brand" data-size={size} title={name}>
        <img alt="" src={icon} />
      </span>
    );
  }
  return (
    <span className="gw-avatar" data-size={size} style={{ "--gw-hue": hueOf(name) } as CSSProperties} title={name}>
      {avatarOf(name)}
    </span>
  );
}

export function Switch({ on, disabled, label, onToggle }: { on: boolean; disabled?: boolean; label: string; onToggle: () => void }) {
  return (
    <button aria-checked={on} aria-label={label} className={`gw-switch ${on ? "is-on" : ""}`} disabled={disabled} onClick={onToggle} role="switch" type="button">
      <span />
    </button>
  );
}

export function SectionLabel({ children, aside }: { children: ReactNode; aside?: ReactNode }) {
  return (
    <div className="gw-section-label">
      <span>{children}</span>
      {aside ? <div className="gw-section-aside">{aside}</div> : null}
    </div>
  );
}

/** 收藏的模型只存在本机浏览器存储里（每个查看者自己的偏好）。 */
const FAVORITES_KEY = "codexpp.gateway.favoriteModels";

export function loadFavorites(): string[] {
  try {
    const value = JSON.parse(window.localStorage.getItem(FAVORITES_KEY) ?? "[]");
    return Array.isArray(value) ? value.filter((item): item is string => typeof item === "string") : [];
  } catch {
    return [];
  }
}

export function saveFavorites(models: string[]) {
  try {
    window.localStorage.setItem(FAVORITES_KEY, JSON.stringify(models));
  } catch {
    // 存不了就只在本次会话里生效。
  }
}
