/** 侧栏状态卡、总览页、网关页共用的网关数据：各自按自己的间隔静默刷新，失败时保留上一次的值。 */
import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { GatewayPayload, GatewayResult } from "./gateway-model";

const shared: { payload: GatewayPayload | null } = { payload: null };
const listeners = new Set<(payload: GatewayPayload) => void>();

/** 命令返回了新的网关数据（如在总览里接入了 Agent、在网关页改了供应商）：立刻推给所有在用的组件，不等下次轮询。 */
export function publishGatewayPayload(result: GatewayResult) {
  if (!result.gateway || !Array.isArray(result.pool)) return;
  shared.payload = result as GatewayPayload;
  for (const listener of listeners) listener(shared.payload);
}

export function useGatewayPayload(intervalMs: number): GatewayPayload | null {
  const [payload, setPayload] = useState<GatewayPayload | null>(shared.payload);
  useEffect(() => {
    listeners.add(setPayload);
    return () => {
      listeners.delete(setPayload);
    };
  }, []);
  useEffect(() => {
    let cancelled = false;
    const load = () =>
      invoke<GatewayResult>("load_gateway")
        .then((result) => {
          if (cancelled || !result.gateway || !Array.isArray(result.pool)) return;
          shared.payload = result as GatewayPayload;
          setPayload(shared.payload);
        })
        .catch(() => undefined);
    void load();
    const timer = window.setInterval(load, intervalMs);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [intervalMs]);
  return payload;
}
