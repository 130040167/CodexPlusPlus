use super::*;

#[test]
fn captured_proxy_matches_late_rollout_with_alias_and_never_creates_usage() {
    let temp = tempfile::tempdir().unwrap();
    let mut db = provider_cache::ProviderCache::default();
    db.open(
        temp.path(),
        Local::now().date_naive() - Days::days(RETENTION_DAYS - 1),
    )
    .unwrap();
    let mut request = evidence(request_evidence_json("captured-before-rollout"));
    request.thread_id = OTHER.into();
    assert_eq!(db.capture_proxy(&[request.clone()]).unwrap(), 1);
    assert_eq!(db.capture_proxy(&[request]).unwrap(), 0);
    let aliases = BTreeMap::from([(OTHER.into(), SESSION.into())]);
    let mut events = vec![request_event(SESSION, "", 10)];
    let before = events[0].delta;
    let mut projection = RequestEvidenceCache::default();
    assert_eq!(
        projection.supplement_captured(&events, &aliases, &db),
        (1, false)
    );
    assert_eq!(
        projection.attribute_with_cache(&mut events, &mut BTreeMap::new(), &aliases, &db)["recoveredEvents"],
        1
    );
    assert_eq!(events[0].provider, "supplier-a");
    assert_eq!(events[0].delta, before);
    assert_eq!(events.len(), 1);
    projection.refresh(&temp.path().join("missing-log"));
    assert!(projection.records.is_empty());
    assert_eq!(
        projection.supplement_captured(&[], &aliases, &db),
        (0, false)
    );
}

#[test]
fn captured_proxy_reused_or_duplicate_ids_cannot_regain_trust() {
    let temp = tempfile::tempdir().unwrap();
    let mut db = provider_cache::ProviderCache::default();
    db.open(
        temp.path(),
        Local::now().date_naive() - Days::days(RETENTION_DAYS - 1),
    )
    .unwrap();
    let good = evidence(request_evidence_json("captured-reused"));
    db.capture_proxy(&[good.clone()]).unwrap();
    let mut changed = good.clone();
    changed.relay_id = "other-supplier".into();
    db.capture_proxy(&[changed]).unwrap();
    db.capture_proxy(&[good.clone()]).unwrap();
    let duplicates = evidence(request_evidence_json("captured-duplicate"));
    db.capture_proxy(&[duplicates.clone(), duplicates]).unwrap();
    let rows = db
        .captured_proxy(SESSION, good.timestamp_ms - 1000, good.timestamp_ms + 1000)
        .unwrap()
        .rows;
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r.keys().is_empty()));
}

fn proxy_cache(home: &Path) -> provider_cache::ProviderCache {
    let mut db = provider_cache::ProviderCache::default();
    db.open(
        home,
        Local::now().date_naive() - Days::days(RETENTION_DAYS - 1),
    )
    .unwrap();
    db
}

fn proxy_request(id: &str, session: &str, tick: i64) -> RequestEvidence {
    let mut row = evidence(request_evidence_json(id));
    row.thread_id = session.into();
    row.timestamp_ms = DateTime::parse_from_rfc3339(&time(tick))
        .unwrap()
        .timestamp_millis();
    row.started_at_ms = row.timestamp_ms - 1000;
    row
}

fn attribute_proxy(
    projection: &mut RequestEvidenceCache,
    events: &mut [UsageEvent],
    db: &provider_cache::ProviderCache,
) -> Value {
    projection.attribute_with_cache(events, &mut BTreeMap::new(), &BTreeMap::new(), db)
}

#[test]
fn captured_proxy_overflow_discards_whole_millisecond_and_cannot_prove_from_live_subset() {
    let temp = tempfile::tempdir().unwrap();
    let mut db = proxy_cache(temp.path());
    let rows: Vec<_> = (0..=MAX_REQUEST_EVIDENCE)
        .map(|i| proxy_request(&format!("overflow-{i:05}"), SESSION, 10))
        .collect();
    db.capture_proxy(&rows).unwrap();
    let at = rows[0].timestamp_ms;
    let batch = db
        .captured_proxy(SESSION, at - 120_000, at + 120_000)
        .unwrap();
    assert!(batch.complete);
    assert_eq!(batch.floor, at + 1);
    assert!(
        batch.rows.is_empty(),
        "a same-millisecond conflict group must never be split"
    );
    let mut projection = RequestEvidenceCache {
        records: vec![rows[0].clone()],
        ..Default::default()
    };
    let mut events = vec![request_event(SESSION, "", 10)];
    assert!(
        projection
            .supplement_captured(&events, &BTreeMap::new(), &db)
            .1
    );
    assert_eq!(
        attribute_proxy(&mut projection, &mut events, &db)["recoveredEvents"],
        0
    );
    assert!(projection.verified.is_empty());
}

