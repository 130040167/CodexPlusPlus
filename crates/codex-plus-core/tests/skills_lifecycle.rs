//! Public-API regressions for Skill lifecycle data preservation.
use codex_plus_core::skills::SkillsManager;

#[test]
fn disabling_a_copied_installation_must_preserve_user_edits() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let backups = temp.path().join("backups");
    let home = temp.path().join("home");
    let state = temp.path().join("skills.json");
    let manager = SkillsManager::new(&source, &backups, &state, &home);
    let local = home.join("skills/alpha");
    std::fs::create_dir_all(source.join("alpha")).unwrap();
    std::fs::create_dir_all(&local).unwrap();
    std::fs::write(source.join("alpha/SKILL.md"), "original source").unwrap();
    std::fs::write(local.join("SKILL.md"), "user edits").unwrap();
    std::fs::write(&state, r#"{"repos":[],"installed":{"alpha":{"id":"alpha","repoKey":"acme/kit@main","repoPath":"alpha","contentHash":"old"}}}"#).unwrap();
    manager.set_enabled("alpha", false).unwrap();
    let mut preserved = std::fs::read_to_string(source.join("alpha/SKILL.md")).unwrap() == "user edits";
    for backup in manager.list_backups() {
        preserved |= std::fs::read_to_string(backups.join(backup.id).join("SKILL.md")).unwrap() == "user edits";
    }
    assert!(preserved, "disable succeeded but removed the only copy of user edits; source is unchanged and backups are empty");
    assert!(!local.exists());
    assert_eq!(std::fs::read_to_string(source.join("alpha/SKILL.md")).unwrap(), "user edits");
    let installed = &manager.load_state().installed["alpha"];
    assert_eq!(installed.repo_key, "acme/kit@main");
    assert_eq!(installed.repo_path, "alpha");
    assert_eq!(installed.content_hash, "old");
    manager.set_enabled("alpha", true).unwrap();
    assert_eq!(std::fs::read_to_string(local.join("SKILL.md")).unwrap(), "user edits");
    std::fs::write(local.join("SKILL.md"), "more edits").unwrap();
    manager.set_enabled("alpha", true).unwrap();
    assert_eq!(std::fs::read_to_string(local.join("SKILL.md")).unwrap(), "more edits");
}

#[test]
#[cfg(windows)]
fn failed_uninstall_must_not_leave_stale_installed_state() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let backups = temp.path().join("backups");
    let home = temp.path().join("home");
    let state = temp.path().join("skills.json");
    let manager = SkillsManager::new(&source, &backups, &state, &home);
    let local = home.join("skills/alpha");
    std::fs::create_dir_all(&local).unwrap();
    std::fs::write(local.join("SKILL.md"), "user original").unwrap();
    std::fs::write(&state, r#"{"repos":[],"installed":{"alpha":{"id":"alpha"}}}"#).unwrap();
    let mut permissions = std::fs::metadata(&state).unwrap().permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(&state, permissions).unwrap();
    let result = manager.uninstall("alpha");
    let mut permissions = std::fs::metadata(&state).unwrap().permissions();
    permissions.set_readonly(false);
    std::fs::set_permissions(&state, permissions).unwrap();
    assert!(result.is_err(), "readonly state must reject persistence");
    assert!(manager.load_state().installed.contains_key("alpha"));
    assert!(local.is_dir(), "failed uninstall removed the installation but retained its installed record; original contents only remain in backup");
    assert_eq!(std::fs::read_to_string(local.join("SKILL.md")).unwrap(), "user original");
    assert!(manager.list_backups().is_empty());
}

