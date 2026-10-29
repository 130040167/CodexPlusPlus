use super::*;

#[test]
fn native_provider_label_does_not_block_relay_evidence() {
    let providers = BTreeMap::from([("supplier-a".into(), "Relay A".into())]);
    assert_eq!(usage_provider("gpt-model", "openai", &providers), "");
    assert_eq!(usage_provider("gpt-model", "OpenAI", &providers), "");
    assert_eq!(
        usage_provider("supplier-a/gpt-model", "openai", &providers),
        "supplier-a"
    );
    assert_eq!(
        usage_provider("gpt-model", "supplier-a", &providers),
        "supplier-a"
    );
    let explicit = BTreeMap::from([("openai".into(), "Explicit provider".into())]);
    assert_eq!(usage_provider("gpt-model", "openai", &explicit), "openai");
}

#[test]
fn native_provider_label_accepts_exact_event_evidence_without_changing_usage() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("request-log.jsonl");
    write(
        temp.path(),
        SESSION,
        &[
            row(
                "session_meta",
                &time(0),
                json!({"id":SESSION,"model_provider":"openai"}),
            ),
            row("turn_context", &time(1), json!({"model":"gpt"})),
            token(&time(10), 100, 100),
        ],
        false,
    );
    let home = fs::canonicalize(temp.path()).unwrap();
    let mut index = Index::default();
    let baseline = index.update_with_request_log(&home, &log);
    assert_eq!(baseline.events[0].provider, "");
    write_provider_records(
        &temp.path().join("usage-provider-evidence.json"),
        vec![provider_record(&baseline.events[0])],
    );
    let recovered = index.update_with_request_log(&home, &log);
    assert_eq!(recovered.events[0].provider, "supplier-a");
    assert_eq!(recovered.events[0].delta, baseline.events[0].delta);
    assert_eq!(recovered.events[0].total, baseline.events[0].total);
    assert_eq!(
        index.files.values().next().unwrap().events[0].provider,
        "openai"
    );
    let warm = index.update_with_request_log(&home, &log);
    assert_eq!(warm.events[0].provider, "supplier-a");
    assert_eq!(warm.bytes_read, 0);
    assert_eq!(warm.provider_evidence["fileBytesRead"], 0);
}

#[test]
fn user_supplier_classification_is_explicit_bounded_and_never_overrides_known() {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        SESSION,
        &[header(), model(), token(&time(10), 10, 10)],
        false,
    );
    let mut snapshot = snapshot(temp.path());
    let original = snapshot.events[0].clone();
    let mut record: ProviderEvidence = serde_json::from_value(json!({
        "eventId": original.id.iter().map(|b|format!("{b:02x}")).collect::<String>(),
        "session":original.session,"timestamp":original.timestamp,"model":original.model,"role":original.role,
        "input":original.delta.input_tokens,"output":original.delta.output_tokens,
        "cached":original.delta.cached_input_tokens,"provider":"supplier-b","providerName":"Supplier B",
        "source":"user-provider-classification"
    })).unwrap();
    assert!(record.valid());
    record.provider = "custom".into();
    assert!(!record.valid());
    record.provider = "supplier-b".into();
    record.source = "guessed".into();
    assert!(!record.valid());
    record.source = "user-provider-classification".into();
    let cache = ProviderEvidenceCache {
        records: HashMap::from([(original.id, record)]),
        ..Default::default()
    };
    snapshot.events[0].provider = "custom".into();
    let stats = cache.attribute(&mut snapshot.events, &mut snapshot.provider_names);
    assert_eq!(stats["userClassifiedEvents"], 1);
    assert_eq!(stats["userClassifiedTokens"], original.delta.total_tokens);
    assert_eq!(stats["verifiedEvents"], 0);
    assert_eq!(stats["verifiedTokens"], 0);
    assert_eq!(snapshot.events[0].delta, original.delta);
    snapshot.events[0].provider = "supplier-a".into();
    let stats = cache.attribute(&mut snapshot.events, &mut snapshot.provider_names);
    assert_eq!(stats["userClassifiedEvents"], 0);
    assert_eq!(stats["conflictedEvents"], 1);
    assert_eq!(snapshot.events[0].provider, "supplier-a");
    snapshot.events[0].provider = "custom".into();
    snapshot.events[0].delta.input_tokens += 1;
    let stats = cache.attribute(&mut snapshot.events, &mut snapshot.provider_names);
    assert_eq!(stats["signatureMismatches"], 1);
    assert_eq!(snapshot.events[0].provider, "custom");
}

