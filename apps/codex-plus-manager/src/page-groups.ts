/** Agent 页面分组：侧栏只显示分组（概览 / 供应商 / 增强 / 设置），分组里的页面在内容区顶部用页签切换。 */

export type PageGroup<R extends string> = { id: string; routes: R[] };

/** 当前 Agent 能看到的分组页面：不属于任何 Agent 的页面（概览）或属于当前 Agent 的页面。 */
export function visibleGroupRoutes<R extends string>(group: PageGroup<R>, toolOf: (route: R) => string | undefined, tool: string): R[] {
  return group.routes.filter((route) => {
    const owner = toolOf(route);
    return !owner || owner === tool;
  });
}

export function groupOfRoute<R extends string, G extends PageGroup<R>>(groups: G[], route: R): G | undefined {
  return groups.find((group) => group.routes.includes(route));
}

/** 点分组时去哪一页：上次在这个分组里看的页面（当前 Agent 还能看到的话），否则第一页。 */
export function groupTarget<R extends string>(visible: R[], remembered: R | undefined): R | undefined {
  return remembered && visible.includes(remembered) ? remembered : visible[0];
}
