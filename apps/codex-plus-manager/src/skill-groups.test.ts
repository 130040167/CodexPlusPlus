import assert from "node:assert/strict";
import test from "node:test";
import { customSkillGroupKey, defaultSkillGroupKey, groupSkills, moveSkillGroup, nextSkillGroupSortDirection, repositorySkillGroupKey, sortSkillGroupKeys, sortSkillGroupsByName } from "./skill-groups.ts";

const labels = { builtin: "内置", user: "用户" };
const skill = (id: string, repoKey = "", bundled = false) => ({ id, repoKey, bundled });

test("default group uses actual source and keeps bundled skills fixed", () => {
  assert.equal(defaultSkillGroupKey(skill("builtin", "owner/repo@main", true)), "builtin");
  assert.equal(defaultSkillGroupKey(skill("cc", "cc-switch")), "user");
  assert.equal(defaultSkillGroupKey(skill("repo", "owner/repo@main:skills")), "repo:owner/repo");
});

test("groups by actual repository, not by import channel, and keeps empty groups", () => {
  const items = [skill("builtin", "", true), skill("mine"), skill("ccs", "cc-switch"),
    skill("local", "local"), skill("remote", "owner/repo@main:skills")];
  const groups = groupSkills(items, items, ["空分组"], {}, [], labels);
  assert.deepEqual(groups.slice(0, 2).map((group) => group.key), ["builtin", "user"]);
  assert.deepEqual(groups.find((group) => group.key === "user")?.items.map((item) => item.id), ["mine", "ccs", "local"]);
  assert.deepEqual(groups.find((group) => group.key === repositorySkillGroupKey("owner/repo"))?.items.map((item) => item.id), ["remote"]);
  assert.deepEqual(groups.find((group) => group.key === customSkillGroupKey("空分组"))?.items, []);
});

test("supports legacy and explicit assignments without moving bundled skills", () => {
  const items = [skill("a", "owner/repo@main"), skill("b"), skill("builtin", "", true)];
  const groups = groupSkills(items, items, ["自定义"], { a: "自定义", b: "repo:owner/repo", builtin: "user" }, [], labels);
  assert.deepEqual(groups.find((group) => group.key === "custom:自定义")?.items.map((item) => item.id), ["a"]);
  assert.deepEqual(groups.find((group) => group.key === "repo:owner/repo")?.items.map((item) => item.id), ["b"]);
  assert.deepEqual(groups.find((group) => group.key === "builtin")?.items.map((item) => item.id), ["builtin"]);
});

test("persists movable order and sorts or drags without moving pinned groups", () => {
  const items = [skill("a", "z/repo@main"), skill("b", "a/repo@main")];
  const groups = groupSkills(items, items, ["middle"], {}, ["repo:z/repo", "custom:middle", "repo:a/repo"], labels);
  assert.deepEqual(groups.map((group) => group.key), ["builtin", "user", "repo:z/repo", "custom:middle", "repo:a/repo"]);
  assert.deepEqual(moveSkillGroup(groups, "repo:a/repo", "repo:z/repo"), ["repo:a/repo", "repo:z/repo", "custom:middle"]);
  assert.deepEqual(moveSkillGroup(groups, "builtin", "repo:z/repo"), ["repo:z/repo", "custom:middle", "repo:a/repo"]);
  assert.deepEqual(sortSkillGroupKeys(groups), ["repo:a/repo", "custom:middle", "repo:z/repo"]);
});

test("repository group renames keep their identity, including when the group is empty", () => {
  const all = [skill("a", "owner/repo@main")];
  const groups = groupSkills([], all, [], {}, [], labels, true, { "repo:owner/repo": "新的名称" });
  assert.equal(groups.find((group) => group.key === "repo:owner/repo")?.label, "新的名称");
  assert.deepEqual(groups.find((group) => group.key === "repo:owner/repo")?.items, []);
});

test("repository groups remain manageable after their last skill is moved away", () => {
  const groups = groupSkills([], [], [], {}, ["repo:owner/old"], labels);
  assert.deepEqual(groups.find((group) => group.key === "repo:owner/old")?.items, []);
});

test("name sorting toggles both directions while builtin and user groups stay pinned", () => {
  const items = [skill("a", "z/repo@main"), skill("b", "a/repo@main")];
  const groups = groupSkills(items, items, ["middle"], {}, ["repo:z/repo", "custom:middle", "repo:a/repo"], labels, true,
    { "repo:z/repo": "first", "repo:a/repo": "last" });
  const original = groups.map((group) => group.key);
  assert.deepEqual(sortSkillGroupsByName(groups, "asc").map((group) => group.key),
    ["builtin", "user", "repo:z/repo", "repo:a/repo", "custom:middle"]);
  assert.deepEqual(sortSkillGroupsByName(groups, "desc").map((group) => group.key),
    ["builtin", "user", "custom:middle", "repo:a/repo", "repo:z/repo"]);
  assert.deepEqual(sortSkillGroupKeys(groups, "desc"), ["custom:middle", "repo:a/repo", "repo:z/repo"]);
  assert.deepEqual(groups.map((group) => group.key), original);
  assert.equal(nextSkillGroupSortDirection(null), "asc");
  assert.equal(nextSkillGroupSortDirection("asc"), "desc");
  assert.equal(nextSkillGroupSortDirection("desc"), "asc");
});

test("active name sorting includes newly added groups and preserves equal-name order", () => {
  const groups = groupSkills([], [], ["same", "new"], {}, [], labels, true, { "repo:example/empty": "same" });
  const ascending = sortSkillGroupsByName(groups, "asc");
  assert.deepEqual(ascending.map((group) => group.key), ["builtin", "user", "custom:new", "custom:same", "repo:example/empty"]);
  assert.deepEqual(sortSkillGroupsByName(groups, "desc").map((group) => group.key),
    ["builtin", "user", "custom:same", "repo:example/empty", "custom:new"]);
});