const CHILD: &str = "01a15000-0000-7000-8000-000000000081";
const GRANDCHILD: &str = "01a15000-0000-7000-8000-000000000082";
const FORK: &str = "01a15000-0000-7000-8000-000000000083";
const ALIAS: &str = "01a15000-0000-7000-8000-000000000084";
const UNKNOWN: &str = "01a15000-0000-7000-8000-000000000085";

fn session_header(id: &str, parent: Option<&str>) -> Value {
    let mut value = row("session_meta", &time(0), json!({"id":id}));
    if let Some(parent) = parent {
        value["payload"]["source"] =
            json!({"subagent":{"thread_spawn":{"parent_thread_id":parent}}});
    }
    value
}

fn snapshot(home: &Path) -> Snapshot {
    let home = fs::canonicalize(home).unwrap();
    Index::default().update_with_request_log(&home, &home.join("missing"))
}

fn total(days: &Value) -> u64 {
    days.as_array()
        .unwrap()
        .iter()
        .map(|day| day["usage"]["totalTokens"].as_u64().unwrap())
        .sum()
}

#[test]
fn scoped_usage_descendants_aliases_unknown_and_global_unchanged() {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        SESSION,
        &[
            session_header(SESSION, None),
            model(),
            token(&time(10), 100, 100),
        ],
        false,
    );
    write(
        temp.path(),
        ALIAS,
        &[
            session_header(CHILD, Some(SESSION)),
            model(),
            token(&time(20), 40, 40),
        ],
        false,
    );
    write(
        temp.path(),
        GRANDCHILD,
        &[
            session_header(GRANDCHILD, Some(ALIAS)),
            model(),
            token(&time(30), 20, 20),
        ],
        false,
    );
    let mut fork = session_header(FORK, None);
    fork["payload"]["forked_from_id"] = json!(SESSION);
    fork["payload"]["parent_thread_id"] = json!(SESSION);
    write(
        temp.path(),
        FORK,
        &[fork, model(), token(&time(40), 80, 80)],
        false,
    );
    write(
        temp.path(),
        OTHER,
        &[
            session_header(OTHER, None),
            model(),
            token(&time(50), 60, 60),
        ],
        false,
    );
    let snapshot = snapshot(temp.path());
    let query = options();
    let before = snapshot.response(&query, false);
    let scoped = snapshot.scoped_usage(SESSION, &query, None);
    assert_eq!(scoped["sessionIds"], json!([SESSION, CHILD, GRANDCHILD]));
    assert_eq!(total(&scoped["days"]), 160);
    assert_eq!(total(&before["days"]), 300);
    assert_eq!(snapshot.session_info[CHILD]["parentSessionId"], SESSION);
    assert_eq!(snapshot.session_info[GRANDCHILD]["parentSessionId"], CHILD);
    assert!(snapshot.session_info[FORK].get("parentSessionId").is_none());
    assert_eq!(
        snapshot.scoped_usage(ALIAS, &query, None)["sessionId"],
        CHILD
    );
    assert_eq!(
        total(&snapshot.scoped_usage(ALIAS, &query, None)["days"]),
        60
    );
    let unknown = snapshot.scoped_usage(UNKNOWN, &query, None);
    assert_eq!(unknown["sessionId"], UNKNOWN);
    assert_eq!(unknown["sessionIds"], json!([]));
    assert_eq!(unknown["days"], json!([]));
    let mut response = snapshot.response(&query, false);
    response["scopedUsage"] = scoped;
    response.as_object_mut().unwrap().remove("scopedUsage");
    assert_eq!(response, before);
}