#[test]
fn captured_proxy_overflow_keeps_recent_window_in_same_session() {
    let temp = tempfile::tempdir().unwrap();
    let mut db = proxy_cache(temp.path());
    let mut rows: Vec<_> = (0..=MAX_REQUEST_EVIDENCE)
        .map(|i| proxy_request(&format!("old-{i:05}"), SESSION, 10))
        .collect();
    rows.push(proxy_request("recent", SESSION, 400_010));
    db.capture_proxy(&rows).unwrap();
    let mut projection = RequestEvidenceCache {
        records: vec![rows[0].clone()],
        ..Default::default()
    };
    let mut events = vec![
        request_event(SESSION, "", 10),
        request_event(SESSION, "", 400_010),
    ];
    assert!(
        projection
            .supplement_captured(&events, &BTreeMap::new(), &db)
            .1
    );
    assert_eq!(
        attribute_proxy(&mut projection, &mut events, &db)["recoveredEvents"],
        1
    );
    assert_eq!(events[0].provider, "");
    assert_eq!(events[1].provider, "supplier-a");
}

#[test]
fn captured_proxy_retention_floor_is_per_event_and_preserves_independent_verified() {
    let temp = tempfile::tempdir().unwrap();
    let mut db = proxy_cache(temp.path());
    let old = proxy_request("retained-old", SESSION, 10);
    // 新事件窗口恰从被淘汰毫秒的下一毫秒开始。
    let recent = proxy_request("retained-new", SESSION, 120_011);
    db.capture_proxy(&[old.clone(), recent]).unwrap();
    let mut prior = RequestEvidenceCache {
        records: vec![old.clone()],
        ..Default::default()
    };
    attribute_proxy(&mut prior, &mut [request_event(SESSION, "", 10)], &db);
    assert_eq!(prior.verified.len(), 1);
    let writer =
        rusqlite::Connection::open(temp.path().join("usage-provider-cache.sqlite")).unwrap();
    writer
        .execute(
            "INSERT INTO capture_retention VALUES('completed_proxy_requests',?1)",
            [old.timestamp_ms],
        )
        .unwrap();
    for independent in [false, true] {
        let mut projection = RequestEvidenceCache {
            records: vec![old.clone()],
            verified: if independent {
                prior.verified.clone()
            } else {
                HashMap::new()
            },
            ..Default::default()
        };
        let mut events = vec![
            request_event(SESSION, "", 10),
            request_event(SESSION, "", 120_011),
        ];
        assert!(
            projection
                .supplement_captured(&events, &BTreeMap::new(), &db)
                .1
        );
        attribute_proxy(&mut projection, &mut events, &db);
        assert_eq!(
            events[0].provider,
            if independent { "supplier-a" } else { "" }
        );
        assert_eq!(events[1].provider, "supplier-a");
        assert_eq!(projection.verified.len(), if independent { 2 } else { 1 });
    }
}

#[test]
fn captured_proxy_rejection_survives_cross_session_time_reuse_restart_and_cache_eviction() {
    for session in [SESSION, OTHER] {
        let temp = tempfile::tempdir().unwrap();
        let mut db = proxy_cache(temp.path());
        let old = proxy_request("reused-across-index", SESSION, 10);
        db.capture_proxy(&[old.clone()]).unwrap();
        let mut prior = RequestEvidenceCache {
            records: vec![old.clone()],
            ..Default::default()
        };
        attribute_proxy(&mut prior, &mut [request_event(SESSION, "", 10)], &db);
        let mut reused = proxy_request(&old.id, session, 400_010);
        reused.relay_id = "supplier-b".into();
        db.capture_proxy(&[reused.clone()]).unwrap();
        drop(db);
        let db = proxy_cache(temp.path());
        let writer =
            rusqlite::Connection::open(temp.path().join("usage-provider-cache.sqlite")).unwrap();
        writer
            .execute(
                "DELETE FROM completed_proxy_requests WHERE id=?1",
                [&old.id],
            )
            .unwrap();
        let mut projection = RequestEvidenceCache {
            records: vec![reused.clone()],
            ..Default::default()
        };
        let mut events = vec![request_event(session, "", 400_010)];
        projection.supplement_captured(&events, &BTreeMap::new(), &db);
        assert_eq!(
            attribute_proxy(&mut projection, &mut events, &db)["recoveredEvents"],
            0
        );
        assert_eq!(events[0].provider, "");
        prior.records.clear();
        let mut old_events = vec![request_event(SESSION, "", 10)];
        assert_eq!(
            attribute_proxy(&mut prior, &mut old_events, &db)["recoveredEvents"],
            0
        );
        assert_eq!(old_events[0].provider, "");
        assert!(prior.verified.values().all(|saved| saved.rejected));
    }
}

