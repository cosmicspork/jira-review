//! A small MCP server: JSON-RPC 2.0 with `initialize`, `ping`, `tools/list`
//! and `tools/call`, independent of transport. `stdio` carries it as lines
//! for any MCP client; `serve` carries it over HTTP on a Unix socket for
//! Tracon, which also sends `tracon/preview` and `tracon/format/convert`.
//! What a call does is the [`Host`]'s: `Standalone` proposes writes for a
//! person to review, `Plugin` applies them only with the node's approval.

use std::collections::HashMap;
use std::future::Future;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::auth::{self, Credential};
use crate::change::{self, ApplyError, Kind, Outcome, Proposal, Render, Versions};
use crate::contract::{self, CallId, Caller, ContractEnv, NonceCache};
use crate::jira::Jira;
use crate::store::{Place, Store};

/// The newest protocol revision this server speaks, and the one it answers
/// with when a client asks for one it does not know.
pub const PROTOCOL_VERSION: &str = "2025-06-18";
const KNOWN_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

pub const READS: [&str; 3] = ["search", "issue", "transitions"];

pub fn is_write(tool: &str) -> bool {
    Kind::parse(tool).is_some()
}

/// A JSON-RPC error.
#[derive(Debug, Clone)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }
}

/// What a server does with the calls it is sent.
pub trait Host: Send + Sync {
    fn tools(&self) -> Vec<Value>;

    /// A `tools/call`: the tool's result, or an error for the call itself.
    /// `meta` is `params._meta`, when the call carried one.
    fn call(
        &self,
        name: &str,
        arguments: Option<&Value>,
        meta: Option<&Value>,
    ) -> impl Future<Output = Result<Value, RpcError>> + Send;

    /// A `tracon/preview`: what a tool call would do, without doing it.
    fn preview(&self, _params: &Value) -> impl Future<Output = Result<Value, RpcError>> + Send {
        async { Err(no_method("tracon/preview")) }
    }

    /// A `tracon/format/convert`.
    fn convert(&self, _params: &Value) -> Result<Value, RpcError> {
        Err(no_method("tracon/format/convert"))
    }
}

fn no_method(method: &str) -> RpcError {
    RpcError::new(-32601, format!("no method {method}"))
}

/// A tool's answer.
pub fn tool_ok(v: &Value) -> Value {
    let text = serde_json::to_string_pretty(v).expect("JSON serializes");
    json!({ "content": [{ "type": "text", "text": text }], "isError": false })
}

/// A tool that ran and could not do what it was asked: the reason, for the
/// caller to read and act on.
pub fn tool_error(message: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": message }], "isError": true })
}

fn error_response(id: Value, e: RpcError) -> Value {
    let mut error = json!({ "code": e.code, "message": e.message });
    if let Some(data) = e.data {
        error["data"] = data;
    }
    json!({ "jsonrpc": "2.0", "id": id, "error": error })
}

/// A JSON-RPC error answer to a message that could not be read.
pub fn parse_error(e: &str) -> Value {
    error_response(
        Value::Null,
        RpcError::new(-32700, format!("parse error: {e}")),
    )
}

