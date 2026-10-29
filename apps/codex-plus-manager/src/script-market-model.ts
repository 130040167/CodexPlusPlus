// 脚本市场与本地脚本列表的纯逻辑（#2781）。不依赖 i18n / React，便于 node --test 直接测；
// 文案由调用方按返回的种类翻译。

/** 市场排序：清单原顺序（维护者编排）、最近更新优先、按名称。 */
export type ScriptMarketSort = "default" | "updated" | "name";

type SortableMarketScript = { name: string; updatedAt?: string };

/** 解析清单里的更新时间；空串、非法值返回 null（视为「未标注」）。 */
export function parseMarketTime(value: string | undefined | null): number | null {
  const text = (value ?? "").trim();
  if (!text) return null;
  const time = new Date(text).getTime();
  return Number.isNaN(time) ? null : time;
}

/**
 * 默认排序：清单里每个脚本都标了有效更新时间时按「最近更新」排，
 * 否则保持清单原顺序——只有部分脚本有时间时，按时间排会把没标的老脚本和
 * 真正停更的脚本混在末尾，反而误导。
 */
export function defaultMarketSort(scripts: SortableMarketScript[]): ScriptMarketSort {
  if (!scripts.length) return "default";
  return scripts.every((script) => parseMarketTime(script.updatedAt) !== null) ? "updated" : "default";
}

/** 返回排序后的新数组，不改入参。同值保持清单原顺序；没有更新时间的排在最后。 */
export function sortMarketScripts<T extends SortableMarketScript>(scripts: T[], sort: ScriptMarketSort): T[] {
  if (sort === "default") return scripts.slice();
  const indexed = scripts.map((script, index) => ({ script, index, time: parseMarketTime(script.updatedAt) }));
  indexed.sort((left, right) => {
    if (sort === "updated") {
      if (left.time !== right.time) {
        if (left.time === null) return 1;
        if (right.time === null) return -1;
        return right.time - left.time;
      }
    } else {
      const byName = left.script.name.localeCompare(right.script.name, undefined, { sensitivity: "base", numeric: true });
      if (byName) return byName;
    }
    return left.index - right.index;
  });
  return indexed.map((item) => item.script);
}

export type RelativeAge =
  | { unit: "justNow" }
  | { unit: "hours" | "days" | "months" | "years"; value: number };

/** 距今多久。时钟偏差导致的「未来时间」按刚刚处理。 */
export function relativeAge(time: number, now: number): RelativeAge {
  const hour = 60 * 60 * 1000;
  const day = 24 * hour;
  const elapsed = Math.max(0, now - time);
  if (elapsed < hour) return { unit: "justNow" };
  if (elapsed < day) return { unit: "hours", value: Math.floor(elapsed / hour) };
  if (elapsed < 30 * day) return { unit: "days", value: Math.floor(elapsed / day) };
  if (elapsed < 365 * day) return { unit: "months", value: Math.floor(elapsed / (30 * day)) };
  return { unit: "years", value: Math.floor(elapsed / (365 * day)) };
}

/**
 * 本地脚本合并后的单列状态。
 *
 * 原来「启用 / 关闭」（用户开关）和 status（loaded / disabled …）分两列：关闭时两列都在说
 * 「关了」，开启时第二列才有信息。合并后开关单独成控件，状态只描述开关之外的事实：
 * 整体开关关着、已加载、加载中、加载失败、未加载（Codex 未运行或还没热重载）。
 */
export type LocalScriptState = "off" | "globalOff" | "loaded" | "loading" | "failed" | "notLoaded";

export function localScriptState(script: { enabled: boolean; status: string }, globalEnabled: boolean): LocalScriptState {
  if (!script.enabled) return "off";
  if (!globalEnabled) return "globalOff";
  switch (script.status) {
    case "loaded":
      return "loaded";
    case "loading":
      return "loading";
    case "failed":
      return "failed";
    default:
      return "notLoaded";
  }
}