#[test]
fn captured_proxy_checks_live_metadata_before_collector_has_captured_reuse() {
    let temp = tempfile::tempdir().unwrap();
    let mut db = proxy_cache(temp.path());
    let old = proxy_request("not-yet-collected", SESSION, 10);
    db.capture_proxy(&[old.clone()]).unwrap();
    let reused = proxy_request(&old.id, OTHER, 400_010);
    let mut projection = RequestEvidenceCache {
        records: vec![reused.clone()],
        ..Default::default()
    };
    let mut events = vec![request_event(OTHER, "", 400_010)];
    projection.supplement_captured(&events, &BTreeMap::new(), &db);
    assert_eq!(
        attribute_proxy(&mut projection, &mut events, &db)["recoveredEvents"],
        0
    );
    drop(db);
    let db = proxy_cache(temp.path());
    assert!(db.rejected_proxy_ids([&old]).unwrap().contains(&old.id));
}

#[test]
fn captured_proxy_checks_verified_ids_at_each_attribution_not_only_supplement() {
    let temp = tempfile::tempdir().unwrap();
    let mut db = proxy_cache(temp.path());
    let old = proxy_request("verified-only", SESSION, 10);
    db.capture_proxy(&[old.clone()]).unwrap();
    let mut projection = RequestEvidenceCache {
        records: vec![old.clone()],
        ..Default::default()
    };
    attribute_proxy(&mut projection, &mut [request_event(SESSION, "", 10)], &db);
    projection.records.clear();
    let mut events = vec![request_event(SESSION, "", 10)];
    projection.supplement_captured(&events, &BTreeMap::new(), &db);
    projection.records.clear();
    db.capture_proxy(&[proxy_request(&old.id, OTHER, 400_010)])
        .unwrap();
    assert_eq!(
        attribute_proxy(&mut projection, &mut events, &db)["recoveredEvents"],
        0
    );
    assert!(projection.verified.values().all(|saved| saved.rejected));
}

#[test]
fn captured_proxy_supplement_rotates_past_256_sessions() {
    let temp = tempfile::tempdir().unwrap();
    let mut db = proxy_cache(temp.path());
    let rows: Vec<_> = (0..257)
        .map(|i| {
            let session = format!("proxy-session-{i:03}");
            proxy_request(&format!("request-{i:03}"), &session, 10)
        })
        .collect();
    db.capture_proxy(&rows).unwrap();
    let original: Vec<_> = rows
        .iter()
        .map(|row| request_event(&row.thread_id, "", 10))
        .collect();
    let mut projection = RequestEvidenceCache::default();
    let mut events = original.clone();
    projection.supplement_captured(&events, &BTreeMap::new(), &db);
    assert_eq!(
        attribute_proxy(&mut projection, &mut events, &db)["recoveredEvents"],
        256
    );
    assert_eq!(events[256].provider, "");
    events.clone_from(&original);
    projection.supplement_captured(&events, &BTreeMap::new(), &db);
    attribute_proxy(&mut projection, &mut events, &db);
    assert!(events.iter().all(|event| event.provider == "supplier-a"));
    assert_eq!(projection.verified.len(), 257);
}

#[test]
fn captured_proxy_unavailable_id_check_preserves_but_does_not_apply_verified() {
    let temp = tempfile::tempdir().unwrap();
    let db = proxy_cache(temp.path());
    let row = proxy_request("check-unavailable", SESSION, 10);
    let mut projection = RequestEvidenceCache {
        records: vec![row],
        ..Default::default()
    };
    attribute_proxy(&mut projection, &mut [request_event(SESSION, "", 10)], &db);
    assert_eq!(projection.verified.len(), 1);
    let mut events = vec![request_event(SESSION, "", 10)];
    let unavailable = provider_cache::ProviderCache::default();
    assert_eq!(
        attribute_proxy(&mut projection, &mut events, &unavailable)["recoveredEvents"],
        0
    );
    assert!(projection.verified.values().all(|saved| !saved.rejected));
    assert_eq!(events[0].provider, "");
    assert_eq!(
        attribute_proxy(&mut projection, &mut events, &db)["recoveredEvents"],
        1
    );
}

#[test]
fn capture_transaction_does_not_advance_cursor_on_write_failure() {
    let temp = tempfile::tempdir().unwrap();
    let mut db = provider_cache::ProviderCache::default();
    db.open(temp.path(), Local::now().date_naive()).unwrap();
    db.capture_native("source", 1, &[], true, 0, "first")
        .unwrap();
    let blocker =
        rusqlite::Connection::open(temp.path().join("usage-provider-cache.sqlite")).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE;").unwrap();
    assert!(
        db.capture_native("source", 2, &[], true, 0, "second")
            .is_err()
    );
    assert_eq!(
        db.capture_cursor("source").unwrap(),
        Some((1, "first".into(), 0))
    );
    blocker.execute_batch("ROLLBACK;").unwrap();
    db.capture_native("source", 2, &[], true, 0, "second")
        .unwrap();
    assert_eq!(
        db.capture_cursor("source").unwrap(),
        Some((2, "second".into(), 0))
    );
}
