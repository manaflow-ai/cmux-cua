//! Replays the shared automation lease vectors (same file as the cmux
//! browser host) and checks result, snapshot and frames after every step.

use serde_json::{json, Value};

use super::*;

const VECTORS: &str = include_str!("vectors.json");

fn text(step: &Value, key: &str) -> String {
    step.get(key)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("step {step} has no string `{key}`"))
        .to_owned()
}

fn opt_text(step: &Value, key: &str) -> String {
    step.get(key).and_then(Value::as_str).unwrap_or_default().to_owned()
}

fn identity(step: &Value) -> AgentIdentity {
    AgentIdentity {
        session: text(step, "session"),
        actor: text(step, "actor"),
        on_behalf_of: step.get("on_behalf_of").and_then(Value::as_str).map(str::to_owned),
        origin: text(step, "origin"),
        label: text(step, "label"),
        implicit_session: step.get("implicit_session").and_then(Value::as_bool).unwrap_or(false),
    }
}

fn engine(step: &Value) -> TargetEngine {
    match step.get("engine").and_then(Value::as_str).unwrap_or("headless") {
        "cef" => TargetEngine::Cef,
        "webkit" => TargetEngine::Webkit,
        "desktop" => TargetEngine::Desktop,
        "headless" => TargetEngine::Headless,
        other => panic!("unknown engine {other}"),
    }
}

fn op_from_step(step: &Value) -> LeaseOp {
    let now_ms = || step.get("now_ms").and_then(Value::as_u64).expect("now_ms");
    match text(step, "op").as_str() {
        "acquire" => LeaseOp::Acquire { target: text(step, "target"), engine: engine(step), who: identity(step), now_ms: now_ms() },
        "act" => LeaseOp::Act { target: text(step, "target"), engine: engine(step), who: identity(step), now_ms: now_ms() },
        "observe" => LeaseOp::Observe { target: text(step, "target"), session: text(step, "session") },
        "release" => LeaseOp::Release { target: text(step, "target"), session: text(step, "session") },
        "session_end" => LeaseOp::SessionEnd { session: text(step, "session") },
        "user_input" => LeaseOp::UserInput { target: text(step, "target") },
        "take_over" => LeaseOp::TakeOver { target: text(step, "target"), origin: opt_text(step, "origin") },
        "hand_back" => LeaseOp::HandBack { target: text(step, "target"), origin: opt_text(step, "origin") },
        "stop" => LeaseOp::Stop { target: text(step, "target"), origin: opt_text(step, "origin") },
        "allow" => LeaseOp::Allow { actor: text(step, "actor"), origin: opt_text(step, "origin") },
        other => panic!("unknown op {other}"),
    }
}

fn result_json(result: &Result<(), LeaseError>) -> Value {
    match result {
        Ok(()) => json!({"ok": true}),
        Err(err) => json!({"error": err.code()}),
    }
}

fn snapshot_json(table: &LeaseTable, target: Option<&str>) -> Value {
    let Some(target) = target else { return Value::Null };
    match table.lease(target) {
        None => Value::Null,
        Some(lease) => {
            let mut value = serde_json::to_value(lease).expect("lease serializes");
            value["needs_fresh_observe"] = json!(table.needs_fresh_observe(target));
            value
        }
    }
}

#[test]
fn shared_vectors_replay_identically() {
    let vectors: Value = serde_json::from_str(VECTORS).expect("vectors parse");
    let cases = vectors["cases"].as_array().expect("cases");
    assert!(cases.len() >= 21, "vendored vectors look truncated");
    for case in cases {
        let name = case["name"].as_str().expect("name");
        let mut table = LeaseTable::new();
        for (index, row) in case["steps"].as_array().expect("steps").iter().enumerate() {
            let step = &row["step"];
            let expect = &row["expect"];
            let outcome = table.apply(op_from_step(step));
            let where_ = format!("case `{name}` step {index} ({})", step["op"]);
            assert_eq!(result_json(&outcome.result), expect["result"], "{where_}: result");
            assert_eq!(
                snapshot_json(&table, step.get("target").and_then(Value::as_str)),
                expect["lease"],
                "{where_}: lease snapshot"
            );
            let frames = serde_json::to_value(&outcome.frames).expect("frames serialize");
            assert_eq!(frames, expect["frames"], "{where_}: frames");
        }
    }
}

#[test]
fn error_codes_match_the_contract_list() {
    let all = [
        LeaseError::LeaseHeld,
        LeaseError::PausedByUser,
        LeaseError::UserDriving,
        LeaseError::StaleAfterHandBack,
        LeaseError::StoppedByUser,
        LeaseError::SessionRequired,
        LeaseError::NotLeaseHolder,
        LeaseError::NoLease,
        LeaseError::NotPaused,
        LeaseError::UserOriginRequired,
        LeaseError::AgentOriginRequired,
    ];
    let codes: Vec<&str> = all.iter().map(|err| err.code()).collect();
    assert_eq!(
        codes,
        [
            "lease_held",
            "paused_by_user",
            "user_driving",
            "stale_after_hand_back",
            "stopped_by_user",
            "session_required",
            "not_lease_holder",
            "no_lease",
            "not_paused",
            "user_origin_required",
            "agent_origin_required",
        ]
    );
}

#[test]
fn on_behalf_of_is_kept_and_rendered() {
    let mut table = LeaseTable::new();
    let who = AgentIdentity {
        session: "s1".into(),
        actor: "agent:subagent-7".into(),
        on_behalf_of: Some("agent:chief".into()),
        origin: "mcp".into(),
        label: "fill form".into(),
        implicit_session: false,
    };
    let outcome = table.apply(LeaseOp::Act { target: "w1".into(), engine: TargetEngine::Desktop, who, now_ms: 7 });
    assert_eq!(outcome.result, Ok(()));
    let lease = outcome.frames[0].lease.as_ref().expect("lease frame");
    assert_eq!(lease.on_behalf_of.as_deref(), Some("agent:chief"));
}

#[test]
fn vendored_vectors_match_the_pinned_cmux_file() {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(VECTORS.as_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    assert_eq!(
        hex, VECTORS_SHA256,
        "lease/vectors.json drifted from {VECTORS_SOURCE}; copy the cmux file again and update both constants"
    );
}
