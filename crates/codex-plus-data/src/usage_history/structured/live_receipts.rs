//! 实时回执只补分类，不产生用量。证据必须逐字段匹配，且不能被两个回执复用。
use super::*;

const MAX_RECEIPTS: usize = 128;
const MAX_MERGED: usize = 8;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Receipt {
    id: String,
    session_id: String,
    timestamp: i64,
    before: Counts,
    after: Counts,
    #[serde(default)]
    model: String,
    #[serde(default)]
    provider: String,
    #[serde(default)]
    request_ids: Vec<String>,
    #[serde(default)]
    request_signatures: BTreeMap<String, String>,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
struct Counts {
    input: u64,
    output: u64,
    cached: u64,
}

impl Receipt {
    fn delta(&self) -> Option<Counts> {
        let counts = Counts {
            input: self.after.input.checked_sub(self.before.input)?,
            output: self.after.output.checked_sub(self.before.output)?,
            cached: self.after.cached.checked_sub(self.before.cached)?,
        };
        (self.timestamp > 0
            && !self.id.is_empty()
            && self.id.len() <= 512
            && !self.session_id.is_empty()
            && self.session_id.len() <= 256
            && self.model.len() <= 256
            && self.provider.len() <= 256
            && self.request_ids.len() <= MAX_MERGED
            && self.request_signatures.len() <= MAX_MERGED
            && self
                .request_ids
                .iter()
                .all(|id| !id.is_empty() && id.len() <= 256)
            && counts.cached <= counts.input
            && counts.input.checked_add(counts.output)? > 0
            && self.before.cached <= self.before.input
            && self.after.cached <= self.after.input)
            .then_some(counts)
    }
}

pub(super) fn match_receipts(
    records: &[RequestEvidence],
    aliases: &BTreeMap<String, String>,
    input: &Value,
) -> Value {
    let Some(rows) = input
        .as_array()
        .filter(|rows| !rows.is_empty() && rows.len() <= MAX_RECEIPTS)
    else {
        return json!([]);
    };
    let aliases = RequestSessionAliases::new(aliases);
    let mut ids: HashMap<&str, &RequestEvidence> = HashMap::new();
    let mut conflicts = HashSet::new();
    for record in records {
        if let Some(previous) = ids.insert(&record.id, record) {
            if previous != record {
                conflicts.insert(record.id.as_str());
            }
        }
    }
    let mut sessions: HashMap<String, Vec<&RequestEvidence>> = HashMap::new();
    for record in ids.values() {
        if let Some(session) = aliases.resolve(&record.thread_id) {
            sessions.entry(session).or_default().push(record);
        }
    }
    for records in sessions.values_mut() {
        records.sort_unstable_by_key(|record| (record.timestamp_ms, &record.id));
    }
    let receipts: Vec<_> = rows
        .iter()
        .map(|row| serde_json::from_value::<Receipt>(row.clone()).ok())
        .collect();
    let mut receipt_ids = HashMap::new();
    for receipt in receipts.iter().flatten() {
        *receipt_ids.entry(&receipt.id).or_insert(0usize) += 1;
    }
    let mut candidates: Vec<(usize, Vec<String>)> = vec![(0, Vec::new()); receipts.len()];
    let mut rejected = HashSet::new();
    let eligible: HashSet<_> = ids
        .values()
        .filter(|record| {
            !conflicts.contains(record.id.as_str())
                && !record.keys().is_empty()
                && record.attempts == Some(1)
                && record.auto_retries == Some(0)
        })
        .map(|record| record.id.as_str())
        .collect();
    // 即使一个回执本身有多个候选，也占用对应证据，避免另一回执误取其子集。
    let mut owners: HashMap<String, Option<usize>> = HashMap::new();
    let claim = |owners: &mut HashMap<String, Option<usize>>, id: &String, index| {
        owners
            .entry(id.clone())
            .and_modify(|owner| {
                if *owner != Some(index) {
                    *owner = None;
                }
            })
            .or_insert(Some(index));
    };
    for (index, receipt) in receipts.iter().enumerate() {
        let Some(receipt) = receipt else {
            continue;
        };
        let Some(delta) = receipt.delta() else {
            continue;
        };
        if !receipt.request_ids.is_empty()
            && (receipt.request_ids.iter().collect::<HashSet<_>>().len()
                != receipt.request_ids.len()
                || receipt.request_signatures.len() != receipt.request_ids.len()
                || receipt.request_ids.iter().any(|id| {
                    receipt.request_signatures.get(id).is_none_or(|value| {
                        value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
                    })
                }))
        {
            rejected.insert(index);
        }
        let Some(session) = aliases.resolve(&receipt.session_id) else {
            if !receipt.request_ids.is_empty() {
                rejected.insert(index);
            }
            continue;
        };
        for id in &receipt.request_ids {
            claim(&mut owners, id, index);
            if let Some(record) = ids.get(id.as_str()) {
                if conflicts.contains(id.as_str())
                    || record.keys().is_empty()
                    || record.attempts != Some(1)
                    || record.auto_retries != Some(0)
                    || aliases.resolve(&record.thread_id).as_deref() != Some(session.as_str())
                    || request_base_model(&record.model) != request_base_model(&receipt.model)
                    || request_base_model(&record.upstream_model)
                        != request_base_model(&receipt.model)
                    || (!receipt.provider.is_empty() && receipt.provider != record.relay_id)
                    || !(0..=DELAYED_REQUEST_MATCH_WINDOW_MS)
                        .contains(&receipt.timestamp.saturating_sub(record.timestamp_ms))
                    || record.timestamp_ms > receipt.timestamp
                    || receipt
                        .request_signatures
                        .get(id)
                        .is_some_and(|saved| saved != &request_signature(record))
                {
                    rejected.insert(index);
                }
            }
        }
        // 轮转后只复核仍存在的旧证据，不允许另组同量请求替换既有证明。
        if !receipt.request_ids.is_empty()
            && receipt
                .request_ids
                .iter()
                .any(|id| !ids.contains_key(id.as_str()))
        {
            continue;
        }
        let Some(records) = sessions.get(&session) else {
            continue;
        };
        let first_end = records.partition_point(|record| {
            record.timestamp_ms < receipt.timestamp.saturating_sub(REQUEST_MATCH_WINDOW_MS)
        });
        let last_end = records.partition_point(|record| record.timestamp_ms <= receipt.timestamp);
        for end in first_end..last_end {
            let last = records[end];
            if !(0..=REQUEST_MATCH_WINDOW_MS)
                .contains(&receipt.timestamp.saturating_sub(last.timestamp_ms))
            {
                continue;
            }
            let mut sum = Counts {
                input: 0,
                output: 0,
                cached: 0,
            };
            let mut model: Option<&str> = None;
            let mut provider: Option<&str> = None;
            let mut request_ids = Vec::new();
            for start in (end.saturating_sub(MAX_MERGED - 1)..=end).rev() {
                let record = records[start];
                if !eligible.contains(record.id.as_str())
                    || !(0..=DELAYED_REQUEST_MATCH_WINDOW_MS)
                        .contains(&receipt.timestamp.saturating_sub(record.timestamp_ms))
                    || (start < end && record.timestamp_ms > records[start + 1].started_at_ms)
                {
                    break;
                }
                let normalized = request_base_model(&record.model);
                if normalized != request_base_model(&record.upstream_model)
                    || model.is_some_and(|previous| previous != normalized)
                    || provider.is_some_and(|previous| previous != record.relay_id)
                {
                    break;
                }
                model = Some(normalized);
                provider = Some(&record.relay_id);
                let usage = record.usage.unwrap();
                let Some(input) = sum.input.checked_add(usage.input_tokens.unwrap()) else {
                    break;
                };
                let Some(output) = sum.output.checked_add(usage.output_tokens.unwrap()) else {
                    break;
                };
                let Some(cached) = sum.cached.checked_add(usage.cached_tokens.unwrap()) else {
                    break;
                };
                sum = Counts {
                    input,
                    output,
                    cached,
                };
                request_ids.push(record.id.clone());
                if sum.input > delta.input || sum.output > delta.output || sum.cached > delta.cached
                {
                    break;
                }
                if sum == delta {
                    for id in &request_ids {
                        claim(&mut owners, id, index);
                    }
                    // 已知模型与请求不一致也保留占用，不能把证据转给另一个回执。
                    if (receipt.model.is_empty()
                        || receipt.model.eq_ignore_ascii_case("unknown")
                        || request_base_model(&receipt.model) == normalized)
                        && (unknown_provider(&receipt.provider)
                            || receipt.provider == record.relay_id)
                    {
                        candidates[index].0 += 1;
                        if candidates[index].0 == 1 {
                            candidates[index].1 = request_ids.clone();
                        }
                        if !receipt.request_ids.is_empty()
                            && receipt.request_ids.iter().collect::<HashSet<_>>()
                                != request_ids.iter().collect::<HashSet<_>>()
                        {
                            rejected.insert(index);
                        }
                    } else {
                        rejected.insert(index);
                    }
                }
            }
        }
    }
    let mut result = Vec::new();
    for (index, (count, candidate_ids)) in candidates.into_iter().enumerate() {
        let Some(receipt) = receipts[index].as_ref() else {
            continue;
        };
        let Some(delta) = receipt.delta() else {
            continue;
        };
        let reused = receipt
            .request_ids
            .iter()
            .chain(candidate_ids.iter())
            .any(|id| owners.get(id).is_some_and(|owner| owner.is_none()));
        let complete_proof_present = !receipt.request_ids.is_empty()
            && receipt
                .request_ids
                .iter()
                .all(|id| ids.contains_key(id.as_str()));
        if rejected.contains(&index)
            || reused
            || count > 1
            || (complete_proof_present && count != 1)
            || receipt_ids[&receipt.id] != 1
        {
            result.push(json!({"id":receipt.id,"sessionId":receipt.session_id,
                "timestamp":receipt.timestamp,"input":delta.input,"output":delta.output,
                "cached":delta.cached,"rejected":true}));
        } else if count == 1 {
            let record = ids[candidate_ids[0].as_str()];
            result.push(json!({
                "id":receipt.id,"sessionId":receipt.session_id,"timestamp":receipt.timestamp,
                "input":delta.input,"output":delta.output,"cached":delta.cached,
                "model":request_base_model(&record.model),
                "provider":record.relay_id,"providerName":record.relay_name,
                "requestIds":candidate_ids,
                "requestSignatures":candidate_ids.iter().map(|id|
                    (id.clone(),request_signature(ids[id.as_str()]))).collect::<BTreeMap<_,_>>()
            }));
        }
    }
    json!(result)
}

fn request_signature(record: &RequestEvidence) -> String {
    // 只散列已脱敏的匹配字段，用于日志部分轮转后复核仍存在的单条请求。
    let digest = Sha256::digest(serde_json::to_vec(record).unwrap_or_default());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(id: &str, at: i64, input: u64, output: u64, cached: u64) -> RequestEvidence {
        RequestEvidence {
            id: id.into(),
            thread_id: "session".into(),
            model: "gpt-6.1-sol".into(),
            upstream_model: "gpt-6.1-sol".into(),
            relay_id: "supplier".into(),
            relay_name: "Supplier".into(),
            success: true,
            state: "success".into(),
            status_code: 200,
            timestamp_ms: at,
            started_at_ms: at - 1000,
            attempts: Some(1),
            auto_retries: Some(0),
            usage: Some(RequestEvidenceUsage {
                input_tokens: Some(input),
                output_tokens: Some(output),
                cached_tokens: Some(cached),
            }),
        }
    }

    fn receipt(id: &str, at: i64, input: u64, output: u64, cached: u64) -> Value {
        json!({"id":id,"sessionId":"session","timestamp":at,
            "before":{"input":0,"output":0,"cached":0},
            "after":{"input":input,"output":output,"cached":cached}})
    }

    fn has_positive(result: &Value) -> bool {
        result
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["rejected"] != true)
    }

    #[test]
    fn exact_actual_merged_receipts_without_rollout_or_recent_limit() {
        let records = vec![
            request("a", 1792648399881, 159745, 148, 154240),
            request("b", 1792648411965, 159949, 206, 154240),
            request("c", 1792648424274, 160211, 209, 154240),
            request("d", 1792648435589, 160597, 65, 154240),
        ];
        let input = json!([
            receipt("one", 1792648414304, 319694, 354, 308480),
            receipt("two", 1792648427335, 160211, 209, 154240),
            receipt("three", 1792648435594, 160597, 65, 154240)
        ]);
        let proofs = match_receipts(&records, &BTreeMap::new(), &input);
        assert_eq!(proofs.as_array().unwrap().len(), 3);
        assert_eq!(proofs[0]["requestIds"], json!(["b", "a"]));
        assert_eq!(
            proofs
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["input"].as_u64().unwrap() + row["output"].as_u64().unwrap())
                .sum::<u64>(),
            641130
        );
    }

    #[test]
    fn rejects_ambiguity_reuse_bad_counts_retry_failure_switch_and_stale_time() {
        let base = request("a", 100000, 10, 2, 3);
        let input = json!([receipt("one", 100010, 10, 2, 3)]);
        assert_eq!(
            match_receipts(&[base.clone()], &BTreeMap::new(), &input)
                .as_array()
                .unwrap()
                .len(),
            1
        );
        for mode in 0..9 {
            let mut records = vec![base.clone()];
            let mut rows = input.clone();
            match mode {
                0 => records.push(request("b", 100001, 10, 2, 3)),
                1 => rows
                    .as_array_mut()
                    .unwrap()
                    .push(receipt("two", 100010, 10, 2, 3)),
                2 => rows[0]["after"]["cached"] = json!(4),
                3 => records[0].auto_retries = Some(1),
                4 => records[0].success = false,
                5 => records[0].upstream_model = "other-model".into(),
                6 => rows[0]["timestamp"] = json!(130001),
                7 => rows[0]["model"] = json!("other-model"),
                _ => {
                    let mut conflict = base.clone();
                    conflict.relay_id = "other".into();
                    records.push(conflict);
                }
            }
            assert!(
                !has_positive(&match_receipts(&records, &BTreeMap::new(), &rows)),
                "mode {mode}"
            );
        }
        for switch in 0..3 {
            let mut second = request("b", 102000, 10, 2, 3);
            match switch {
                0 => {
                    second.model = "other".into();
                    second.upstream_model = "other".into();
                }
                1 => second.relay_id = "other".into(),
                _ => second.success = false,
            }
            assert_eq!(
                match_receipts(
                    &[base.clone(), second],
                    &BTreeMap::new(),
                    &json!([receipt("merged", 102010, 20, 4, 6)])
                ),
                json!([])
            );
        }
    }

    #[test]
    fn trusted_aliases_duplicates_limits_and_cycles() {
        let base = request("a", 100000, 10, 2, 3);
        let mut row = receipt("one", 100010, 10, 2, 3);
        row["sessionId"] = json!("alias");
        let mut aliases = BTreeMap::from([("alias".into(), "session".into())]);
        assert_eq!(
            match_receipts(
                &[base.clone(), base.clone()],
                &aliases,
                &json!([row.clone()])
            )
            .as_array()
            .unwrap()
            .len(),
            1
        );
        aliases.insert("session".into(), "alias".into());
        assert_eq!(
            match_receipts(&[base.clone()], &aliases, &json!([row.clone()])),
            json!([])
        );
        assert_eq!(
            match_receipts(&[base], &BTreeMap::new(), &json!(vec![row; 129])),
            json!([])
        );
    }

    #[test]
    fn persisted_classification_survives_rotation_and_rejects_present_conflicts() {
        let base = request("a", 100000, 10, 2, 3);
        let mut row = receipt("one", 100010, 10, 2, 3);
        row["model"] = json!("gpt-6.1-sol");
        row["provider"] = json!("supplier");
        row["requestIds"] = json!(["a"]);
        row["requestSignatures"] = json!({"a":request_signature(&base)});
        assert_eq!(
            match_receipts(&[], &BTreeMap::new(), &json!([row.clone()])),
            json!([])
        );
        for mode in 0..4 {
            let mut record = base.clone();
            match mode {
                0 => record.usage.as_mut().unwrap().input_tokens = Some(11),
                1 => record.relay_id = "other".into(),
                2 => record.thread_id = "other".into(),
                _ => record.success = false,
            }
            let result = match_receipts(&[record], &BTreeMap::new(), &json!([row.clone()]));
            assert_eq!(result[0]["rejected"], true, "mode {mode}");
        }
        let mut competing = receipt("two", 100010, 10, 2, 3);
        competing["model"] = json!("other");
        let result = match_receipts(&[base], &BTreeMap::new(), &json!([row, competing]));
        assert_eq!(result.as_array().unwrap().len(), 2);
        assert!(!has_positive(&result));
    }

    #[test]
    fn partial_rotation_rechecks_per_request_signature_and_alias_conflict() {
        let first = request("a", 100000, 10, 2, 3);
        let second = request("b", 102000, 20, 3, 4);
        let mut row = receipt("one", 102010, 30, 5, 7);
        let result = match_receipts(
            &[first.clone(), second.clone()],
            &BTreeMap::new(),
            &json!([row.clone()]),
        );
        row["model"] = result[0]["model"].clone();
        row["provider"] = result[0]["provider"].clone();
        row["requestIds"] = result[0]["requestIds"].clone();
        row["requestSignatures"] = result[0]["requestSignatures"].clone();
        assert_eq!(
            match_receipts(&[second.clone()], &BTreeMap::new(), &json!([row.clone()])),
            json!([])
        );
        let mut changed = second;
        changed.usage.as_mut().unwrap().cached_tokens = Some(5);
        assert_eq!(
            match_receipts(&[changed], &BTreeMap::new(), &json!([row.clone()]))[0]["rejected"],
            true
        );
        let aliases = BTreeMap::from([
            ("session".into(), "alias".into()),
            ("alias".into(), "session".into()),
        ]);
        assert_eq!(
            match_receipts(&[], &aliases, &json!([row]))[0]["rejected"],
            true
        );
    }

    #[test]
    fn long_request_known_provider_and_rotated_proof_cannot_be_replaced() {
        let mut long = request("long", 500000, 10, 2, 3);
        long.started_at_ms = 100000;
        let row = receipt("one", 500010, 10, 2, 3);
        assert!(has_positive(&match_receipts(
            &[long.clone()],
            &BTreeMap::new(),
            &json!([row.clone()])
        )));
        let mut known = row.clone();
        known["provider"] = json!("other-supplier");
        assert_eq!(
            match_receipts(&[long.clone()], &BTreeMap::new(), &json!([known]))[0]["rejected"],
            true
        );
        let a = request("a", 100000, 10, 2, 3);
        let b = request("b", 102000, 20, 3, 4);
        let mut merged = receipt("merged", 102010, 30, 5, 7);
        let proof = match_receipts(
            &[a.clone(), b.clone()],
            &BTreeMap::new(),
            &json!([merged.clone()]),
        );
        for key in ["model", "provider", "requestIds", "requestSignatures"] {
            merged[key] = proof[0][key].clone();
        }
        let c = request("c", 102000, 30, 5, 7);
        assert_eq!(
            match_receipts(&[b, c], &BTreeMap::new(), &json!([merged.clone()])),
            json!([])
        );
        merged["requestSignatures"] = json!({});
        assert_eq!(
            match_receipts(&[], &BTreeMap::new(), &json!([merged]))[0]["rejected"],
            true
        );
    }
}
