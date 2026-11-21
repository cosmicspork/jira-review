mod support;

use std::path::Path;
use std::process::Stdio;

use serde_json::{Value, json};
use support::Fake;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

const BIN: &str = env!("CARGO_BIN_EXE_jira-review");

/// The binary with only the environment a test gives it: the fake's
/// credential and a state directory of its own.
fn command(fake: &Fake, state: &Path) -> Command {
    let mut cmd = Command::new(BIN);
    cmd.env_clear()
        .envs(fake.cloud())
        .env("JIRA_REVIEW_STATE", state)
        .current_dir(state);
    cmd
}

struct Mcp {
    _child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
}

impl Mcp {
    fn start(fake: &Fake, state: &Path) -> Self {
        let mut child = command(fake, state)
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let lines = BufReader::new(child.stdout.take().unwrap()).lines();
        Self {
            _child: child,
            stdin,
            lines,
        }
    }

    async fn send(&mut self, msg: Value) {
        let mut line = msg.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await.unwrap();
        self.stdin.flush().await.unwrap();
    }

    async fn answer(&mut self, id: u64) -> Value {
        loop {
            let line = self.lines.next_line().await.unwrap().expect("an answer");
            let v: Value = serde_json::from_str(&line).unwrap();
            if v["id"] == id {
                return v;
            }
        }
    }

    async fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await;
        self.answer(id).await
    }

    /// A tool call's answer, parsed from its text content.
    async fn tool(&mut self, id: u64, name: &str, args: Value) -> (bool, Value) {
        let v = self
            .request(id, "tools/call", json!({ "name": name, "arguments": args }))
            .await;
        let result = &v["result"];
        let text = result["content"][0]["text"].as_str().unwrap();
        let parsed = serde_json::from_str(text).unwrap_or_else(|_| json!(text));
        (result["isError"] == true, parsed)
    }
}

