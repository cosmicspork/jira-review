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
    assert_eq!(support::formatted(tools), support::WIKI_FIELDS);
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

/// The manifest's `select = "profile"` has the node lend `jira` itself when
/// a call's `profile` is absent, `null` or `""`; the plugin must pick the
/// same binding, and refuse a profile that is not a string.
#[tokio::test(flavor = "multi_thread")]
async fn a_call_without_a_profile_uses_jira_as_select_lends_it() {
    let fake = Fake::start().await;
    let dir = tempfile::tempdir().unwrap();
    let mut plugin = Plugin::start(dir.path()).await;
    let bare = json!({ "jira": { "JIRA_URL": fake.url, "JIRA_PAT": "bare-pat" } });
    for args in [
        json!({ "key": "WRK-1" }),
        json!({ "key": "WRK-1", "profile": null }),
        json!({ "key": "WRK-1", "profile": "" }),
    ] {
        let ctx = signed("issue", &args, None, bare.clone());
        let (error, v) = text(&plugin.call("issue", args.clone(), Some(ctx)).await);
        assert!(!error, "{args}: {v}");
        assert_eq!(v["key"], "WRK-1");
    }
    let sent = fake.requests();
    assert_eq!(sent.len(), 3);
    assert!(
        sent.iter()
            .all(|r| r.auth.as_deref() == Some("Bearer bare-pat"))
    );

    // On a channel that binds no `jira`, select lends such a call nothing.
    let args = json!({ "key": "WRK-1", "profile": "" });
    let ctx = signed("issue", &args, None, json!({}));
    let (error, v) = text(&plugin.call("issue", args, Some(ctx)).await);
    assert!(error);
    assert_eq!(v, "credential jira was not lent to this call");

    let args = json!({ "key": "WRK-1", "profile": 7 });
    let ctx = signed("issue", &args, None, bare);
    let (error, v) = text(&plugin.call("issue", args, Some(ctx)).await);
    assert!(error);
    assert_eq!(v, "profile expects a string");
    assert_eq!(fake.requests().len(), 3);
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

/// `tracon/preview` for `tool` with `args`, its context signed as the node
/// signs a preview's: like a call's, with no approval.
async fn preview(plugin: &mut Plugin, fake: &Fake, tool: &str, args: &Value) -> Value {
    let ctx = signed(tool, args, None, lent(fake));
    plugin
        .rpc(
            "tracon/preview",
            json!({ "name": tool, "arguments": args, "_meta": { "tracon": ctx } }),
        )
        .await
}

/// An approved call carrying the token of the preview the operator saw.
fn approved_with(tool: &str, args: &Value, token: &Value, fake: &Fake) -> Value {
    let mut ctx = unsigned(tool, args, Some("apv-9"), lent(fake));
    ctx["preview_token"] = token.clone();
    contract::sign(&ctx, &key())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_preview_shows_the_diff_sends_nothing_and_its_token_is_applied() {
    let fake = Fake::start().await;
    let dir = tempfile::tempdir().unwrap();
    let mut plugin = Plugin::start(dir.path()).await;
    let args = json!({ "key": "WRK-1", "summary": "Port the new feed", "profile": "work" });

    let v = preview(&mut plugin, &fake, "update", &args).await;
    let result = &v["result"];
    assert_eq!(result["previewable"], true, "{v}");
    assert_eq!(
        result["summary"],
        format!("update WRK-1 on {}", fake.url).as_str()
    );
    let body = result["body"].as_str().unwrap();
    assert!(body.starts_with("```diff\n"), "{body}");
    assert!(body.ends_with("```\n"), "{body}");
    assert!(
        body.contains("against WRK-1 as updated 2026-11-16T09:12:33.000+0000"),
        "{body}"
    );
    assert!(body.contains("  - Port the feed\n"), "{body}");
    assert!(body.contains("  + Port the new feed\n"), "{body}");
    assert_eq!(
        result["token"],
        r#"{"WRK-1":"2026-11-16T09:12:33.000+0000"}"#
    );
    assert!(fake.writes().is_empty());
    let decided = dir.path().join("d/proposals/decided");
    assert_eq!(std::fs::read_dir(&decided).map_or(0, |d| d.count()), 0);

    let ctx = approved_with("update", &args, &result["token"], &fake);
    let (error, v) = text(&plugin.call("update", args, Some(ctx)).await);
    assert!(!error, "{v}");
    assert_eq!(fake.writes().len(), 1);
    assert_eq!(
        fake.issue("WRK-1")["fields"]["summary"],
        "Port the new feed"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_approved_write_whose_issue_moved_since_its_preview_is_refused_as_stale() {
    let fake = Fake::start().await;
    let dir = tempfile::tempdir().unwrap();
    let mut plugin = Plugin::start(dir.path()).await;
    let args = json!({ "key": "WRK-1", "body": "Shipped.", "profile": "work" });

    let v = preview(&mut plugin, &fake, "comment", &args).await;
    let token = v["result"]["token"].clone();
    assert!(token.is_string(), "{v}");
    fake.bump("WRK-1");

    let ctx = approved_with("comment", &args, &token, &fake);
    let v = plugin.call("comment", args.clone(), Some(ctx)).await;
    assert_eq!(v["error"]["code"], -32002, "{v}");
    assert_eq!(v["error"]["data"], json!({ "stale": true }));
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("WRK-1 moved"),
        "{v}"
    );
    assert!(fake.writes().is_empty());
    let decided = dir.path().join("d/proposals/decided");
    let recorded: Vec<_> = std::fs::read_dir(decided).unwrap().collect();
    assert_eq!(recorded.len(), 1);
    let recorded: Value = serde_json::from_str(
        &std::fs::read_to_string(recorded[0].as_ref().unwrap().path()).unwrap(),
    )
    .unwrap();
    assert_eq!(recorded["outcome"]["status"], "stale");

    let ctx = approved_with("comment", &args, &json!("not a token"), &fake);
    let v = plugin.call("comment", args, Some(ctx)).await;
    assert_eq!(v["error"]["code"], -32602, "{v}");
    assert!(fake.writes().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn previews_of_reads_refusals_and_bad_contexts() {
    let fake = Fake::start().await;
    let dir = tempfile::tempdir().unwrap();
    let mut plugin = Plugin::start(dir.path()).await;

    let args = json!({ "key": "WRK-1", "profile": "work" });
    let v = preview(&mut plugin, &fake, "issue", &args).await;
    assert_eq!(v["result"]["previewable"], false, "{v}");
    assert_eq!(v["result"]["token"], Value::Null);
    assert!(fake.requests().is_empty());

    let v = plugin
        .rpc(
            "tracon/preview",
            json!({ "name": "comment", "arguments": args }),
        )
        .await;
    assert_eq!(refused_at(&v), 1);
    let ctx = signed("comment", &json!({ "key": "WRK-2" }), None, lent(&fake));
    let v = plugin
        .rpc(
            "tracon/preview",
            json!({ "name": "comment", "arguments": args, "_meta": { "tracon": ctx } }),
        )
        .await;
    assert_eq!(refused_at(&v), 3);

    let args = json!({ "key": "WRK-1", "transition": "Teleport", "profile": "work" });
    let v = preview(&mut plugin, &fake, "transition", &args).await;
    assert_eq!(v["error"]["code"], -32003, "{v}");
    assert!(
        v["error"]["message"].as_str().unwrap().contains("Teleport"),
        "{v}"
    );

    let v = plugin
        .rpc(
            "tracon/preview",
            json!({ "name": "delete", "arguments": {} }),
        )
        .await;
    assert_eq!(v["error"]["code"], -32602, "{v}");
    let v = plugin.rpc("tracon/preview", json!({})).await;
    assert_eq!(v["error"]["code"], -32602, "{v}");
    let v = plugin.rpc("tracon/nothing", json!({})).await;
    assert_eq!(v["error"]["code"], -32601, "{v}");
    assert!(fake.writes().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn jira_wiki_converts_both_ways_without_a_context() {
    let dir = tempfile::tempdir().unwrap();
    let mut plugin = Plugin::start(dir.path()).await;
    let convert = |to: &str, text: &str| json!({ "format": "jira_wiki", "to": to, "text": text });

    let v = plugin
        .rpc(
            "tracon/format/convert",
            convert("markdown", "h2. Goal\n*bold* and {{code}}"),
        )
        .await;
    assert_eq!(
        v["result"],
        json!({ "text": jira_review::wiki::to_markdown("h2. Goal\n*bold* and {{code}}") })
    );
    assert!(
        v["result"]["text"].as_str().unwrap().contains("**bold**"),
        "{v}"
    );

    let v = plugin
        .rpc(
            "tracon/format/convert",
            convert("jira_wiki", "## Goal\n**bold**"),
        )
        .await;
    assert_eq!(
        v["result"],
        json!({ "text": jira_review::wiki::from_markdown("## Goal\n**bold**") })
    );
    assert!(
        v["result"]["text"].as_str().unwrap().contains("h2. Goal"),
        "{v}"
    );

    for params in [
        json!({ "format": "markdown", "to": "jira_wiki", "text": "x" }),
        json!({ "format": "jira_wiki", "to": "html", "text": "x" }),
        json!({ "format": "jira_wiki", "to": "markdown" }),
        json!({ "to": "markdown", "text": "x" }),
    ] {
        let v = plugin.rpc("tracon/format/convert", params).await;
        assert_eq!(v["error"]["code"], -32602, "{v}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_preview_body_fences_the_diff_beyond_any_backticks_in_it() {
    let fake = Fake::start().await;
    let args = json!({ "key": "WRK-1", "body": "{code}\n```\nnot markdown\n```\n{code}" });
    let p = jira_review::change::propose(jira_review::change::Kind::Comment, &args, &fake.client())
        .await
        .unwrap();
    let body = jira_review::mcp::preview_of(&p)["body"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(body.starts_with("````diff\n"), "{body}");
    assert!(body.ends_with("\n````\n"), "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_paged_search_and_a_markdown_comment_through_the_plugin() {
    let fake = Fake::start().await;
    let dir = tempfile::tempdir().unwrap();
    let mut plugin = Plugin::start(dir.path()).await;

    let args = json!({ "jql": "project = WRK", "page": true, "profile": "work" });
    let ctx = signed("search", &args, None, lent(&fake));
    let (error, v) = text(&plugin.call("search", args, Some(ctx)).await);
    assert!(!error, "{v}");
    assert_eq!(v["site"], fake.url.as_str());
    assert_eq!(v["issues"].as_array().unwrap().len(), 3);
    assert_eq!(v["next_cursor"], Value::Null);

    let args = json!({ "key": "WRK-1", "body": "Done, **finally**.", "format": "markdown",
                       "profile": "work" });
    let ctx = signed("comment", &args, Some("apv-8"), lent(&fake));
    let (error, v) = text(&plugin.call("comment", args, Some(ctx)).await);
    assert!(!error, "{v}");
    let writes = fake.writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(
        writes[0].body,
        json!({ "body": jira_review::wiki::from_markdown("Done, **finally**.") })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_route_is_applied_only_along_the_route_its_card_showed() {
    let fake = Fake::start().await;
    fake.strict_workflow();
    let dir = tempfile::tempdir().unwrap();
    let mut plugin = Plugin::start(dir.path()).await;
    let args = json!({ "key": "WRK-1", "to_status": "UAT", "profile": "work" });

    let v = preview(&mut plugin, &fake, "transition", &args).await;
    let body = v["result"]["body"].as_str().unwrap();
    for line in [
        "  1. To Do -> Requirements Gathering by 11 (Gather)\n",
        "  2. Requirements Gathering -> In Progress by 21 (Start)\n",
        "  3. In Progress -> UAT by 31 (Test)\n",
    ] {
        assert!(body.contains(line), "{line:?} not in\n{body}");
    }
    let token = v["result"]["token"].clone();
    let shown: Value = serde_json::from_str(token.as_str().unwrap()).unwrap();
    assert_eq!(shown["route"][0], json!(["10001", "11", "10004"]));
    assert!(fake.writes().is_empty());

    // The workflow gains a shortcut while the card waits: the issue has not
    // moved, but the route found now is not the one approved.
    fake.add_transition("23", "Fast track", &["To Do"], "In Progress");
    let ctx = approved_with("transition", &args, &token, &fake);
    let v = plugin.call("transition", args.clone(), Some(ctx)).await;
    assert_eq!(v["error"]["code"], -32002, "{v}");
    assert_eq!(v["error"]["data"], json!({ "stale": true }));
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not the one its card showed"),
        "{v}"
    );
    assert!(fake.writes().is_empty());

    let v = preview(&mut plugin, &fake, "transition", &args).await;
    let token = v["result"]["token"].clone();
    let ctx = approved_with("transition", &args, &token, &fake);
    let (error, v) = text(&plugin.call("transition", args, Some(ctx)).await);
    assert!(!error, "{v}");
    assert_eq!(v["result"]["final_status"], "UAT");
    let ids: Vec<&Value> = v["result"]["hops"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| &h["transition"]["id"])
        .collect();
    assert_eq!(ids, ["23", "31"]);
    assert_eq!(fake.writes().len(), 2);
}
