use codex_plus_core::settings::{BackendSettings, SettingsStore};
use serde_json::json;

#[test]
fn native_fast_unlock_requires_explicit_opt_in() {
    let defaults = BackendSettings::default();
    assert!(!defaults.codex_app_native_fast_unlock);
    assert_eq!(
        serde_json::to_value(defaults).unwrap()["codexAppNativeFastUnlock"],
        false
    );
    let legacy: BackendSettings =
        serde_json::from_value(json!({ "codexAppServiceTierControls": true })).unwrap();
    assert!(!legacy.codex_app_native_fast_unlock);

    let imported: BackendSettings = serde_json::from_value(json!({
        "codexAppNativeFastUnlock": true,
        "codexAppServiceTierControls": false,
    }))
    .unwrap();
    assert!(imported.codex_app_native_fast_unlock);
    assert!(!imported.codex_app_service_tier_controls);
}

#[test]
fn native_fast_unlock_persists_without_changing_service_tier_controls() {
    let temp = tempfile::tempdir().unwrap();
    let store = SettingsStore::new(temp.path().join("settings.json"));
    store.save(&BackendSettings::default()).unwrap();
    let settings = store
        .update(json!({ "codexAppNativeFastUnlock": true }))
        .unwrap();
    assert!(settings.codex_app_native_fast_unlock);
    assert!(!settings.codex_app_service_tier_controls);
    assert!(store.load().unwrap().codex_app_native_fast_unlock);

    let settings = store
        .update(json!({ "codexAppServiceTierControls": true }))
        .unwrap();
    assert!(settings.codex_app_native_fast_unlock);
    let settings = store
        .update(json!({ "codexAppNativeFastUnlock": false }))
        .unwrap();
    assert!(!settings.codex_app_native_fast_unlock);
    assert!(settings.codex_app_service_tier_controls);
    assert!(!store.load().unwrap().codex_app_native_fast_unlock);
}
