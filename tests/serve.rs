mod support;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ed25519_dalek::SigningKey;
use jira_review::contract;
use serde_json::{Value, json};
use support::Fake;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};

const BIN: &str = env!("CARGO_BIN_EXE_jira-review");
const TOKEN: &str = "serve-test-token";

fn key() -> SigningKey {
    SigningKey::from_bytes(&std::array::from_fn(|i| i as u8 + 1))
}

fn node_key() -> String {
    hex::encode(key().verifying_key().to_bytes())
}

fn serve(dir: &Path, env: &[(&str, &str)]) -> (Command, PathBuf) {
    let socket = dir.join("s.sock");
    let mut cmd = Command::new(BIN);
    cmd.env_clear()
        .envs(env.iter().copied())
        .arg("serve")
        .arg("--socket")
        .arg(&socket)
        .arg("--data")
        .arg(dir.join("d"))
        .kill_on_drop(true);
    (cmd, socket)
}

/// One HTTP/1.1 exchange on the socket: the status and the JSON body.
async fn http(socket: &Path, method: &str, body: &Value) -> std::io::Result<(u16, Value)> {
    let mut stream = UnixStream::connect(socket).await?;
    let body = body.to_string();
    let request = format!(
        "{method} /mcp HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await?;
    let raw = String::from_utf8_lossy(&raw);
    let status = raw[9..12].parse().unwrap();
    let body = raw.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    Ok((status, serde_json::from_str(body).unwrap_or(Value::Null)))
}

struct Plugin {
    _child: Child,
    socket: PathBuf,
    next: u64,
}

impl Plugin {
    async fn start(dir: &Path) -> Self {
        let node = node_key();
        let (mut cmd, socket) = serve(
            dir,
            &[
                ("TRACON_PLUGIN_TOKEN", TOKEN),
                ("TRACON_NODE_KEY", &node),
                ("TRACON_CONTRACT", "1"),
            ],
        );
        let child = cmd.spawn().unwrap();
        // Ready means initialize answers within 30 s.
        let deadline = Instant::now() + Duration::from_secs(30);
        let init = json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize",
                           "params": { "protocolVersion": "2025-06-18", "capabilities": {},
                                       "clientInfo": { "name": "node", "version": "0" } } });
        loop {
            if let Ok((200, v)) = http(&socket, "POST", &init).await {
                assert_eq!(v["result"]["serverInfo"]["name"], "jira-review");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "initialize did not answer within 30 s"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Self {
            _child: child,
            socket,
            next: 1,
        }
    }

    async fn rpc(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let msg = json!({ "jsonrpc": "2.0", "id": self.next, "method": method, "params": params });
        let (status, v) = http(&self.socket, "POST", &msg).await.unwrap();
        assert_eq!(status, 200);
        v
    }

    async fn call(&mut self, tool: &str, args: Value, ctx: Option<Value>) -> Value {
        let mut params = json!({ "name": tool, "arguments": args });
        if let Some(ctx) = ctx {
            params["_meta"] = json!({ "tracon": ctx });
        }
        self.rpc("tools/call", params).await
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// A context the node would sign for `tool` with `args`, lending `lent`.
fn unsigned(tool: &str, args: &Value, approval: Option<&str>, lent: Value) -> Value {
    let now = now_ms();
    json!({
        "v": 1, "plugin": "jira-review", "instance": contract::instance_of(TOKEN),
        "tool": tool, "arguments_sha256": contract::arguments_sha256(Some(args)),
        "caller": { "kind": "agent", "session": "s-1", "harness": "claude" },
        "channel": "personal", "approval_id": approval,
        "issued_ms": now, "expires_ms": now + 30_000,
        "nonce": uuid::Uuid::new_v4().simple().to_string(),
        "credentials": lent,
    })
}

fn signed(tool: &str, args: &Value, approval: Option<&str>, lent: Value) -> Value {
    contract::sign(&unsigned(tool, args, approval, lent), &key())
}

fn lent(fake: &Fake) -> Value {
    json!({ "jira-work": { "JIRA_URL": fake.url, "JIRA_PAT": "work-pat" } })
}

fn refused_at(v: &Value) -> u64 {
    assert_eq!(v["error"]["code"], -32001, "{v}");
    v["error"]["data"]["check"].as_u64().unwrap()
}

fn text(v: &Value) -> (bool, Value) {
    let result = &v["result"];
    let text = result["content"][0]["text"].as_str().unwrap();
    (
        result["isError"] == true,
        serde_json::from_str(text).unwrap_or_else(|_| json!(text)),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn tools_are_listed_without_a_context_and_get_is_refused() {
    let fake = Fake::start().await;
    let dir = tempfile::tempdir().unwrap();
    let mut plugin = Plugin::start(dir.path()).await;
    let list = plugin.rpc("tools/list", json!({})).await;
    let tools = list["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 8);
    assert!(
        tools
            .iter()
            .all(|t| t["inputSchema"]["properties"]["profile"].is_object())
    );
    let (status, _) = http(&plugin.socket, "GET", &json!(null)).await.unwrap();
    assert_eq!(status, 405);
    let mode = std::fs::metadata(&plugin.socket).unwrap();
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(mode.permissions().mode() & 0o777, 0o600);
    assert!(fake.requests().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_whose_context_does_not_verify_is_refused_untouched() {
    let fake = Fake::start().await;
    let dir = tempfile::tempdir().unwrap();
    let mut plugin = Plugin::start(dir.path()).await;
    let args = json!({ "key": "WRK-1", "profile": "work" });

    let v = plugin.call("issue", args.clone(), None).await;
    assert_eq!(refused_at(&v), 1);

    let mut ctx = unsigned("issue", &args, None, lent(&fake));
    ctx["instance"] = json!(contract::instance_of("someone-elses-token"));
    let v = plugin
        .call("issue", args.clone(), Some(contract::sign(&ctx, &key())))
        .await;
    assert_eq!(refused_at(&v), 2);

    let ctx = signed(
        "issue",
        &json!({ "key": "WRK-2", "profile": "work" }),
        None,
        lent(&fake),
    );
    let v = plugin.call("issue", args.clone(), Some(ctx)).await;
    assert_eq!(refused_at(&v), 3);

    let mut ctx = signed("issue", &args, None, lent(&fake));
    ctx["channel"] = json!("elsewhere");
    let v = plugin.call("issue", args.clone(), Some(ctx)).await;
    assert_eq!(refused_at(&v), 4);

    let mut ctx = unsigned("issue", &args, None, lent(&fake));
    ctx["issued_ms"] = json!(now_ms() - 120_000);
    ctx["expires_ms"] = json!(now_ms() - 60_000);
    let v = plugin
        .call("issue", args.clone(), Some(contract::sign(&ctx, &key())))
        .await;
    assert_eq!(refused_at(&v), 5);

    assert!(fake.requests().is_empty(), "{:?}", fake.requests());

    let ctx = signed("issue", &args, None, lent(&fake));
    let (error, issue) = text(&plugin.call("issue", args.clone(), Some(ctx.clone())).await);
    assert!(!error, "{issue}");
    assert_eq!(issue["key"], "WRK-1");
    let v = plugin.call("issue", args.clone(), Some(ctx)).await;
    assert_eq!(refused_at(&v), 6);
    assert_eq!(fake.requests().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_uses_the_credential_lent_for_its_profile() {
    let fake = Fake::start().await;
    let dir = tempfile::tempdir().unwrap();
    let mut plugin = Plugin::start(dir.path()).await;
    let args = json!({ "key": "WRK-1", "profile": "work" });
    let ctx = signed("transitions", &args, None, lent(&fake));
    let (error, v) = text(&plugin.call("transitions", args, Some(ctx)).await);
    assert!(!error, "{v}");
    assert_eq!(v["transitions"][1]["to"], "Done");
    assert_eq!(fake.requests()[0].auth.as_deref(), Some("Bearer work-pat"));

    let args = json!({ "key": "WRK-1", "profile": "home" });
    let ctx = signed("issue", &args, None, lent(&fake));
    let (error, v) = text(&plugin.call("issue", args, Some(ctx)).await);
    assert!(error);
    assert_eq!(v, "credential jira-home was not lent to this call");
    assert_eq!(fake.requests().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_write_applies_only_with_an_approval() {
    let fake = Fake::start().await;
    let dir = tempfile::tempdir().unwrap();
    let mut plugin = Plugin::start(dir.path()).await;
    let args = json!({ "key": "WRK-1", "transition": "Start", "profile": "work" });

    let ctx = signed("transition", &args, None, lent(&fake));
    let v = plugin.call("transition", args.clone(), Some(ctx)).await;
    assert_eq!(refused_at(&v), 7);
    assert!(fake.requests().is_empty());

    let ctx = signed("transition", &args, Some("apv-7"), lent(&fake));
    let (error, v) = text(&plugin.call("transition", args, Some(ctx)).await);
    assert!(!error, "{v}");
    assert_eq!(v["result"]["status"], "In Progress");
    let writes = fake.writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].body, json!({ "transition": { "id": "11" } }));
    assert_eq!(
        fake.issue("WRK-1")["fields"]["status"]["name"],
        "In Progress"
    );
    let recorded = dir
        .path()
        .join("d/proposals/decided")
        .join(format!("{}.json", v["proposal_id"].as_str().unwrap()));
    let recorded: Value =
        serde_json::from_str(&std::fs::read_to_string(recorded).unwrap()).unwrap();
    assert_eq!(recorded["outcome"]["status"], "approved");
}

#[tokio::test(flavor = "multi_thread")]
async fn serve_will_not_start_without_the_contract_environment() {
    let dir = tempfile::tempdir().unwrap();
    let node = node_key();
    for (env, says) in [
        (
            vec![("TRACON_NODE_KEY", node.as_str())],
            "TRACON_PLUGIN_TOKEN",
        ),
        (vec![("TRACON_PLUGIN_TOKEN", TOKEN)], "TRACON_NODE_KEY"),
        (
            vec![
                ("TRACON_PLUGIN_TOKEN", TOKEN),
                ("TRACON_NODE_KEY", "not-hex"),
            ],
            "TRACON_NODE_KEY",
        ),
        (
            vec![
                ("TRACON_PLUGIN_TOKEN", TOKEN),
                ("TRACON_NODE_KEY", node.as_str()),
                ("TRACON_CONTRACT", "2"),
            ],
            "TRACON_CONTRACT",
        ),
    ] {
        let (mut cmd, socket) = serve(dir.path(), &env);
        let out = cmd.output().await.unwrap();
        assert!(!out.status.success());
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains(says), "{err}");
        assert!(!socket.exists());
    }
}