#[test]
fn scoped_usage_accepts_top_level_parent_for_explicit_subagent_only() {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        SESSION,
        &[
            session_header(SESSION, None),
            model(),
            token(&time(10), 100, 100),
        ],
        false,
    );
    write(
        temp.path(),
        CHILD,
        &[
            row(
                "session_meta",
                &time(15),
                json!({
                    "id": CHILD,
                    "parent_thread_id": SESSION,
                    "source": {"subagent": {}}
                }),
            ),
            model(),
            token(&time(20), 40, 40),
        ],
        false,
    );
    let snapshot = snapshot(temp.path());
    assert_eq!(snapshot.session_info[CHILD]["role"], "subagent");
    assert_eq!(snapshot.session_info[CHILD]["parentSessionId"], SESSION);
    let scoped = snapshot.scoped_usage(SESSION, &options(), None);
    assert_eq!(scoped["sessionIds"], json!([SESSION, CHILD]));
    assert_eq!(total(&scoped["days"]), 140);
}

#[test]
fn scoped_usage_conflicts_and_cycles_fail_closed_without_losing_global_tokens() {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        SESSION,
        &[
            session_header(SESSION, None),
            model(),
            token(&time(10), 100, 100),
        ],
        false,
    );
    write(
        temp.path(),
        CHILD,
        &[
            session_header(CHILD, Some(SESSION)),
            model(),
            token(&time(20), 40, 40),
        ],
        false,
    );
    write(
        temp.path(),
        &format!("duplicate-{CHILD}"),
        &[session_header(CHILD, Some(OTHER))],
        false,
    );
    write(
        temp.path(),
        GRANDCHILD,
        &[
            session_header(GRANDCHILD, Some(CHILD)),
            model(),
            token(&time(30), 20, 20),
        ],
        false,
    );
    write(
        temp.path(),
        FORK,
        &[
            session_header(FORK, Some(ALIAS)),
            model(),
            token(&time(40), 80, 80),
        ],
        false,
    );
    write(
        temp.path(),
        ALIAS,
        &[
            session_header(ALIAS, Some(FORK)),
            model(),
            token(&time(50), 60, 60),
        ],
        false,
    );
    write(
        temp.path(),
        OTHER,
        &[
            session_header(OTHER, Some(OTHER)),
            model(),
            token(&time(60), 10, 10),
        ],
        false,
    );
    let snapshot = snapshot(temp.path());
    let query = options();
    assert_eq!(
        snapshot.scoped_usage(SESSION, &query, None)["sessionIds"],
        json!([SESSION])
    );
    assert_eq!(
        snapshot.scoped_usage(FORK, &query, None)["sessionIds"],
        json!([FORK])
    );
    assert_eq!(
        snapshot.scoped_usage(OTHER, &query, None)["sessionIds"],
        json!([OTHER])
    );
    for id in [CHILD, FORK, ALIAS, OTHER] {
        assert!(
            snapshot.session_info[id].get("parentSessionId").is_none(),
            "{id}"
        );
    }
    assert_eq!(total(&snapshot.response(&query, false)["days"]), 310);
}