async fn review(fake: &Fake, state: &Path, args: &[&str]) -> (i32, String) {
    let out = command(fake, state)
        .arg("review")
        .args(args)
        .output()
        .await
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn reads_answer_and_writes_wait_for_a_person() {
    let fake = Fake::start().await;
    let state = tempfile::tempdir().unwrap();
    let mut mcp = Mcp::start(&fake, state.path());

    let init = mcp
        .request(
            1,
            "initialize",
            json!({ "protocolVersion": "2025-06-18",
                                            "capabilities": {},
                                            "clientInfo": { "name": "t", "version": "0" } }),
        )
        .await;
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init["result"]["serverInfo"]["name"], "jira-review");
    mcp.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
        .await;

    let list = mcp.request(2, "tools/list", json!({})).await;
    assert_eq!(
        support::formatted(list["result"]["tools"].as_array().unwrap()),
        support::WIKI_FIELDS
    );
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "search",
            "issue",
            "transitions",
            "update",
            "transition",
            "link",
            "create",
            "comment",
            "proposal_status"
        ]
    );

    // Previews and conversions are Tracon's, answered only by `serve`.
    for (id, method) in [(30, "tracon/preview"), (31, "tracon/format/convert")] {
        let v = mcp.request(id, method, json!({})).await;
        assert_eq!(v["error"]["code"], -32601, "{v}");
    }

    let (error, issue) = mcp.tool(3, "issue", json!({ "key": "WRK-1" })).await;
    assert!(!error);
    assert_eq!(issue["summary"], "Port the feed");

    // A write proposes, and nothing reaches Jira.
    let (error, proposed) = mcp
        .tool(4, "comment", json!({ "key": "WRK-1", "body": "Shipped." }))
        .await;
    assert!(!error, "{proposed}");
    assert_eq!(proposed["status"], "pending");
    assert!(proposed["diff"].as_str().unwrap().contains("  + Shipped."));
    let id = proposed["proposal_id"].as_str().unwrap().to_string();
    assert!(fake.writes().is_empty());

    let (_, status) = mcp
        .tool(5, "proposal_status", json!({ "id": id, "wait_secs": 0 }))
        .await;
    assert_eq!(status["status"], "pending");
    let (code, listed) = review(&fake, state.path(), &["--list"]).await;
    assert_eq!(code, 0);
    assert!(listed.contains(&id), "{listed}");

    // An agent waiting on it hears as soon as a person approves it.
    mcp.send(json!({ "jsonrpc": "2.0", "id": 6, "method": "tools/call",
                     "params": { "name": "proposal_status",
                                 "arguments": { "id": id, "wait_secs": 20 } } }))
        .await;
    let (code, out) = review(&fake, state.path(), &["--approve", &id]).await;
    assert_eq!(code, 0, "{out}");
    let waited = mcp.answer(6).await;
    let status: Value =
        serde_json::from_str(waited["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(status["status"], "approved");
    assert_eq!(status["result"]["key"], "WRK-1");
    let writes = fake.writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].body, json!({ "body": "Shipped." }));

    // Approving it twice applies it once.
    let (code, out) = review(&fake, state.path(), &["--approve", &id]).await;
    assert_ne!(code, 0);
    assert!(out.contains("already decided"), "{out}");
    assert_eq!(fake.writes().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_or_stale_proposal_says_so_and_writes_nothing() {
    let fake = Fake::start().await;
    let state = tempfile::tempdir().unwrap();
    let mut mcp = Mcp::start(&fake, state.path());

    let (_, proposed) = mcp
        .tool(1, "update", json!({ "key": "WRK-2", "summary": "Tidy up" }))
        .await;
    let id = proposed["proposal_id"].as_str().unwrap().to_string();
    let (code, _) = review(
        &fake,
        state.path(),
        &["--reject", &id, "--reason", "not now"],
    )
    .await;
    assert_eq!(code, 0);
    let (_, status) = mcp.tool(2, "proposal_status", json!({ "id": id })).await;
    assert_eq!(status["status"], "rejected");
    assert_eq!(status["reason"], "not now");

    let (_, proposed) = mcp
        .tool(3, "update", json!({ "key": "WRK-2", "summary": "Tidy up" }))
        .await;
    let id = proposed["proposal_id"].as_str().unwrap().to_string();
    fake.bump("WRK-2");
    let (code, out) = review(&fake, state.path(), &["--approve", &id]).await;
    assert_eq!(code, 3, "{out}");
    let (_, status) = mcp.tool(4, "proposal_status", json!({ "id": id })).await;
    assert_eq!(status["status"], "stale");
    assert!(
        status["reason"].as_str().unwrap().contains("WRK-2 moved"),
        "{status}"
    );
    assert!(fake.writes().is_empty());

    // A refused proposal is an error the agent reads, not a failed call.
    let (error, message) = mcp
        .tool(5, "update", json!({ "key": "WRK-2", "status": "Done" }))
        .await;
    assert!(error);
    assert!(message.as_str().unwrap().contains("does not set status"));
    let unknown = mcp
        .request(
            6,
            "tools/call",
            json!({ "name": "delete", "arguments": {} }),
        )
        .await;
    assert_eq!(unknown["error"]["code"], -32602);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_route_is_one_proposal_a_person_approves_once() {
    let fake = Fake::start().await;
    fake.strict_workflow();
    let state = tempfile::tempdir().unwrap();
    let mut mcp = Mcp::start(&fake, state.path());

    let (error, proposed) = mcp
        .tool(
            1,
            "transition",
            json!({ "key": "WRK-1", "to_status": "Done",
                    "fields": { "resolution": { "name": "Fixed" } } }),
        )
        .await;
    assert!(!error, "{proposed}");
    let diff = proposed["diff"].as_str().unwrap();
    assert!(
        diff.contains("  5. Rollout -> Done by 51 (Finish)\n"),
        "{diff}"
    );
    let id = proposed["proposal_id"].as_str().unwrap().to_string();
    assert!(fake.writes().is_empty());

    let (code, out) = review(&fake, state.path(), &["--approve", &id]).await;
    assert_eq!(code, 0, "{out}");
    let (_, status) = mcp.tool(2, "proposal_status", json!({ "id": id })).await;
    assert_eq!(status["status"], "approved", "{status}");
    assert_eq!(status["result"]["final_status"], "Done");
    assert_eq!(status["result"]["hops"].as_array().unwrap().len(), 5);
    assert_eq!(fake.writes().len(), 5);
}
