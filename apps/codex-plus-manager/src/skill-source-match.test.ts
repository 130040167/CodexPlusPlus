import assert from "node:assert/strict";
import { test } from "node:test";
import { groupSourceMatches, preferredSourceMatch, selectedSourceMatches, sourceMatchKey, sourceMatchLabel, type SkillSourceMatch } from "./skill-source-match.ts";

const match = (localId: string, repoPath: string, exact: boolean): SkillSourceMatch => ({ localId, localName: localId, localHash: "local", snapshot: "snapshot", repoKey: "owner/repo@main", repoPath, remoteName: repoPath, remoteHash: "remote", exact });

test("source labels distinguish branches for the same repository path", () => {
  const main = match("a", "skills/alpha", true);
  const dev = { ...main, repoKey: "owner/repo@feature/dev" };
  assert.equal(sourceMatchLabel(main), "owner/repo@main / skills/alpha");
  assert.equal(sourceMatchLabel(dev), "owner/repo@feature/dev / skills/alpha");
  assert.notEqual(sourceMatchLabel(main), sourceMatchLabel(dev));
});

test("source labels preserve configured subdirectories and the full skill path", () => {
  const curated = { ...match("a", "skills/.curated/alpha", true), repoKey: "owner/repo@main:skills/.curated" };
  const all = { ...curated, repoKey: "owner/repo@main:skills" };
  assert.equal(sourceMatchLabel(curated), "owner/repo@main:skills/.curated / skills/.curated/alpha");
  assert.equal(sourceMatchLabel(all), "owner/repo@main:skills / skills/.curated/alpha");
  assert.notEqual(sourceMatchLabel(curated), sourceMatchLabel(all));
});

test("prefers a unique content match without guessing among multiple exact candidates", () => {
  const exact = match("a", "one", true);
  const weak = match("a", "two", false);
  assert.equal(preferredSourceMatch([weak, exact]), sourceMatchKey(exact));
  assert.equal(preferredSourceMatch([exact, match("a", "three", true)]), "");
  assert.equal(preferredSourceMatch([weak, match("a", "three", false)]), "");
});

test("rejects missing selections and assigning one repository path to multiple skills", () => {
  const a = match("a", "one", true);
  const b = match("b", "one", true);
  const groups = groupSourceMatches([a, b]);
  assert.equal(selectedSourceMatches(groups, { a: "" }), null);
  assert.equal(selectedSourceMatches(groups, { a: sourceMatchKey(a), b: sourceMatchKey(b) }), null);
  assert.deepEqual(selectedSourceMatches(groups, { a: sourceMatchKey(a) }), [a]);
});

test("deduplicates repository candidates and keeps exact matches first", () => {
  const weak = match("a", "two", false);
  const exact = match("a", "one", true);
  assert.deepEqual(groupSourceMatches([weak, exact, exact]).get("a"), [exact, weak]);
});
