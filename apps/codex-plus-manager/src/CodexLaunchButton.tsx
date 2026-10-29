/**
 * 「启动 Codex / 重启 Codex」主按钮：Codex 页面的外壳顶栏右侧（#2744 / #2745：2.x 升级后用户找不到启动入口）。
 * 状态和动作都由 App 传进来，和侧栏播放键、⌘K、托盘面板走同一套 launch / restart 流程，这里只负责画按钮。
 */
import { Loader2, Play, RotateCw } from "lucide-react";
import { Button } from "@/components/ui/button";
import { t } from "./i18n";

export type CodexLaunchState = {
  /** Codex 应用正在运行：按钮变成「重启 Codex」。 */
  running: boolean;
  /** 启动 / 重启 / 停止流程进行中（App 的 launchPending）。 */
  busy: boolean;
  /** 不能启动的原因（例如没有找到 Codex 应用）；null 表示可以启动。 */
  unavailableReason: string | null;
  onLaunch: () => void;
  onRestart: () => void;
};

export function CodexLaunchButton({ state, size = "default", className = "" }: {
  state: CodexLaunchState;
  size?: "default" | "sm";
  className?: string;
}) {
  // 运行中仍然可以重启；只有还没启动、又找不到 Codex 时才置灰。
  const unavailable = !state.running && Boolean(state.unavailableReason);
  const label = state.busy ? t("处理中…") : state.running ? t("重启 Codex") : t("启动 Codex");
  const hint = unavailable
    ? state.unavailableReason ?? ""
    : state.running
      ? t("重启 Codex，让供应商和增强设置重新生效")
      : t("启动 Codex，并加载 Codex++ 的供应商和增强设置");
  return (
    // 置灰的按钮不响应鼠标（disabled:pointer-events-none），提示挂在外层，悬停时仍能看到原因。
    <span className={`codex-launch ${className}`.trim()} data-running={state.running} data-unavailable={unavailable} title={hint}>
      <Button
        className="codex-launch-button"
        disabled={state.busy || unavailable}
        onClick={state.running ? state.onRestart : state.onLaunch}
        size={size}
        title={hint}
        type="button"
        variant="default"
      >
        {state.busy ? <Loader2 aria-hidden="true" className="animate-spin" /> : state.running ? <RotateCw aria-hidden="true" /> : <Play aria-hidden="true" />}
        {label}
      </Button>
    </span>
  );
}
