use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;

fn returned(answer: Answer) -> Value {
    match answer.outcome {
        Outcome::Returned(value) => value,
        Outcome::Raised { kind, message } => panic!("raised {kind}: {message}"),
    }
}

#[test]
fn hello_names_the_pinned_commit() {
    let pin = std::fs::read_to_string(repo_root().join("tests").join("rpctl-ref.txt")).unwrap();
    let commit = pin
        .lines()
        .find_map(|l| l.strip_prefix("commit: "))
        .unwrap();
    let hello = Oracle::spawn().unwrap().hello().unwrap();
    let short = hello["rpctl_commit"].as_str().unwrap();
    assert!(commit.starts_with(short), "{short} vs {commit}");
}

#[test]
fn rpctl_function_round_trip() {
    let mut oracle = Oracle::spawn().unwrap();
    let call = |o: &mut Oracle, cmd: &str| {
        returned(
            o.call("approve.destructive_reason", json!([cmd]), json!({}))
                .unwrap(),
        )
    };
    assert_eq!(
        call(&mut oracle, "rm -rf build"),
        json!("recursive/forced delete")
    );
    assert_eq!(
        call(&mut oracle, "git reset --hard"),
        json!("discards uncommitted work")
    );
    assert_eq!(call(&mut oracle, "ls -la"), Value::Null);
}

#[test]
fn tuples_come_back_tagged() {
    let mut oracle = Oracle::spawn().unwrap();
    let answer = oracle
        .call(
            "approve.plain_pipeline",
            json!(["git status && ls | wc -l"]),
            json!({}),
        )
        .unwrap();
    assert_eq!(
        returned(answer),
        tuple(vec![
            json!([["git", "status"], ["ls"], ["wc", "-l"]]),
            json!(["&&", "|"]),
        ])
    );
}

#[test]
fn exceptions_carry_type_and_message() {
    let mut oracle = Oracle::spawn().unwrap();
    let answer = oracle
        .call("shlex.split", json!(["echo \"unclosed"]), json!({}))
        .unwrap();
    assert_eq!(
        answer.outcome,
        Outcome::Raised {
            kind: "ValueError".to_string(),
            message: "No closing quotation".to_string(),
        }
    );
}

#[test]
fn non_ascii_survives_the_pipe() {
    let mut oracle = Oracle::spawn().unwrap();
    let value = json!({"ключ": "值 😀"});
    let ascii = returned(
        oracle
            .call("json.dumps", json!([value]), json!({}))
            .unwrap(),
    );
    assert_eq!(
        ascii,
        json!("{\"\\u043a\\u043b\\u044e\\u0447\": \"\\u503c \\ud83d\\ude00\"}")
    );
    let raw = returned(
        oracle
            .call("json.dumps", json!([value]), json!({"ensure_ascii": false}))
            .unwrap(),
    );
    assert_eq!(raw, json!("{\"ключ\": \"值 😀\"}"));
}

#[test]
fn functions_off_the_whitelist_are_refused() {
    let mut oracle = Oracle::spawn().unwrap();
    let err = oracle
        .call("os.system", json!(["true"]), json!({}))
        .unwrap_err();
    assert!(err.to_string().contains("whitelist"), "{err}");
    // The process keeps serving after a refusal.
    assert!(oracle.hello().is_ok());
}