/// Answer one JSON-RPC message; `None` for a notification or a response,
/// which get no answer.
pub async fn handle<H: Host>(host: &H, msg: Value) -> Option<Value> {
    let Some(method) = msg.get("method").and_then(Value::as_str) else {
        if msg.get("result").is_some() || msg.get("error").is_some() {
            return None;
        }
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        return Some(error_response(id, RpcError::new(-32600, "invalid request")));
    };
    let id = msg.get("id").cloned()?;
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => {
            let asked = params["protocolVersion"].as_str().unwrap_or_default();
            let version = KNOWN_VERSIONS
                .into_iter()
                .find(|v| *v == asked)
                .unwrap_or(PROTOCOL_VERSION);
            Ok(json!({
                "protocolVersion": version,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "jira-review", "version": env!("CARGO_PKG_VERSION") },
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": host.tools() })),
        "tools/call" => match params["name"].as_str() {
            None => Err(RpcError::new(-32602, "tools/call needs a tool name")),
            Some(name) => {
                let meta = params.get("_meta").filter(|m| !m.is_null());
                host.call(name, params.get("arguments"), meta).await
            }
        },
        "tracon/preview" => host.preview(&params).await,
        "tracon/format/convert" => host.convert(&params),
        other => Err(no_method(other)),
    };
    Some(match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(e) => error_response(id, e),
    })
}

fn string(description: &str) -> Value {
    json!({ "type": "string", "description": description })
}

/// A string in the `jira_wiki` format, marked so that Tracon's card renders
/// it through `tracon/format/convert`.
fn wiki(description: &str) -> Value {
    json!({ "type": "string", "description": description, "x-tracon-format": "jira_wiki" })
}

fn nullable(description: &str) -> Value {
    json!({ "type": ["string", "null"], "description": description })
}

fn strings(description: &str) -> Value {
    json!({ "type": "array", "items": { "type": "string" }, "description": description })
}

/// The eight tools, as `tools/list` serves them. `profile` adds the argument
/// that picks which lent credential a plugin call uses.
pub fn definitions(profile: bool, write_note: &str) -> Vec<Value> {
    let key = string("Issue key, e.g. WRK-123.");
    let format = json!({
        "type": "string", "enum": ["jira_wiki", "markdown"],
        "description": "What the text is written in: jira_wiki unless you say. Markdown is \
                        converted to wiki markup, and the change shows what will be sent.",
    });
    let issue_fields = json!({
        "project": string("Project key, e.g. WRK."),
        "type": string("Issue type name, e.g. Task."),
        "summary": { "type": "string" },
        "description": wiki("Jira wiki markup, or Markdown with format markdown."),
        "priority": string("Priority name, e.g. High."),
        "assignee": nullable("Who to assign it: account id, username, email, display name, or \
                              \"me\"."),
        "duedate": nullable("Due date, YYYY-MM-DD."),
        "labels": strings("Single tokens, no spaces."),
        "parent": string("Parent issue key; in issues, #N for an earlier item."),
    });
    let mut create = json!({
        "type": "object",
        "properties": issue_fields.clone(),
    });
    create["properties"]["format"] = format.clone();
    create["properties"]["issues"] = json!({
        "type": "array",
        "description": format!(
            "Several issues in one change, in place of the fields above: at most {}, created \
             in order. An item's parent may be \"#N\", the earlier item at zero-based index N.",
            crate::args::MAX_ISSUES
        ),
        "items": { "type": "object", "properties": issue_fields,
                   "required": ["project", "type", "summary"] },
    });
    let read = json!({ "readOnlyHint": true, "openWorldHint": true });
    let write = |destructive: bool| {
        json!({ "readOnlyHint": false, "destructiveHint": destructive,
                "idempotentHint": false, "openWorldHint": true })
    };
    let mut tools = vec![
        json!({
            "name": "search",
            "description": "Search Jira issues with JQL. One row per issue: key, type, summary, \
                            status, priority, assignee, duedate, parent, labels. Read one in full \
                            with issue. With page or a cursor it answers one page instead: \
                            { site, account_email, issues (id, key, summary, status, \
                            status_category, priority, issue_type, labels, parent), \
                            next_cursor }, next_cursor null after the last page.",
            "inputSchema": { "type": "object", "properties": {
                "jql": string("e.g. project = WRK AND statusCategory != Done ORDER BY updated DESC"),
                "limit": { "type": "integer",
                           "description": "At most 50, 20 unless you say; paged, at most 100, \
                                           100 unless you say." },
                "page": { "type": "boolean", "description": "Answer the first page." },
                "cursor": string("The next_cursor of the page before: answers the next page."),
            }, "required": ["jql"] },
            "annotations": read,
        }),
        json!({
            "name": "issue",
            "description": "A Jira issue: summary, status, assignee, duedate, description (wiki markup), \
                            labels, parent, links (each with its direction and wording), \
                            subtasks, created, updated, and its ten most recent comments.",
            "inputSchema": { "type": "object", "properties": { "key": key },
                             "required": ["key"] },
            "annotations": read,
        }),
        json!({
            "name": "transitions",
            "description": "The transitions a Jira issue offers from its current status: each \
                            one's id, name, the status it leads to, and the fields it requires \
                            (with allowed values when Jira lists them).",
            "inputSchema": { "type": "object", "properties": { "key": key },
                             "required": ["key"] },
            "annotations": read,
        }),
        json!({
            "name": "update",
            "description": format!(
                "Edit an issue's summary, description, priority, assignee, due date, labels or \
                 parent. add_labels \
                 and remove_labels change labels relative to what the issue has; labels \
                 replaces them all and is not taken with either. Status changes only through \
                 transition. {write_note}"
            ),
            "inputSchema": { "type": "object", "properties": {
                "key": key,
                "summary": { "type": "string" },
                "description": wiki("Jira wiki markup, or Markdown with format markdown."),
                "format": format.clone(),
                "priority": string("Priority name, e.g. High."),
                "assignee": nullable("Who to assign it: account id, username, email, display \
                                      name, or \"me\"; found among the users the issue can be \
                                      assigned to, and refused with the candidates when it \
                                      names several. null unassigns."),
                "duedate": nullable("Due date, YYYY-MM-DD; null clears it."),
                "labels": strings("Replaces the issue's labels. Single tokens, no spaces."),
                "add_labels": strings("Labels to add, keeping the rest."),
                "remove_labels": strings("Labels to remove, keeping the rest. Matched ignoring \
                                          case; any the issue lacks come back as not_on_issue."),
                "parent": string("Parent issue key."),
            }, "required": ["key"] },
            "annotations": write(true),
        }),
        json!({
            "name": "transition",
            "description": format!(
                "Move an issue through its workflow by a transition it offers now, named by id \
                 or by name (of the transition or of the status it leads to), as transitions \
                 lists them. Fields the transition requires go in fields. {write_note}"
            ),
            "inputSchema": { "type": "object", "properties": {
                "key": key,
                "transition": string("Transition id or name, e.g. 31 or In Review."),
                "fields": { "type": "object",
                            "description": "Field id to value, as Jira takes it, e.g. \
                                            {\"resolution\": {\"name\": \"Fixed\"}}." },
            }, "required": ["key", "transition"] },
            "annotations": write(true),
        }),
        json!({
            "name": "link",
            "description": format!(
                "Link two issues, read in the type's outward wording: from blocks to, for type \
                 Blocks. A type this Jira does not have is refused with the names it does. \
                 {write_note}"
            ),
            "inputSchema": { "type": "object", "properties": {
                "from": key,
                "to": string("Issue key, e.g. WRK-124."),
                "type": string("Link type name, e.g. Blocks."),
            }, "required": ["from", "to", "type"] },
            "annotations": write(false),
        }),
        json!({
            "name": "create",
            "description": format!(
                "Create an issue, or with issues several in order, stopping at the first Jira \
                 refusal: the result lists what was created, the item that failed and the \
                 indexes skipped. {write_note}"
            ),
            "inputSchema": create,
            "annotations": write(false),
        }),
        json!({
            "name": "comment",
            "description": format!(
                "Comment on an issue. The body is Jira wiki markup (*bold*, {{{{code}}}}, \
                 * bullets), or Markdown with format markdown. {write_note}"
            ),
            "inputSchema": { "type": "object", "properties": {
                "key": key,
                "body": wiki("Jira wiki markup, or Markdown with format markdown."),
                "format": format,
            }, "required": ["key", "body"] },
            "annotations": write(false),
        }),
    ];
    if profile {
        for tool in &mut tools {
            tool["inputSchema"]["properties"]["profile"] =
                string("Which lent Jira credential to use: none for jira, P for jira-P.");
        }
    }
    tools
}

/// A read tool's answer.
pub async fn read(name: &str, args: &Value, jira: &Jira) -> Result<Value, String> {
    match name {
        "search" => {
            crate::args::only(args, "search", &["jql", "limit", "cursor", "page"])?;
            let jql = crate::args::text(args, "jql")?;
            let cursor = match args.get("cursor") {
                None | Some(Value::Null) => None,
                Some(Value::String(c)) => Some(c.as_str()),
                Some(_) => return Err("cursor expects a string".into()),
            };
            let page = match args.get("page") {
                None | Some(Value::Null) => false,
                Some(v) => v.as_bool().ok_or("page expects true or false")?,
            };
            let paged = page || cursor.is_some();
            let limit = match args.get("limit") {
                None | Some(Value::Null) if paged => 100,
                None | Some(Value::Null) => 20,
                Some(v) => v.as_u64().ok_or("limit expects a positive integer")?,
            };
            if paged {
                jira.search_page(jql, limit, cursor).await
            } else {
                jira.search(jql, limit).await
            }
        }
        "issue" | "transitions" => {
            crate::args::only(args, name, &["key"])?;
            let key = crate::args::issue_key(args.get("key"), "key")?;
            if name == "issue" {
                jira.issue(key).await
            } else {
                jira.transitions(key).await
            }
        }
        other => Err(format!("no read tool named {other}")),
    }
}

fn unknown(name: &str) -> RpcError {
    RpcError::new(-32602, format!("no tool named {name}"))
}

/// `jira-review mcp`: one credential from the environment, `.env` or the
/// keyring; writes wait in the store for `jira-review review`.
pub struct Standalone {
    pub credential: Result<Credential, String>,
    pub store: Store,
}

const STANDALONE_WRITE: &str = "Nothing is written yet: this proposes the change and returns a \
                                proposal_id. A person approves or rejects it with `jira-review \
                                review`; wait for that with proposal_status.";

/// The longest `proposal_status` waits in one call.
pub const MAX_WAIT_SECS: u64 = 45;

impl Standalone {
    fn jira(&self) -> Result<Jira, String> {
        self.credential.clone().map(Jira::new)
    }

    async fn status(&self, args: &Value) -> Result<Value, String> {
        crate::args::only(args, "proposal_status", &["id", "wait_secs"])?;
        let id = crate::args::text(args, "id")?;
        let wait = args
            .get("wait_secs")
            .and_then(Value::as_u64)
            .unwrap_or(30)
            .min(MAX_WAIT_SECS);
        let deadline = Instant::now() + Duration::from_secs(wait);
        loop {
            let found = self.store.get(id)?;
            let decided = match &found {
                None => return Err(format!("no proposal {id}")),
                Some((Place::Decided, p)) => p.outcome.as_ref(),
                Some((Place::Pending, _)) => None,
            };
            if let Some(o) = decided {
                let mut v = serde_json::to_value(o).expect("an outcome serializes");
                v["id"] = json!(id);
                return Ok(v);
            }
            if Instant::now() >= deadline {
                // Claimed but not yet written back: someone is applying it.
                let status = match found {
                    Some((Place::Decided, _)) => "applying",
                    _ => "pending",
                };
                return Ok(json!({ "id": id, "status": status }));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

impl Host for Standalone {
    fn tools(&self) -> Vec<Value> {
        let mut tools = definitions(false, STANDALONE_WRITE);
        tools.push(json!({
            "name": "proposal_status",
            "description": format!(
                "Wait for a proposed write to be decided, up to wait_secs (at most \
                 {MAX_WAIT_SECS}, 30 unless you say). status is pending while nobody has \
                 decided; approved with the result Jira gave; rejected with the reviewer's \
                 reason; stale when the issue moved after it was proposed (propose again); \
                 failed when Jira refused it."
            ),
            "inputSchema": { "type": "object", "properties": {
                "id": string("The proposal_id a write returned."),
                "wait_secs": { "type": "integer" },
            }, "required": ["id"] },
            "annotations": { "readOnlyHint": true, "openWorldHint": false },
        }));
        tools
    }

    async fn call(
        &self,
        name: &str,
        arguments: Option<&Value>,
        _meta: Option<&Value>,
    ) -> Result<Value, RpcError> {
        let empty = json!({});
        let args = arguments.unwrap_or(&empty);
        let answer = if name == "proposal_status" {
            self.status(args).await
        } else if READS.contains(&name) {
            match self.jira() {
                Ok(jira) => read(name, args, &jira).await,
                Err(e) => Err(e),
            }
        } else if let Some(kind) = Kind::parse(name) {
            match self.jira() {
                Ok(jira) => match change::propose(kind, args, &jira).await {
                    Ok(p) => self.store.put(&p).map(|()| {
                        json!({
                            "proposal_id": p.id,
                            "status": "pending",
                            "diff": change::render(&p, Render::default()),
                            "review": "run `jira-review review` in a terminal to approve or \
                                       reject it; wait with proposal_status",
                        })
                    }),
                    Err(e) => Err(e),
                },
                Err(e) => Err(e),
            }
        } else {
            return Err(unknown(name));
        };
        Ok(match answer {
            Ok(v) => tool_ok(&v),
            Err(e) => tool_error(&e),
        })
    }
}

/// `jira-review serve`: every call verified against the node's signed
/// caller context, its credential lent with it, and a write applied only
/// with the node's approval.
pub struct Plugin {
    pub env: ContractEnv,
    pub nonces: NonceCache,
    pub store: Store,
}

const PLUGIN_WRITE: &str = "Applied only once the operator has approved this call.";

/// Code for a call refused by its caller context.
pub const REFUSED: i64 = -32001;
/// Code for an approved write refused because an issue moved since its
/// preview; its `data` is `{"stale": true}`.
pub const STALE: i64 = -32002;
/// Code for a preview that could not be taken: the write would be refused.
pub const NOT_PREVIEWED: i64 = -32003;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// The Jira a call's `profile` picks among the credentials lent to it, and
/// its arguments without `profile`.
fn lent_jira(
    args: &Value,
    lent: &HashMap<String, HashMap<String, String>>,
) -> Result<(Value, Jira), String> {
    let mut args = args.clone();
    let profile = match args.as_object_mut().and_then(|o| o.remove("profile")) {
        None | Some(Value::Null) => None,
        Some(Value::String(p)) if !p.trim().is_empty() => Some(p.trim().to_string()),
        Some(_) => return Err("profile expects a non-empty string".into()),
    };
    let wanted = auth::lent_name(profile.as_deref());
    let fields = lent
        .get(&wanted)
        .ok_or_else(|| format!("credential {wanted} was not lent to this call"))?;
    let jira = Jira::new(Credential::from_map(
        fields,
        &format!("credential {wanted}"),
    )?);
    Ok((args, jira))
}

/// A proposal as `tracon/preview` answers it: its diff as Markdown, and its
/// `versions` as the token an approved call hands back.
pub fn preview_of(p: &Proposal) -> Value {
    let diff = change::changes(p, Render::default());
    let longest = diff.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    json!({
        "previewable": true,
        "summary": change::title(p),
        "body": format!("{fence}diff\n{diff}{fence}\n"),
        "token": contract::canonical(&json!(p.versions)),
    })
}

/// `tracon/format/convert` for `jira_wiki`, the format this plugin owns.
pub fn convert(params: &Value) -> Result<Value, RpcError> {
    let bad = |m: String| RpcError::new(-32602, m);
    match params["format"].as_str() {
        Some("jira_wiki") => {}
        Some(other) => return Err(bad(format!("format {other} is not jira_wiki"))),
        None => return Err(bad("tracon/format/convert needs a format".into())),
    }
    let text = params["text"]
        .as_str()
        .ok_or_else(|| bad("tracon/format/convert needs text".into()))?;
    let text = match params["to"].as_str() {
        Some("markdown") => crate::wiki::to_markdown(text),
        Some("jira_wiki") => crate::wiki::from_markdown(text),
        Some(other) => return Err(bad(format!("to {other} is not markdown or jira_wiki"))),
        None => return Err(bad("tracon/format/convert needs to".into())),
    };
    Ok(json!({ "text": text }))
}

impl Plugin {
    /// Check the caller context of a call or a preview of `name`.
    fn verify(
        &self,
        name: &str,
        arguments: Option<&Value>,
        meta: Option<&Value>,
        write: bool,
    ) -> Result<Caller, RpcError> {
        if !READS.contains(&name) && !is_write(name) {
            return Err(unknown(name));
        }
        let context = meta.map_or(&Value::Null, |m| &m["tracon"]);
        let call = CallId {
            tool: name,
            arguments,
            write,
        };
        contract::verify(context, &call, &self.env, now_ms(), &self.nonces).map_err(|r| RpcError {
            code: REFUSED,
            message: format!("refused: {}", r.reason),
            data: Some(json!({ "check": r.check })),
        })
    }

    async fn run(&self, name: &str, args: &Value, caller: &Caller) -> Result<Value, RpcError> {
        let answer = |r: Result<Value, String>| match r {
            Ok(v) => tool_ok(&v),
            Err(e) => tool_error(&e),
        };
        let (args, jira) = match lent_jira(args, &caller.credentials) {
            Ok(lent) => lent,
            Err(e) => return Ok(tool_error(&e)),
        };
        let Some(kind) = Kind::parse(name) else {
            return Ok(answer(read(name, &args, &jira).await));
        };
        let expected: Option<Versions> = match &caller.preview_token {
            None => None,
            Some(token) => Some(serde_json::from_str(token).map_err(|_| {
                RpcError::new(
                    -32602,
                    "preview_token is not a version token from this plugin",
                )
            })?),
        };
        // The node's card was the review: propose and apply in one step,
        // against the versions the card showed when it had a preview.
        let mut p = match change::propose(kind, &args, &jira).await {
            Ok(p) => p,
            Err(e) => return Ok(tool_error(&e)),
        };
        let applied = change::apply(&p, &jira, expected.as_ref()).await;
        p.outcome = Some(Outcome::of(applied.clone()));
        if let Err(e) = self.store.record(&p) {
            eprintln!("jira-review: could not record {}: {e}", p.id);
        }
        match applied {
            Err(e @ ApplyError::Stale { .. }) => Err(RpcError {
                code: STALE,
                message: e.to_string(),
                data: Some(json!({ "stale": true })),
            }),
            applied => Ok(answer(
                applied
                    .map(|result| json!({ "proposal_id": p.id, "result": result }))
                    .map_err(|e| e.to_string()),
            )),
        }
    }
}

impl Host for Plugin {
    fn tools(&self) -> Vec<Value> {
        definitions(true, PLUGIN_WRITE)
    }

    async fn call(
        &self,
        name: &str,
        arguments: Option<&Value>,
        meta: Option<&Value>,
    ) -> Result<Value, RpcError> {
        let caller = self.verify(name, arguments, meta, is_write(name))?;
        let empty = json!({});
        self.run(name, arguments.unwrap_or(&empty), &caller).await
    }

    /// Checks 1 to 6 of the caller context, as for a call; a preview needs
    /// no approval. Nothing is sent to Jira but reads.
    async fn preview(&self, params: &Value) -> Result<Value, RpcError> {
        let name = params["name"]
            .as_str()
            .ok_or_else(|| RpcError::new(-32602, "tracon/preview needs a tool name"))?;
        let arguments = params.get("arguments");
        let meta = params.get("_meta").filter(|m| !m.is_null());
        let caller = self.verify(name, arguments, meta, false)?;
        let Some(kind) = Kind::parse(name) else {
            return Ok(json!({
                "previewable": false,
                "summary": format!("{name} only reads"),
                "body": format!("`{name}` reads from Jira and changes nothing."),
                "token": null,
            }));
        };
        let empty = json!({});
        let refused = |e: String| RpcError::new(NOT_PREVIEWED, e);
        let (args, jira) =
            lent_jira(arguments.unwrap_or(&empty), &caller.credentials).map_err(refused)?;
        let p = change::propose(kind, &args, &jira).await.map_err(refused)?;
        Ok(preview_of(&p))
    }

    fn convert(&self, params: &Value) -> Result<Value, RpcError> {
        convert(params)
    }
}
