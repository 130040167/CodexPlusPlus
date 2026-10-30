//! 第三方 Skill 的导入预览、重复匹配和显式覆盖保护。
use super::*;

pub(super) struct SkillTargetSnapshot {
    pub token: String,
    pub directory_hashes: Vec<Option<String>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillImportMatch {
    pub id: String,
    pub name: String,
    pub reasons: Vec<String>,
    pub snapshot: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillImportCandidate {
    pub id: String,
    pub name: String,
    pub description: String,
    pub source_hash: String,
    pub repo_key: String,
    pub repo_path: String,
    pub matches: Vec<SkillImportMatch>,
    pub duplicate_candidates: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillImportSelection {
    pub id: String,
    pub source_hash: String,
    pub repo_key: String,
    pub repo_path: String,
    pub target_id: Option<String>,
    pub target_snapshot: Option<String>,
}

impl SkillsManager {
    pub fn preview_skill_import(
        &self,
        candidates: &[(String, PathBuf)],
        origins: &BTreeMap<String, crate::ccs_import::CcsSkillOrigin>,
    ) -> anyhow::Result<Vec<SkillImportCandidate>> {
        self.prepare_managed_skills()?;
        let _operation = self.operation_lock.lock().unwrap();
        let _guard = self.state_lock.lock().unwrap();
        let state = self.load_state_checked_unlocked()?;
        let mut result = Vec::new();
        for (id, path) in candidates {
            validate_skill_id(id)?;
            anyhow::ensure!(
                path.join(SKILL_MANIFEST_FILE).is_file(),
                "{id} 没有 SKILL.md"
            );
            let source_hash = skill_directory_hash(path)?;
            let (name, description) = read_skill_manifest(&path.join(SKILL_MANIFEST_FILE), id);
            let matches = self.import_matches_unlocked(&state, id, &name, &source_hash)?;
            let origin = origins.get(id);
            result.push(SkillImportCandidate {
                id: id.clone(),
                name,
                description,
                source_hash,
                repo_key: origin.map(|item| item.repo_key.clone()).unwrap_or_default(),
                repo_path: origin
                    .map(|item| item.repo_path.clone())
                    .unwrap_or_default(),
                matches,
                duplicate_candidates: Vec::new(),
            });
        }
        for index in 0..result.len() {
            result[index].duplicate_candidates = result
                .iter()
                .enumerate()
                .filter(|(other, candidate)| {
                    *other != index
                        && (same_name(&candidate.name, &result[index].name)
                            || candidate.source_hash == result[index].source_hash)
                })
                .map(|(_, candidate)| candidate.id.clone())
                .collect();
        }
        Ok(result)
    }

    pub fn apply_skill_import(
        &self,
        candidates: &[(String, PathBuf)],
        origins: &BTreeMap<String, crate::ccs_import::CcsSkillOrigin>,
        import_source: &str,
        selections: &[SkillImportSelection],
    ) -> anyhow::Result<(usize, Vec<String>)> {
        anyhow::ensure!(
            matches!(import_source, LOCAL_IMPORT_REPO_KEY | "cc-switch"),
            "不支持的导入渠道"
        );
        let preview = self.preview_skill_import(candidates, origins)?;
        let mut selected_ids = std::collections::BTreeSet::new();
        let mut target_ids = std::collections::BTreeSet::new();
        let mut protected_targets = std::collections::BTreeSet::new();
        for selection in selections {
            anyhow::ensure!(
                selected_ids.insert(selection.id.clone()),
                "同一个导入项不能重复选择"
            );
            let target = selection.target_id.as_deref().unwrap_or(&selection.id);
            anyhow::ensure!(
                target_ids.insert(target.to_lowercase()),
                "多个导入项不能同时写入同一个 Skill"
            );
            if selection.target_id.is_some() {
                protected_targets.insert(target.to_lowercase());
            }
        }
        let mut imported = 0;
        let mut errors = Vec::new();
        for selection in selections {
            let candidate = match preview.iter().find(|item| item.id == selection.id) {
                Some(candidate) => candidate,
                None => {
                    errors.push(format!("{}：导入项已不存在", selection.id));
                    continue;
                }
            };
            if candidate.source_hash != selection.source_hash {
                errors.push(format!("{}：导入源已变化，请重新预览", selection.id));
                continue;
            }
            if candidate.repo_key != selection.repo_key
                || candidate.repo_path != selection.repo_path
            {
                errors.push(format!("{}：仓库来源已变化，请重新预览", selection.id));
                continue;
            }
            if candidate
                .duplicate_candidates
                .iter()
                .any(|id| selected_ids.contains(id))
            {
                errors.push(format!(
                    "{}：所选导入项之间存在同名或相同内容，请只选择一个",
                    selection.id
                ));
                continue;
            }
            let (target, snapshot) = match (&selection.target_id, &selection.target_snapshot) {
                (Some(target), Some(snapshot))
                    if candidate
                        .matches
                        .iter()
                        .any(|item| item.id == *target && item.snapshot == *snapshot) =>
                {
                    (target.as_str(), Some(snapshot.as_str()))
                }
                (None, None) if candidate.matches.is_empty() => (candidate.id.as_str(), None),
                _ => {
                    errors.push(format!(
                        "{}：重复项或已有内容已变化，请重新预览并选择",
                        selection.id
                    ));
                    continue;
                }
            };
            let path = &candidates
                .iter()
                .find(|(id, _)| id == &selection.id)
                .expect("preview candidate")
                .1;
            let current = match self.preview_skill_import(candidates, origins) {
                Ok(current) => current,
                Err(error) => {
                    errors.push(format!("{}：重新检查失败：{error:#}", selection.id));
                    continue;
                }
            };
            let current = current
                .iter()
                .find(|item| item.id == selection.id)
                .expect("unchanged candidate list");
            if current.source_hash != selection.source_hash
                || current.repo_key != selection.repo_key
                || current.repo_path != selection.repo_path
            {
                errors.push(format!(
                    "{}：导入源或仓库来源已变化，请重新预览",
                    selection.id
                ));
                continue;
            }
            let current_ids: std::collections::BTreeSet<_> = current
                .matches
                .iter()
                .map(|item| item.id.to_lowercase())
                .collect();
            let shown_ids: std::collections::BTreeSet<_> = candidate
                .matches
                .iter()
                .map(|item| item.id.to_lowercase())
                .collect();
            if current_ids != shown_ids
                || current_ids
                    .iter()
                    .any(|id| target_ids.contains(id) && !protected_targets.contains(id))
            {
                errors.push(format!("{}：重复项已变化，请重新预览", selection.id));
                continue;
            }
            match self.import_directory_with_source(
                &candidate.id,
                target,
                path,
                import_source,
                origins.get(&candidate.id),
                Some(&selection.source_hash),
                snapshot,
            ) {
                Ok(_) => imported += 1,
                Err(error) => errors.push(format!("{}：{error:#}", selection.id)),
            }
        }
        Ok((imported, errors))
    }

    pub(super) fn import_target_snapshot_unlocked(
        &self,
        state: &SkillsState,
        id: &str,
    ) -> anyhow::Result<String> {
        Ok(self
            .import_target_snapshot_details_unlocked(state, id)?
            .token)
    }

    pub(super) fn import_target_snapshot_details_unlocked(
        &self,
        state: &SkillsState,
        id: &str,
    ) -> anyhow::Result<SkillTargetSnapshot> {
        validate_skill_id(id)?;
        let mut hash = Sha256::new();
        hash.update(serde_json::to_vec(&state.installed.get(id))?);
        let mut roots = vec![self.source_dir.clone(), self.linked_dir()];
        roots.extend(self.agent_dirs.iter().map(|(_, root)| root.clone()));
        let mut directory_hashes = Vec::with_capacity(roots.len());
        for root in roots {
            let path = root.join(id);
            match std::fs::symlink_metadata(&path) {
                Ok(metadata) => {
                    hash.update(b"present");
                    hash.update(if metadata.file_type().is_symlink() {
                        b"link"
                    } else {
                        b"copy"
                    });
                    hash.update(if is_managed_copy(&path) {
                        &b"managed"[..]
                    } else {
                        &b"external"[..]
                    });
                    let directory_hash = skill_directory_hash(&path)?;
                    hash.update(directory_hash.as_bytes());
                    directory_hashes.push(Some(directory_hash));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    hash.update(b"absent");
                    directory_hashes.push(None);
                }
                Err(error) => return Err(error.into()),
            }
            hash.update([0]);
        }
        Ok(SkillTargetSnapshot {
            token: format!("{:x}", hash.finalize()),
            directory_hashes,
        })
    }

    pub(super) fn import_matches_unlocked(
        &self,
        state: &SkillsState,
        id: &str,
        name: &str,
        source_hash: &str,
    ) -> anyhow::Result<Vec<SkillImportMatch>> {
        let mut existing: BTreeMap<String, String> = state
            .installed
            .iter()
            .map(|(id, item)| {
                (
                    id.clone(),
                    if item.name.is_empty() {
                        id.clone()
                    } else {
                        item.name.clone()
                    },
                )
            })
            .collect();
        for skill in self.list_bundled_skills() {
            if same_name(id, &skill.id) || same_name(name, &skill.name) {
                anyhow::bail!("导入项与 Codex 内置 Skill 重名：{}", skill.id);
            }
        }
        for skill in self.list_local_skills() {
            existing.entry(skill.id).or_insert(skill.name);
        }
        for (_, root) in &self.agent_dirs {
            for (id, (name, _)) in scan_skill_dirs(root) {
                existing.entry(id).or_insert(name);
            }
        }
        let mut matches = Vec::new();
        for (target, existing_name) in existing {
            let mut reasons = Vec::new();
            if same_name(id, &target) {
                reasons.push("id".into());
            }
            if same_name(name, &existing_name) {
                reasons.push("name".into());
            }
            let path = std::iter::once(self.source_dir.join(&target))
                .chain(std::iter::once(self.linked_dir().join(&target)))
                .chain(self.agent_dirs.iter().map(|(_, root)| root.join(&target)))
                .find(|path| path.join(SKILL_MANIFEST_FILE).is_file());
            if let Some(path) = path {
                if skill_directory_hash(&path)? == source_hash {
                    reasons.push("content".into());
                }
            }
            if !reasons.is_empty() {
                matches.push(SkillImportMatch {
                    snapshot: self.import_target_snapshot_unlocked(state, &target)?,
                    id: target,
                    name: existing_name,
                    reasons,
                });
            }
        }
        Ok(matches)
    }
}

fn same_name(left: &str, right: &str) -> bool {
    !left.trim().is_empty() && left.trim().to_lowercase() == right.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, SkillsManager) {
        let temp = tempfile::tempdir().unwrap();
        let manager = SkillsManager::new(
            temp.path().join("library"),
            temp.path().join("backups"),
            temp.path().join("state.json"),
            temp.path().join("home"),
        );
        (temp, manager)
    }

    fn write_skill(dir: &Path, name: &str, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\n---\n{body}"),
        )
        .unwrap();
    }

    #[test]
    fn duplicate_requires_explicit_target_and_preserves_enabled_state() {
        let (temp, manager) = fixture();
        let original = temp.path().join("old");
        let incoming = temp.path().join("new");
        write_skill(&original, "Shared", "old");
        write_skill(&incoming, "Shared", "new");
        manager
            .import_from_local_directory("existing", &original)
            .unwrap();
        manager.set_enabled("existing", true).unwrap();
        assert!(
            manager
                .import_from_local_directory("new", &incoming)
                .is_err()
        );
        let candidates = vec![("new".into(), incoming.clone())];
        let preview = manager
            .preview_skill_import(&candidates, &BTreeMap::new())
            .unwrap();
        assert_eq!(preview[0].matches.len(), 1);
        assert_eq!(preview[0].matches[0].id, "existing");
        let without_choice = SkillImportSelection {
            id: "new".into(),
            source_hash: preview[0].source_hash.clone(),
            repo_key: String::new(),
            repo_path: String::new(),
            target_id: None,
            target_snapshot: None,
        };
        let (count, errors) = manager
            .apply_skill_import(&candidates, &BTreeMap::new(), "local", &[without_choice])
            .unwrap();
        assert_eq!(count, 0);
        assert_eq!(errors.len(), 1);
        let selection = SkillImportSelection {
            id: "new".into(),
            source_hash: preview[0].source_hash.clone(),
            repo_key: String::new(),
            repo_path: String::new(),
            target_id: Some("existing".into()),
            target_snapshot: Some(preview[0].matches[0].snapshot.clone()),
        };
        let (count, errors) = manager
            .apply_skill_import(&candidates, &BTreeMap::new(), "local", &[selection])
            .unwrap();
        assert_eq!(count, 1);
        assert!(errors.is_empty());
        assert!(
            std::fs::read_to_string(manager.source_dir.join("existing/SKILL.md"))
                .unwrap()
                .contains("new")
        );
        assert!(
            std::fs::read_to_string(manager.linked_dir().join("existing/SKILL.md"))
                .unwrap()
                .contains("new")
        );
        assert!(manager.list_backups().iter().any(|backup| {
            std::fs::read_to_string(manager.backups_dir.join(&backup.id).join("SKILL.md"))
                .is_ok_and(|text| text.contains("old"))
        }));
    }

    #[test]
    fn explicit_import_overwrite_updates_existing_agent_copy_and_keeps_backup() {
        let temp = tempfile::tempdir().unwrap();
        let agent_root = temp.path().join("claude");
        let manager = SkillsManager::new(
            temp.path().join("library"),
            temp.path().join("backups"),
            temp.path().join("state.json"),
            temp.path().join("home"),
        )
        .with_agent_dirs(vec![("claude".into(), agent_root.clone())]);
        let existing = agent_root.join("existing");
        let incoming = temp.path().join("incoming");
        write_skill(&existing, "Shared", "original Claude version");
        write_skill(&incoming, "Shared", "imported replacement");
        let candidates = vec![("new".into(), incoming.clone())];
        let preview = manager
            .preview_skill_import(&candidates, &BTreeMap::new())
            .unwrap();
        let target = preview[0]
            .matches
            .iter()
            .find(|item| item.id == "existing")
            .unwrap();
        let selection = SkillImportSelection {
            id: "new".into(),
            source_hash: preview[0].source_hash.clone(),
            repo_key: String::new(),
            repo_path: String::new(),
            target_id: Some(target.id.clone()),
            target_snapshot: Some(target.snapshot.clone()),
        };
        let (count, errors) = manager
            .apply_skill_import(&candidates, &BTreeMap::new(), "local", &[selection])
            .unwrap();
        assert_eq!(count, 1);
        assert!(errors.is_empty());
        assert!(
            std::fs::read_to_string(existing.join("SKILL.md"))
                .unwrap()
                .contains("imported replacement")
        );
        assert!(
            std::fs::read_to_string(manager.source_dir.join("existing/SKILL.md"))
                .unwrap()
                .contains("imported replacement")
        );
        assert!(
            std::fs::read_to_string(incoming.join("SKILL.md"))
                .unwrap()
                .contains("imported replacement")
        );
        assert!(manager.list_backups().iter().any(|backup| {
            std::fs::read_to_string(manager.backups_dir.join(&backup.id).join("SKILL.md"))
                .is_ok_and(|text| text.contains("original Claude version"))
        }));
    }

    #[test]
    fn stale_source_or_target_requires_a_new_preview() {
        let (temp, manager) = fixture();
        let old = temp.path().join("old");
        let incoming = temp.path().join("incoming");
        write_skill(&old, "Shared", "old");
        write_skill(&incoming, "Shared", "incoming");
        manager
            .import_from_local_directory("existing", &old)
            .unwrap();
        let candidates = vec![("new".into(), incoming.clone())];
        let preview = manager
            .preview_skill_import(&candidates, &BTreeMap::new())
            .unwrap();
        let selection = || SkillImportSelection {
            id: "new".into(),
            source_hash: preview[0].source_hash.clone(),
            repo_key: String::new(),
            repo_path: String::new(),
            target_id: Some("existing".into()),
            target_snapshot: Some(preview[0].matches[0].snapshot.clone()),
        };
        write_skill(&incoming, "Shared", "changed source");
        let (count, errors) = manager
            .apply_skill_import(&candidates, &BTreeMap::new(), "local", &[selection()])
            .unwrap();
        assert_eq!(count, 0);
        assert!(!errors.is_empty());
        write_skill(&incoming, "Shared", "incoming");
        write_skill(
            &manager.source_dir.join("existing"),
            "Shared",
            "changed target",
        );
        let (count, errors) = manager
            .apply_skill_import(&candidates, &BTreeMap::new(), "local", &[selection()])
            .unwrap();
        assert_eq!(count, 0);
        assert!(!errors.is_empty());
        assert!(
            std::fs::read_to_string(manager.source_dir.join("existing/SKILL.md"))
                .unwrap()
                .contains("changed target")
        );
    }

    #[test]
    fn duplicate_content_and_duplicate_incoming_items_are_not_silently_imported() {
        let (temp, manager) = fixture();
        let old = temp.path().join("old");
        let one = temp.path().join("one");
        let two = temp.path().join("two");
        write_skill(&old, "same", "old");
        write_skill(&one, "other", "new");
        write_skill(&two, "other", "new");
        manager
            .import_from_local_directory("existing", &old)
            .unwrap();
        let candidates = vec![("one".into(), one), ("two".into(), two)];
        let preview = manager
            .preview_skill_import(&candidates, &BTreeMap::new())
            .unwrap();
        assert_eq!(preview[0].duplicate_candidates, vec!["two"]);
        let selections: Vec<_> = preview
            .iter()
            .map(|item| SkillImportSelection {
                id: item.id.clone(),
                source_hash: item.source_hash.clone(),
                repo_key: String::new(),
                repo_path: String::new(),
                target_id: None,
                target_snapshot: None,
            })
            .collect();
        let (count, errors) = manager
            .apply_skill_import(&candidates, &BTreeMap::new(), "local", &selections)
            .unwrap();
        assert_eq!(count, 0);
        assert_eq!(errors.len(), 2);
        let equal = temp.path().join("equal");
        write_skill(&equal, "same", "old");
        let preview = manager
            .preview_skill_import(&[("different".into(), equal)], &BTreeMap::new())
            .unwrap();
        assert!(
            preview[0].matches[0]
                .reasons
                .iter()
                .any(|reason| reason == "content")
        );
    }

    #[test]
    fn repository_origin_and_import_channel_remain_separate_and_hash_matches_remote() {
        let (temp, manager) = fixture();
        let incoming = temp.path().join("incoming");
        write_skill(&incoming, "alpha", "content");
        let repo = parse_repo_key("owner/repo@main").unwrap();
        let mut origins = BTreeMap::new();
        origins.insert(
            "alpha".into(),
            crate::ccs_import::CcsSkillOrigin {
                repo_key: repo.key(),
                repo_path: "skills/alpha".into(),
            },
        );
        let candidates = vec![("alpha".into(), incoming.clone())];
        let preview = manager.preview_skill_import(&candidates, &origins).unwrap();
        let selection = SkillImportSelection {
            id: "alpha".into(),
            source_hash: preview[0].source_hash.clone(),
            repo_key: preview[0].repo_key.clone(),
            repo_path: preview[0].repo_path.clone(),
            target_id: None,
            target_snapshot: None,
        };
        let (count, errors) = manager
            .apply_skill_import(&candidates, &origins, "cc-switch", &[selection])
            .unwrap();
        assert_eq!(count, 1);
        assert!(errors.is_empty());
        let installed = &manager.load_state().installed["alpha"];
        assert_eq!(installed.repo_key, repo.key());
        assert_eq!(installed.repo_path, "skills/alpha");
        assert_eq!(installed.import_source, "cc-switch");
        let content = std::fs::read(incoming.join("SKILL.md")).unwrap();
        let mut blob = sha1::Sha1::new();
        blob.update(format!("blob {}\0", content.len()).as_bytes());
        blob.update(content);
        let tree = serde_json::json!({"tree":[{"type":"blob","path":"skills/alpha/SKILL.md","sha":format!("{:x}", blob.finalize())}]});
        assert_eq!(
            installed.content_hash,
            parse_skills_from_tree(&repo, &tree)[0].content_hash
        );
    }

    #[test]
    fn imported_repository_requires_verification_and_respects_explicit_removal() {
        let (temp, manager) = fixture();
        let incoming = temp.path().join("incoming");
        write_skill(&incoming, "alpha", "content");
        let repo = parse_repo_key("owner/repo@main").unwrap();
        let mut origins = BTreeMap::new();
        origins.insert(
            "alpha".into(),
            crate::ccs_import::CcsSkillOrigin {
                repo_key: repo.key(),
                repo_path: "skills/alpha".into(),
            },
        );
        let candidates = vec![("alpha".into(), incoming)];
        let preview = manager.preview_skill_import(&candidates, &origins).unwrap();
        let selection = SkillImportSelection {
            id: "alpha".into(),
            source_hash: preview[0].source_hash.clone(),
            repo_key: repo.key(),
            repo_path: "skills/alpha".into(),
            target_id: None,
            target_snapshot: None,
        };
        let (count, errors) = manager
            .apply_skill_import(&candidates, &origins, "cc-switch", &[selection])
            .unwrap();
        assert_eq!(count, 1);
        assert!(errors.is_empty());
        assert!(
            !manager
                .list_repos()
                .iter()
                .any(|item| item.key() == repo.key())
        );
        assert_eq!(manager.pending_imported_repos(), vec![repo.clone()]);
        let remote = vec![RemoteSkill {
            id: "alpha".into(),
            name: "alpha".into(),
            description: String::new(),
            repo_key: repo.key(),
            repo_path: "skills/alpha".into(),
            content_hash: "verified".into(),
        }];
        assert!(
            manager
                .save_verified_imported_repo(repo.clone(), &remote)
                .unwrap()
        );
        assert!(manager.pending_imported_repos().is_empty());
        manager.delete_repo(&repo.key()).unwrap();
        assert!(manager.pending_imported_repos().is_empty());
        assert!(
            !manager
                .save_verified_imported_repo(repo.clone(), &remote)
                .unwrap()
        );
        manager.save_repo(repo.clone(), None).unwrap();
        let mut disabled = repo.clone();
        disabled.enabled = false;
        assert!(
            manager
                .update_repo_enabled_if_unchanged(&disabled, None)
                .unwrap()
        );
        assert!(
            !manager
                .save_verified_imported_repo(repo.clone(), &remote)
                .unwrap()
        );
        assert!(
            !manager
                .list_repos()
                .iter()
                .find(|item| item.key() == repo.key())
                .unwrap()
                .enabled
        );
    }

    #[test]
    fn changed_repository_metadata_is_not_silently_accepted() {
        let (temp, manager) = fixture();
        let incoming = temp.path().join("incoming");
        write_skill(&incoming, "alpha", "content");
        let candidates = vec![("alpha".into(), incoming)];
        let mut origins = BTreeMap::new();
        origins.insert(
            "alpha".into(),
            crate::ccs_import::CcsSkillOrigin {
                repo_key: "owner/old@main".into(),
                repo_path: "alpha".into(),
            },
        );
        let preview = manager.preview_skill_import(&candidates, &origins).unwrap();
        let selection = SkillImportSelection {
            id: "alpha".into(),
            source_hash: preview[0].source_hash.clone(),
            repo_key: preview[0].repo_key.clone(),
            repo_path: preview[0].repo_path.clone(),
            target_id: None,
            target_snapshot: None,
        };
        origins.get_mut("alpha").unwrap().repo_key = "owner/new@main".into();
        let (count, errors) = manager
            .apply_skill_import(&candidates, &origins, "cc-switch", &[selection])
            .unwrap();
        assert_eq!(count, 0);
        assert!(!errors.is_empty());
        assert!(!manager.source_dir.join("alpha").exists());
    }

    #[test]
    fn bundled_name_cannot_be_overwritten_or_imported_as_a_duplicate() {
        let (temp, manager) = fixture();
        let builtin = manager.linked_dir().join(".system/builtin");
        let incoming = temp.path().join("incoming");
        write_skill(&builtin, "Built-in", "original");
        write_skill(&incoming, "Built-in", "replacement");
        assert!(
            manager
                .preview_skill_import(&[("new".into(), incoming.clone())], &BTreeMap::new())
                .is_err()
        );
        assert!(
            manager
                .import_from_local_directory("builtin", &incoming)
                .is_err()
        );
        assert!(
            std::fs::read_to_string(builtin.join("SKILL.md"))
                .unwrap()
                .contains("original")
        );
    }
}