#[test]
#[cfg(windows)]
fn failed_local_disable_must_not_hide_the_skill() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let backups = temp.path().join("backups");
    let home = temp.path().join("home");
    let state = temp.path().join("skills.json");
    let manager = SkillsManager::new(&source, &backups, &state, &home);
    let local = home.join("skills/alpha");
    std::fs::create_dir_all(&local).unwrap();
    std::fs::write(local.join("SKILL.md"), "user original").unwrap();
    std::fs::write(&state, r#"{"repos":[],"installed":{}}"#).unwrap();
    assert!(manager.local_inventory().iter().any(|skill| skill.id == "alpha"));
    let mut permissions = std::fs::metadata(&state).unwrap().permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(&state, permissions).unwrap();
    let result = manager.set_enabled("alpha", false);
    let mut permissions = std::fs::metadata(&state).unwrap().permissions();
    permissions.set_readonly(false);
    std::fs::set_permissions(&state, permissions).unwrap();
    assert!(result.is_err(), "readonly state must reject persistence");
    assert!(manager.local_inventory().iter().any(|skill| skill.id == "alpha"), "failed disable moved the only local directory to source without registering it; inventory and backups no longer expose the skill");
    assert_eq!(std::fs::read_to_string(local.join("SKILL.md")).unwrap(), "user original");
    assert!(!source.join("alpha").exists());
    assert!(manager.list_backups().is_empty());
}

#[test]
#[cfg(windows)]
fn failed_copy_disable_restores_both_directories_and_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let backups = temp.path().join("backups");
    let home = temp.path().join("home");
    let state = temp.path().join("skills.json");
    let manager = SkillsManager::new(&source, &backups, &state, &home);
    let local = home.join("skills/alpha");
    std::fs::create_dir_all(&local).unwrap();
    std::fs::create_dir_all(source.join("alpha")).unwrap();
    std::fs::write(local.join("SKILL.md"), "user edits").unwrap();
    std::fs::write(source.join("alpha/SKILL.md"), "old source").unwrap();
    std::fs::write(&state, r#"{"repos":[],"installed":{"alpha":{"id":"alpha","repoKey":"acme/kit@main","repoPath":"alpha"}}}"#).unwrap();
    let before = manager.load_state();
    let mut permissions = std::fs::metadata(&state).unwrap().permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(&state, permissions).unwrap();
    let result = manager.set_enabled("alpha", false);
    let mut permissions = std::fs::metadata(&state).unwrap().permissions();
    permissions.set_readonly(false);
    std::fs::set_permissions(&state, permissions).unwrap();
    assert!(result.is_err());
    assert_eq!(manager.load_state(), before);
    assert_eq!(std::fs::read_to_string(local.join("SKILL.md")).unwrap(), "user edits");
    assert_eq!(std::fs::read_to_string(source.join("alpha/SKILL.md")).unwrap(), "old source");
    assert!(manager.list_backups().is_empty());
}

#[test]
#[cfg(windows)]
fn failed_second_backup_restores_the_first_uninstall_move() {
    use std::os::windows::fs::OpenOptionsExt;
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let backups = temp.path().join("backups");
    let home = temp.path().join("home");
    let state = temp.path().join("skills.json");
    let manager = SkillsManager::new(&source, &backups, &state, &home);
    let local = home.join("skills/alpha");
    std::fs::create_dir_all(&local).unwrap();
    std::fs::create_dir_all(source.join("alpha")).unwrap();
    std::fs::write(local.join("SKILL.md"), "local edits").unwrap();
    std::fs::write(source.join("alpha/SKILL.md"), "source content").unwrap();
    std::fs::write(&state, r#"{"repos":[],"installed":{"alpha":{"id":"alpha"}}}"#).unwrap();
    let before = manager.load_state();
    // 不共享删除权限的目录句柄模拟 Windows 文件占用，阻止第二次移动。
    let occupied = std::fs::OpenOptions::new().read(true).share_mode(3)
        .custom_flags(0x02000000).open(source.join("alpha")).unwrap();
    let result = manager.uninstall("alpha");
    drop(occupied);
    assert!(result.is_err());
    assert_eq!(manager.load_state(), before);
    assert_eq!(std::fs::read_to_string(local.join("SKILL.md")).unwrap(), "local edits");
    assert_eq!(std::fs::read_to_string(source.join("alpha/SKILL.md")).unwrap(), "source content");
    assert!(manager.list_backups().is_empty());
}
