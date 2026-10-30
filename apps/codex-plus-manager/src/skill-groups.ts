export type GroupableSkill = { id: string; repoKey: string; bundled: boolean };
export type SkillGroup<T> = { key: string; label: string; kind: "builtin" | "user" | "repo" | "custom"; items: T[] };
export type SkillGroupSortDirection = "asc" | "desc";

export const BUILTIN_SKILL_GROUP = "builtin";
export const USER_SKILL_GROUP = "user";
export const customSkillGroupKey = (name: string) => `custom:${name}`;
export const repositorySkillGroupKey = (name: string) => `repo:${name}`;

/** cc-switch 等只是导入渠道；没有明确仓库的 Skill 归入用户分组。 */
export function skillRepositoryName(repoKey: string): string | null {
  const match = /^([^/@:]+\/[^/@:]+)@[^:]+(?::.*)?$/.exec(repoKey);
  return match?.[1] ?? null;
}

export function defaultSkillGroupKey(skill: GroupableSkill): string {
  if (skill.bundled) return BUILTIN_SKILL_GROUP;
  const repository = skillRepositoryName(skill.repoKey);
  return repository ? repositorySkillGroupKey(repository) : USER_SKILL_GROUP;
}

export function groupSkills<T extends GroupableSkill>(
  visible: T[],
  known: T[],
  customNames: string[],
  assignments: Record<string, string>,
  order: string[],
  labels: { builtin: string; user: string },
  includeEmpty = true,
  aliases: Record<string, string> = {},
): SkillGroup<T>[] {
  const groups = new Map<string, SkillGroup<T>>();
  const add = (key: string, label: string, kind: SkillGroup<T>["kind"]) => {
    if (!groups.has(key)) groups.set(key, { key, label, kind, items: [] });
  };
  if (includeEmpty) {
    add(BUILTIN_SKILL_GROUP, labels.builtin, "builtin");
    add(USER_SKILL_GROUP, labels.user, "user");
    for (const name of customNames) add(customSkillGroupKey(name), name, "custom");
    for (const skill of known) {
      const name = skillRepositoryName(skill.repoKey);
      if (name) add(repositorySkillGroupKey(name), aliases[repositorySkillGroupKey(name)] || name, "repo");
    }
    // 保留曾显示或手动排序过的仓库分组，哪怕最后一个 Skill 已移动走。
    for (const key of [...order, ...Object.keys(aliases), ...Object.values(assignments)]) {
      if (typeof key === "string" && key.startsWith("repo:") && key.length > 5)
        add(key, aliases[key] || key.slice(5), "repo");
    }
  }
  for (const skill of visible) {
    const assignment = assignments[skill.id];
    const legacyCustom = customNames.includes(assignment) ? customSkillGroupKey(assignment) : assignment;
    let key = defaultSkillGroupKey(skill);
    if (!skill.bundled && legacyCustom === USER_SKILL_GROUP) key = USER_SKILL_GROUP;
    else if (!skill.bundled && legacyCustom?.startsWith("custom:") && customNames.includes(legacyCustom.slice(7))) key = legacyCustom;
    else if (!skill.bundled && legacyCustom?.startsWith("repo:") && legacyCustom.length > 5) key = legacyCustom;
    if (key === BUILTIN_SKILL_GROUP) add(key, labels.builtin, "builtin");
    else if (key === USER_SKILL_GROUP) add(key, labels.user, "user");
    else if (key.startsWith("custom:")) add(key, key.slice(7), "custom");
    else add(key, aliases[key] || key.slice(5), "repo");
    groups.get(key)!.items.push(skill);
  }
  const position = new Map(order.map((key, index) => [key, index]));
  return [...groups.values()].sort((a, b) => {
    const pinned = (group: SkillGroup<T>) => group.kind === "builtin" ? 0 : group.kind === "user" ? 1 : 2;
    const rank = pinned(a) - pinned(b);
    if (rank) return rank;
    const aPosition = position.get(a.key) ?? Number.MAX_SAFE_INTEGER;
    const bPosition = position.get(b.key) ?? Number.MAX_SAFE_INTEGER;
    return aPosition - bPosition || a.label.localeCompare(b.label);
  });
}

/** 排序和拖动都只影响普通分组，内置和用户分组保持固定。 */
export function sortSkillGroupsByName<T>(groups: SkillGroup<T>[], direction: SkillGroupSortDirection): SkillGroup<T>[] {
  const pinned = groups.filter((group) => group.kind === "builtin" || group.kind === "user");
  const movable = groups.filter((group) => group.kind === "repo" || group.kind === "custom")
    .sort((a, b) => (direction === "asc" ? 1 : -1) * a.label.localeCompare(b.label));
  return [...pinned, ...movable];
}

export function sortSkillGroupKeys<T>(groups: SkillGroup<T>[], direction: SkillGroupSortDirection = "asc"): string[] {
  return sortSkillGroupsByName(groups, direction)
    .filter((group) => group.kind === "repo" || group.kind === "custom").map((group) => group.key);
}

export function nextSkillGroupSortDirection(direction: SkillGroupSortDirection | null): SkillGroupSortDirection {
  return direction === "asc" ? "desc" : "asc";
}

export function moveSkillGroup<T>(groups: SkillGroup<T>[], from: string, to: string): string[] {
  const movable = groups.filter((group) => group.kind === "repo" || group.kind === "custom").map((group) => group.key);
  const source = movable.indexOf(from);
  const destination = movable.indexOf(to);
  if (source < 0 || destination < 0 || source === destination) return movable;
  movable.splice(destination, 0, ...movable.splice(source, 1));
  return movable;
}