#[test]
fn scoped_usage_date_and_half_open_minute_bounds_share_day_shape() {
    let temp = tempfile::tempdir().unwrap();
    let today = Local::now().date_naive();
    let yesterday = today - Days::days(1);
    let start = today
        .and_hms_opt(12, 0, 0)
        .unwrap()
        .and_local_timezone(Local)
        .single()
        .unwrap();
    let end = start + Days::minutes(1);
    let timestamp = |at: DateTime<Local>| at.to_rfc3339();
    write(
        temp.path(),
        SESSION,
        &[
            session_header(SESSION, None),
            model(),
            token(&timestamp(start - Days::days(1)), 10, 10),
            token(&timestamp(start - Days::milliseconds(1)), 30, 20),
            token(&timestamp(start), 60, 30),
            token(&timestamp(end - Days::milliseconds(1)), 100, 40),
            token(&timestamp(end), 150, 50),
        ],
        false,
    );
    let snapshot = snapshot(temp.path());
    let query = Query::parse(
        &json!({"from":today.to_string(),"to":today.to_string()}),
        today,
    )
    .unwrap();
    let bounds = Some((start.timestamp_millis(), end.timestamp_millis()));
    let scoped = snapshot.scoped_usage(SESSION, &query, bounds);
    let global = snapshot.response_page(&query, false, 1, None, bounds);
    assert_eq!(scoped["days"], global["days"]);
    assert_eq!(total(&scoped["days"]), 70);
    assert_eq!(
        total(&snapshot.scoped_usage(SESSION, &query, None)["days"]),
        140
    );
    let yesterday_query = Query::parse(
        &json!({"from":yesterday.to_string(),"to":yesterday.to_string()}),
        today,
    )
    .unwrap();
    assert_eq!(
        total(&snapshot.scoped_usage(SESSION, &yesterday_query, None)["days"]),
        10
    );
    assert_eq!(
        snapshot.scoped_usage(SESSION, &yesterday_query, bounds)["days"],
        json!([])
    );
}

#[test]
fn scoped_usage_invalid_ids_are_rejected_before_index_access() {
    let temp = tempfile::tempdir().unwrap();
    for invalid in [
        Value::Null,
        json!(""),
        json!("not-a-session"),
        json!(42),
        json!({}),
        json!([]),
    ] {
        let response = query(temp.path(), &options(), &json!({"scopeSessionId":invalid}));
        assert_eq!(response["status"], "unavailable");
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
    }
}

#[test]
fn scoped_usage_repeated_headers_normalize_aliases_and_block_missing_or_invalid_parent() {
    let temp = tempfile::tempdir().unwrap();
    write(
        temp.path(),
        ALIAS,
        &[
            session_header(SESSION, None),
            model(),
            token(&time(10), 100, 100),
        ],
        false,
    );
    write(
        temp.path(),
        CHILD,
        &[
            session_header(CHILD, Some(ALIAS)),
            session_header(CHILD, Some(SESSION)),
            model(),
            token(&time(20), 40, 40),
        ],
        false,
    );
    write(
        temp.path(),
        GRANDCHILD,
        &[
            session_header(GRANDCHILD, Some(SESSION)),
            session_header(GRANDCHILD, None),
            model(),
            token(&time(30), 20, 20),
        ],
        false,
    );
    write(
        temp.path(),
        FORK,
        &[
            session_header(FORK, Some(SESSION)),
            session_header(FORK, Some("invalid")),
            model(),
            token(&time(40), 80, 80),
        ],
        false,
    );
    let snapshot = snapshot(temp.path());
    let scoped = snapshot.scoped_usage(ALIAS, &options(), None);
    assert_eq!(scoped["sessionId"], SESSION);
    assert_eq!(scoped["sessionIds"], json!([SESSION, CHILD]));
    assert_eq!(total(&scoped["days"]), 140);
    for id in [GRANDCHILD, FORK] {
        assert!(snapshot.session_info[id].get("parentSessionId").is_none());
    }
}

#[test]
fn usage_contract_keeps_model_provider_role_and_parent_fields() {
    let temp = tempfile::tempdir().unwrap();
    let child = CHILD;
    write(
        temp.path(),
        SESSION,
        &[
            row(
                "session_meta",
                &time(0),
                json!({"id":SESSION,"model_provider":"supplier-a"}),
            ),
            row(
                "turn_context",
                &time(1),
                json!({"model":"gpt-6.1-sol","model_provider":"supplier-a"}),
            ),
            token(&time(10), 100, 100),
        ],
        false,
    );
    write(
        temp.path(),
        child,
        &[
            session_header(child, Some(SESSION)),
            row(
                "turn_context",
                &time(1),
                json!({"model":"gpt-6.1-sol","model_provider":"supplier-a"}),
            ),
            token(&time(20), 40, 40),
        ],
        false,
    );

    let mut snapshot = snapshot(temp.path());
    snapshot
        .provider_names
        .insert("supplier-a".into(), "Supplier A".into());
    let response = snapshot.response(&options(), false);
    assert_eq!(response["accountingVersion"], 2);
    let breakdowns = response["days"][0]["breakdowns"].as_array().unwrap();
    assert_eq!(breakdowns.len(), 2);
    for role in ["main", "subagent"] {
        let row = breakdowns.iter().find(|row| row["role"] == role).unwrap();
        assert_eq!(row["model"], "gpt-6.1-sol");
        assert_eq!(row["provider"], "supplier-a");
        assert_eq!(row["providerName"], "Supplier A");
        assert_eq!(row["role"], role);
        assert_eq!(row["recordCount"], 1);
        assert_eq!(
            row["usage"]["totalTokens"].as_u64(),
            Some(if role == "main" { 100 } else { 40 })
        );
    }
    assert_eq!(response["sessionInfo"][child]["parentSessionId"], SESSION);
}

#[test]
fn scoped_usage_query_preserves_global_tools_cursors_and_activity_revision() {
    let temp = tempfile::tempdir().unwrap();
    for (id, parent, tokens) in [
        (SESSION, None, 100),
        (CHILD, Some(SESSION), 40),
        (OTHER, None, 60),
    ] {
        write(
            temp.path(),
            id,
            &[
                session_header(id, parent),
                model(),
                token(&time(10), tokens, tokens),
                row(
                    "response_item",
                    &time(20),
                    json!({"type":"function_call","call_id":"call",
                "name":"mcp__scope__inspect","arguments":"{}"}),
                ),
                row(
                    "response_item",
                    &time(30),
                    json!({"type":"function_call_output","call_id":"call",
                "output":"ok"}),
                ),
            ],
            false,
        );
    }
    let home = fs::canonicalize(temp.path()).unwrap();
    let options = options();
    let request = json!({"scopeSessionId":format!("local:{}", SESSION.to_uppercase())});
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut scoped = loop {
        let response = query(&home, &options, &request);
        if response["scan"]["ready"] == true && response["scan"]["processing"] == false {
            break response;
        }
        assert!(
            Instant::now() < deadline,
            "test rollout index did not become ready"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(scoped["scopedUsage"]["sessionId"], SESSION);
    assert_eq!(scoped["scopedUsage"]["sessionIds"], json!([SESSION, CHILD]));
    assert_eq!(total(&scoped["scopedUsage"]["days"]), 140);
    assert_eq!(total(&scoped["days"]), 200);
    assert_eq!(scoped["activityRecords"].as_array().unwrap().len(), 3);
    assert_eq!(scoped["sessionCursors"].as_array().unwrap().len(), 3);
    let scoped_usage = scoped
        .as_object_mut()
        .unwrap()
        .remove("scopedUsage")
        .unwrap();
    assert_eq!(scoped, query(&home, &options, &json!({})));
    let unchanged = query(
        &home,
        &options,
        &json!({
            "scopeSessionId":SESSION,"activityRevision":scoped["updatedAt"],
        }),
    );
    assert_eq!(unchanged["activityPagination"]["unchanged"], true);
    assert_eq!(unchanged["activityRecords"], json!([]));
    assert_eq!(unchanged["scopedUsage"], scoped_usage);
}
