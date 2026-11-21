//! A write as a change file: what the issue has, what it would have, and
//! when it last changed. `propose` only reads; `apply` refuses when an issue
//! the change depends on has moved since, and only then sends the write.
//!
//! The file is pretty-printed JSON with a fixed key order, so a change can be
//! reviewed in git like any other diff.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::args::{self, LinkType, NewIssue};
use crate::jira::Jira;
use crate::{route, views};

/// The change file format this build reads and writes.
pub const FORMAT: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Update,
    Transition,
    Link,
    Create,
    Comment,
}

impl Kind {
    pub const ALL: [Kind; 5] = [
        Kind::Update,
        Kind::Transition,
        Kind::Link,
        Kind::Create,
        Kind::Comment,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Kind::Update => "update",
            Kind::Transition => "transition",
            Kind::Link => "link",
            Kind::Create => "create",
            Kind::Comment => "comment",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.name() == name)
    }
}

/// Each existing issue a change depends on, and its `updated` stamp when the
/// change was proposed: the change's version token.
pub type Versions = BTreeMap<String, String>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proposal {
    pub jira_review: u32,
    pub id: String,
    pub kind: Kind,
    /// The site the change was proposed against.
    pub jira: String,
    pub created: String,
    /// The tool arguments, validated; `apply` checks them again.
    pub args: Value,
    pub versions: Versions,
    pub current: Value,
    pub proposed: Value,
    /// How it was decided, once it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Outcome>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outcome {
    pub status: Status,
    pub decided: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Approved and applied: `result` is what Jira answered.
    Approved,
    /// Rejected by a reviewer: `reason` says why, when they said.
    Rejected,
    /// Approved, but an issue had moved, so nothing was sent.
    Stale,
    /// Approved, and Jira refused it or could not be reached.
    Failed,
}

impl Outcome {
    pub fn new(status: Status, result: Option<Value>, reason: Option<String>) -> Self {
        Self {
            status,
            decided: now(),
            result,
            reason,
        }
    }

    /// The outcome of an `apply`.
    pub fn of(applied: Result<Value, ApplyError>) -> Self {
        match applied {
            Ok(v) => Self::new(Status::Approved, Some(v), None),
            Err(e @ ApplyError::Stale { .. }) => {
                Self::new(Status::Stale, None, Some(e.to_string()))
            }
            Err(e) => Self::new(Status::Failed, None, Some(e.to_string())),
        }
    }
}

/// Why `apply` sent nothing, or what went wrong once it had.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyError {
    /// The change file is not one this build can apply as it stands.
    Invalid(String),
    /// `key` changed since the change was proposed: nothing was sent.
    Stale {
        key: String,
        was: String,
        now: String,
    },
    /// Jira refused, or could not be reached.
    Jira(String),
}

impl fmt::Display for ApplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApplyError::Invalid(e) | ApplyError::Jira(e) => f.write_str(e),
            ApplyError::Stale { key, was, now } => write!(
                f,
                "{key} moved since this change was proposed (was {was}, now {now}); propose again"
            ),
        }
    }
}

impl std::error::Error for ApplyError {}

impl Proposal {
    /// What the change is about, for a header or a list: an issue key, two
    /// keys, or the project(s) of new issues.
    pub fn subject(&self) -> String {
        let a = &self.args;
        let s = |v: &Value| v.as_str().unwrap_or("?").trim().to_string();
        match self.kind {
            Kind::Update | Kind::Transition | Kind::Comment => s(&a["key"]),
            Kind::Link => format!("{} {} {}", s(&a["from"]), s(&a["type"]), s(&a["to"])),
            Kind::Create => {
                let mut projects: Vec<String> = match a["issues"].as_array() {
                    Some(items) => items.iter().map(|i| s(&i["project"])).collect(),
                    None => vec![s(&a["project"])],
                };
                projects.dedup();
                projects.join(",")
            }
        }
    }

    pub fn to_json(&self) -> String {
        let mut s = serde_json::to_string_pretty(self).expect("a proposal serializes");
        s.push('\n');
        s
    }

    pub fn from_json(s: &str) -> Result<Self, String> {
        let p: Proposal = serde_json::from_str(s).map_err(|e| format!("not a change file: {e}"))?;
        if p.jira_review != FORMAT {
            return Err(format!(
                "change file format {} is not {FORMAT}, the one this build reads",
                p.jira_review
            ));
        }
        Ok(p)
    }
}

/// An RFC 3339 UTC timestamp, to the second.
pub fn now() -> String {
    let t = time::OffsetDateTime::now_utc()
        .replace_nanosecond(0)
        .expect("zero nanoseconds is valid");
    t.format(&time::format_description::well_known::Rfc3339)
        .expect("RFC 3339 formats")
}

/// Propose a write: read what it would change, and return the change
/// without sending anything. Markdown given with `format: "markdown"` is
/// converted first, so the proposal's `args` hold the wiki markup it sends.
///
/// This is also Tracon's `tracon/preview`: the proposal is the preview's
/// answer and its `versions` the version token the node hands back with an
/// approved call, for [`apply`]'s `expected`.
pub async fn propose(kind: Kind, args: &Value, jira: &Jira) -> Result<Proposal, String> {
    let converted;
    let args = match kind {
        Kind::Update | Kind::Create | Kind::Comment => {
            converted = args::wiki_input(args)?;
            &converted
        }
        Kind::Transition | Kind::Link => args,
    };
    let (versions, current, proposed) = match kind {
        Kind::Update => propose_update(args, jira).await?,
        Kind::Transition => propose_transition(args, jira).await?,
        Kind::Link => propose_link(args, jira).await?,
        Kind::Create => propose_create(args, jira).await?,
        Kind::Comment => propose_comment(args, jira).await?,
    };
    let mut p = Proposal {
        jira_review: FORMAT,
        id: String::new(),
        kind,
        jira: jira.url().to_string(),
        created: now(),
        args: args.clone(),
        versions,
        current,
        proposed,
        outcome: None,
    };
    let slug: String = p
        .subject()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    p.id = format!("{}-{}-{slug}", uuid::Uuid::now_v7(), kind.name());
    Ok(p)
}

type Proposed = (Versions, Value, Value);

const UPDATE_SHOWN: [&str; 7] = [
    "summary",
    "description",
    "priority",
    "assignee",
    "duedate",
    "labels",
    "parent",
];

/// An update's key, the fields it sets, its label edits and the assignee it
/// names.
type UpdateParts<'a> = (
    &'a str,
    Map<String, Value>,
    Vec<Value>,
    Option<Option<&'a str>>,
);

/// The update's fields and its label edits, as `apply` sends them, less the
/// assignee, which is a user to find and is sent as the account found.
fn update_parts(args: &Value) -> Result<UpdateParts<'_>, String> {
    let key = args::issue_key(args.get("key"), "key")?;
    let fields = args::fields_from(args, "update", &[])?;
    let edits = args::label_edits(args)?;
    let assignee = args::assignee_arg(args)?;
    if fields.is_empty() && edits.is_empty() && assignee.is_none() {
        return Err(format!(
            "nothing to update: name at least one of {}, {}",
            args::EDITABLE.join(", "),
            args::RELATIVE.join(", ")
        ));
    }
    Ok((key, fields, edits, assignee))
}

/// The account a change found for the assignee its arguments name: null to
/// unassign, else the `assignee_ref` propose put in `found`. A change that
/// names a user and holds no account for them is not applied.
fn assignee_ref(asked: Option<&str>, found: &Value) -> Result<Value, String> {
    let reference = &found["assignee_ref"];
    match asked {
        None => Ok(Value::Null),
        Some(_) if reference.is_object() => Ok(reference.clone()),
        Some(user) => Err(format!(
            "the change names assignee {user:?} but holds no account for them; propose again"
        )),
    }
}

/// The fields of an update as `apply` sends them, the assignee's account
/// included.
fn update_body(p: &Proposal) -> Result<UpdateParts<'_>, String> {
    let (key, mut fields, edits, assignee) = update_parts(&p.args)?;
    if let Some(asked) = assignee {
        fields.insert("assignee".into(), assignee_ref(asked, &p.proposed)?);
    }
    Ok((key, fields, edits, assignee))
}

/// The issues of a create as `apply` sends them, each named assignee's
/// account put in its fields from the planned issue at the same index.
fn create_body(p: &Proposal) -> Result<Vec<NewIssue>, String> {
    let mut plan = args::create_plan(&p.args)?;
    for (i, issue) in plan.iter_mut().enumerate() {
        if let Some(asked) = &issue.assignee {
            let found = &p.proposed["issues"][i];
            let reference =
                assignee_ref(asked.as_deref(), found).map_err(|e| format!("issues[{i}]: {e}"))?;
            issue.fields.insert("assignee".into(), reference);
        }
    }
    Ok(plan)
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

fn version_of(key: &str, raw: &Value) -> Result<(String, String), String> {
    let updated = raw["fields"]["updated"]
        .as_str()
        .ok_or_else(|| format!("jira did not say when {key} was updated"))?;
    Ok((key.to_string(), updated.to_string()))
}

async fn propose_update(args: &Value, jira: &Jira) -> Result<Proposed, String> {
    let (key, fields, edits, assignee) = update_parts(args)?;
    let raw = jira.get_issue(key, views::CURRENT_FIELDS).await?;
    let versions = Versions::from([version_of(key, &raw)?]);
    let current = views::current_view(&raw);
    let present = strings(&current["labels"]);
    let (edits, absent) = args::match_removals(edits, &present);
    let mut proposed = current.clone();
    for (k, v) in &fields {
        proposed[k] = match k.as_str() {
            "priority" => v["name"].clone(),
            "parent" => v["key"].clone(),
            _ => v.clone(),
        };
    }
    if !edits.is_empty() {
        let mut labels = present.clone();
        for e in &edits {
            if let Some(add) = e["add"].as_str() {
                if !labels.iter().any(|l| l == add) {
                    labels.push(add.to_string());
                }
            } else if let Some(remove) = e["remove"].as_str() {
                labels.retain(|l| l != remove);
            }
        }
        proposed["labels"] = json!(labels);
    }
    match assignee {
        None => {}
        Some(None) => {
            proposed["assignee"] = Value::Null;
            proposed["assignee_ref"] = Value::Null;
        }
        Some(Some(asked)) => {
            let user = jira.assignee(asked, ("issueKey", key)).await?;
            proposed["assignee"] = user["name"].clone();
            proposed["assignee_ref"] = user["ref"].clone();
        }
    }
    if proposed == current {
        let mut why = format!("nothing would change on {key}");
        if !absent.is_empty() {
            why.push_str(&format!(": it has none of {}", absent.join(", ")));
        }
        return Err(why);
    }
    if !absent.is_empty() {
        proposed["not_on_issue"] = json!(absent);
    }
    Ok((versions, current, proposed))
}

async fn propose_transition(args: &Value, jira: &Jira) -> Result<Proposed, String> {
    if route::is_route(args)? {
        return route::propose(args, jira).await;
    }
    args::only(args, "transition", &["key", "transition", "fields"])?;
    let key = args::issue_key(args.get("key"), "key")?;
    let asked = args::transition_arg(args)?;
    let given = match args.get("fields") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(f)) => f.clone(),
        Some(_) => return Err("fields expects an object of field id to value".into()),
    };
    let raw = jira.transitions_raw(key).await?;
    let issue = jira.get_issue(key, "status,updated").await?;
    let versions = Versions::from([version_of(key, &issue)?]);
    let t = route::pick(&raw, asked, key)?;
    route::check_fields(t, &given)?;
    let current = json!({ "status": issue["fields"]["status"]["name"] });
    let mut proposed = json!({
        "status": t["to"]["name"],
        "transition": { "id": t["id"], "name": t["name"] },
    });
    if !given.is_empty() {
        proposed["fields"] = Value::Object(given);
    }
    Ok((versions, current, proposed))
}

async fn propose_link(args: &Value, jira: &Jira) -> Result<Proposed, String> {
    args::only(args, "link", &["from", "to", "type"])?;
    let (from, to, asked) = args::link_args(args)?;
    let kind = args::link_type(&jira.link_types().await?, asked)?;
    let from_raw = jira.get_issue(from, "issuelinks,updated").await?;
    let to_raw = jira.get_issue(to, "summary,status,updated").await?;
    let versions = Versions::from([version_of(from, &from_raw)?, version_of(to, &to_raw)?]);
    let before = views::links(&from_raw["fields"]);
    let new = json!({
        "type": kind.name,
        "direction": "outward",
        "label": kind.outward,
        "key": to,
        "summary": to_raw["fields"]["summary"],
        "status": to_raw["fields"]["status"]["name"],
    });
    if views::linked(&before, from, to, &kind).is_ok() {
        return Err(format!("{from} already {} {to}", kind.outward));
    }
    let mut after = before.clone();
    after.push(new);
    Ok((
        versions,
        json!({ "links": before }),
        json!({
            "links": after,
            "link": { "type": kind.name, "outward": kind.outward, "inward": kind.inward },
        }),
    ))
}

/// A planned issue as a reviewer reads it: names, not Jira's shapes.
fn readable(issue: &NewIssue) -> Value {
    let f = &issue.fields;
    let mut out = json!({
        "project": f["project"]["key"],
        "type": f["issuetype"]["name"],
        "summary": f["summary"],
    });
    if let Some(d) = f.get("description") {
        out["description"] = d.clone();
    }
    if let Some(p) = f.get("priority") {
        out["priority"] = p["name"].clone();
    }
    if let Some(a) = &issue.assignee {
        out["assignee"] = json!(a);
    }
    if let Some(d) = f.get("duedate") {
        out["duedate"] = d.clone();
    }
    if let Some(l) = f.get("labels") {
        out["labels"] = l.clone();
    }
    match (issue.parent_item, f.get("parent")) {
        (Some(n), _) => out["parent"] = json!(format!("#{n}")),
        (None, Some(p)) => out["parent"] = p["key"].clone(),
        (None, None) => {}
    }
    out
}

async fn propose_create(args: &Value, jira: &Jira) -> Result<Proposed, String> {
    let plan = args::create_plan(args)?;
    // A parent named by key must exist now; it is not checked again at apply,
    // where a new issue has nothing older to be stale against.
    let mut seen = Vec::new();
    for (i, issue) in plan.iter().enumerate() {
        if let Some(key) = issue.fields.get("parent").and_then(|p| p["key"].as_str()) {
            if seen.contains(&key) {
                continue;
            }
            jira.get_issue(key, "summary")
                .await
                .map_err(|e| format!("issues[{i}]: parent {key}: {e}"))?;
            seen.push(key);
        }
    }
    let mut issues: Vec<Value> = plan.iter().map(readable).collect();
    // Each named assignee is found among who the project can assign, once
    // per project and name.
    let mut found: BTreeMap<(String, String), Value> = BTreeMap::new();
    for (i, issue) in plan.iter().enumerate() {
        let Some(asked) = issue.assignee.as_ref() else {
            continue;
        };
        let Some(asked) = asked else {
            issues[i]["assignee_ref"] = Value::Null;
            continue;
        };
        let project = issue.fields["project"]["key"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let at = (project, asked.to_lowercase());
        if !found.contains_key(&at) {
            let user = jira
                .assignee(asked, ("project", &at.0))
                .await
                .map_err(|e| format!("issues[{i}]: assignee: {e}"))?;
            found.insert(at.clone(), user);
        }
        let user = &found[&at];
        issues[i]["assignee"] = user["name"].clone();
        issues[i]["assignee_ref"] = user["ref"].clone();
    }
    Ok((Versions::new(), Value::Null, json!({ "issues": issues })))
}

async fn propose_comment(args: &Value, jira: &Jira) -> Result<Proposed, String> {
    args::only(args, "comment", &["key", "body"])?;
    let key = args::issue_key(args.get("key"), "key")?;
    let body = args::text(args, "body")?;
    let raw = jira.get_issue(key, "comment,updated").await?;
    let versions = Versions::from([version_of(key, &raw)?]);
    let last = raw["fields"]["comment"]["comments"]
        .as_array()
        .and_then(|c| c.last())
        .map(|c| json!({ "author": c["author"]["displayName"], "created": c["created"] }))
        .unwrap_or(Value::Null);
    Ok((
        versions,
        json!({ "last_comment": last }),
        json!({ "body": body }),
    ))
}

/// Apply a proposed change: check its arguments again, refuse if any issue
/// it depends on moved since `expected` (the proposal's own `versions` when
/// `None`), then send it.
///
/// Jira has no conditional write, so a change that lands between the check
/// and the send is not caught.
pub async fn apply(
    p: &Proposal,
    jira: &Jira,
    expected: Option<&Versions>,
) -> Result<Value, ApplyError> {
    use ApplyError::{Invalid, Jira as Refused};
    if p.jira_review != FORMAT {
        return Err(Invalid(format!(
            "change file format {} is not {FORMAT}",
            p.jira_review
        )));
    }
    if p.jira.trim_end_matches('/') != jira.url() {
        return Err(Invalid(format!(
            "this change was proposed against {}, but the credential is for {}",
            p.jira,
            jira.url()
        )));
    }
    // Every check that needs no request runs before the first one.
    let a = &p.args;
    match p.kind {
        Kind::Update => update_body(p).map(drop),
        Kind::Transition if is_route(p) => route::validate(p),
        Kind::Transition => args::issue_key(a.get("key"), "key")
            .and_then(|_| args::transition_arg(a))
            .map(drop),
        Kind::Link => args::link_args(a).map(drop),
        Kind::Create => create_body(p).map(drop),
        Kind::Comment => args::issue_key(a.get("key"), "key")
            .and_then(|_| args::text(a, "body"))
            .map(drop),
    }
    .map_err(Invalid)?;

    let versions = expected.unwrap_or(&p.versions);
    for (key, was) in versions {
        let now = jira.updated(key).await.map_err(Refused)?;
        if &now != was {
            return Err(ApplyError::Stale {
                key: key.clone(),
                was: was.clone(),
                now,
            });
        }
    }

    match p.kind {
        Kind::Update => {
            let (key, fields, edits, _) = update_body(p).map_err(Invalid)?;
            let (edits, absent) = args::match_removals(edits, &strings(&p.current["labels"]));
            let mut named: Vec<String> = fields.keys().cloned().collect();
            let mut body = json!({ "fields": fields });
            if !edits.is_empty() {
                named.push("labels".into());
                body["update"] = json!({ "labels": edits });
            }
            if !named.is_empty() {
                jira.edit(key, &body).await.map_err(Refused)?;
            }
            let mut v = json!({ "key": key, "updated": named });
            if !absent.is_empty() {
                v["not_on_issue"] = json!(absent);
            }
            Ok(v)
        }
        Kind::Transition if is_route(p) => route::apply(p, jira).await,
        Kind::Transition => {
            let key = args::issue_key(a.get("key"), "key").map_err(Invalid)?;
            let id = p.proposed["transition"]["id"]
                .as_str()
                .ok_or_else(|| Invalid("the change names no transition id".into()))?;
            let mut body = json!({ "transition": { "id": id } });
            if let Some(fields) = a.get("fields").filter(|f| f.is_object()) {
                body["fields"] = fields.clone();
            }
            jira.transition(key, &body).await.map_err(Refused)?;
            Ok(json!({ "key": key, "transition": id, "status": p.proposed["status"] }))
        }
        Kind::Link => {
            let (from, to, _) = args::link_args(a).map_err(Invalid)?;
            let l = &p.proposed["link"];
            let kind = LinkType {
                name: l["type"].as_str().unwrap_or_default().to_string(),
                outward: l["outward"].as_str().unwrap_or_default().to_string(),
                inward: l["inward"].as_str().unwrap_or_default().to_string(),
            };
            if kind.name.is_empty() {
                return Err(Invalid("the change names no link type".into()));
            }
            jira.link(&kind.name, from, to).await.map_err(Refused)?;
            // A link is created with 201 and no body, so whether it reads the
            // way it was asked is read from `from` itself.
            let back = jira
                .get_issue(from, "issuelinks")
                .await
                .map_err(|e| Refused(format!("jira linked {from} and {to}, but {e}")))?;
            views::linked(&views::links(&back["fields"]), from, to, &kind).map_err(Refused)
        }
        Kind::Create => {
            let plan = create_body(p).map_err(Invalid)?;
            if a.get("issues").is_none() {
                let only = plan
                    .into_iter()
                    .next()
                    .ok_or_else(|| Invalid("nothing to create".into()))?;
                return jira.create(only.fields).await.map_err(Refused);
            }
            create_in_order(plan, |fields| jira.create(fields))
                .await
                .map_err(Refused)
        }
        Kind::Comment => {
            let key = args::issue_key(a.get("key"), "key").map_err(Invalid)?;
            let body = args::text(a, "body").map_err(Invalid)?;
            jira.comment(key, body).await.map_err(Refused)
        }
    }
}

/// Whether a transition change takes a route of several hops rather than
/// one transition.
pub fn is_route(p: &Proposal) -> bool {
    p.kind == Kind::Transition && p.proposed.get("hops").is_some()
}

/// Create a planned set in order, each `#N` parent made the key item N was
/// created as, stopping at the first refusal. The result says what exists
/// now and what does not, so the caller asks again for only what is missing.
/// A run that created nothing is an error, as a refused single issue is.
pub async fn create_in_order<F, Fut>(plan: Vec<NewIssue>, mut create: F) -> Result<Value, String>
where
    F: FnMut(Map<String, Value>) -> Fut,
    Fut: std::future::Future<Output = Result<Value, String>>,
{
    let total = plan.len();
    let mut keys: Vec<String> = Vec::with_capacity(total);
    let mut created = Vec::new();
    for (index, mut issue) in plan.into_iter().enumerate() {
        if let Some(n) = issue.parent_item {
            issue
                .fields
                .insert("parent".into(), json!({ "key": keys[n] }));
        }
        match create(issue.fields).await {
            Ok(v) => {
                let key = v["key"].as_str().unwrap_or_default().to_string();
                created.push(json!({ "index": index, "key": key, "url": v["url"] }));
                keys.push(key);
            }
            Err(error) if created.is_empty() => {
                let untried = match total - 1 {
                    0 => String::new(),
                    1 => "; the other item was not tried".into(),
                    n => format!("; the other {n} were not tried"),
                };
                return Err(format!("issues[0]: {error}; nothing was created{untried}"));
            }
            Err(error) => {
                return Ok(json!({
                    "created": created,
                    "failed": { "index": index, "error": error },
                    "skipped": ((index + 1)..total).collect::<Vec<_>>(),
                }));
            }
        }
    }
    Ok(json!({ "created": created, "failed": null, "skipped": [] }))
}

/// How `show` renders a change.
#[derive(Debug, Clone, Copy, Default)]
pub struct Render {
    /// ANSI colour: red for what goes, green for what comes.
    pub color: bool,
    /// Wiki text (descriptions, comment bodies) shown as Markdown.
    pub markdown: bool,
}

/// What a line of a rendered change is, so each surface can mark it its
/// own way: ANSI colour in a terminal, a class on the review page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// The change's one-line title.
    Title,
    /// A field name, a note, or a blank line.
    Plain,
    /// What goes.
    Removed,
    /// What comes.
    Added,
    /// What stays, shown around a change.
    Context,
}

/// One line of a rendered change, without its marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub tone: Tone,
    pub text: String,
}

#[derive(Default)]
struct Out {
    lines: Vec<Line>,
}

impl Out {
    fn push(&mut self, tone: Tone, text: &str) {
        self.lines.push(Line {
            tone,
            text: text.to_string(),
        });
    }

    fn line(&mut self, text: &str) {
        self.push(Tone::Plain, text);
    }

    fn removed(&mut self, text: &str) {
        self.push(Tone::Removed, text);
    }

    fn added(&mut self, text: &str) {
        self.push(Tone::Added, text);
    }

    fn context(&mut self, text: &str) {
        self.push(Tone::Context, text);
    }

    /// The lines as text, with `-`/`+` markers and, when asked, colour.
    fn text(&self, color: bool) -> String {
        let mut s = String::new();
        for Line { tone, text } in &self.lines {
            let (code, text) = match tone {
                Tone::Title => ("1", text.clone()),
                Tone::Plain => ("", text.clone()),
                Tone::Removed => ("31", format!("  - {text}")),
                Tone::Added => ("32", format!("  + {text}")),
                Tone::Context => ("", format!("    {text}")),
            };
            if color && !code.is_empty() {
                s.push_str(&format!("\x1b[{code}m{text}\x1b[0m\n"));
            } else {
                s.push_str(&text);
                s.push('\n');
            }
        }
        s
    }
}

fn scalar(v: &Value) -> String {
    match v {
        Value::Null => "(none)".into(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// A text field's change as a unified line diff, three lines of context.
fn text_diff(out: &mut Out, old: &str, new: &str) {
    let diff = similar::TextDiff::from_lines(old, new);
    for (i, group) in diff.grouped_ops(3).iter().enumerate() {
        if i > 0 {
            out.line("    ...");
        }
        for op in group {
            for change in diff.iter_changes(op) {
                let text = change.value().trim_end_matches('\n');
                match change.tag() {
                    similar::ChangeTag::Delete => out.removed(text),
                    similar::ChangeTag::Insert => out.added(text),
                    similar::ChangeTag::Equal => out.context(text),
                }
            }
        }
    }
}

/// Render a change for a reviewer: a header, then each field it changes.
pub fn render(p: &Proposal, opts: Render) -> String {
    let mut out = Out::default();
    out.push(Tone::Title, &title(p));
    out.line(&format!("id {}, proposed {}", p.id, p.created));
    render_changes(&mut out, p, opts);
    out.text(opts.color)
}

/// What a change is, in one line: its kind, subject and site.
pub fn title(p: &Proposal) -> String {
    format!("{} {} on {}", p.kind.name(), p.subject(), p.jira)
}

/// [`render`] without the header: what the change depends on and each field
/// it changes.
pub fn changes(p: &Proposal, opts: Render) -> String {
    let mut out = Out::default();
    render_changes(&mut out, p, opts);
    out.text(opts.color)
}

/// [`changes`] as lines with their tone and no markers, for a surface that
/// marks them itself. `opts.color` is ignored.
pub fn change_lines(p: &Proposal, opts: Render) -> Vec<Line> {
    let mut out = Out::default();
    render_changes(&mut out, p, opts);
    out.lines
}

fn render_changes(out: &mut Out, p: &Proposal, opts: Render) {
    let wiki = |v: &Value| -> String {
        let s = v.as_str().unwrap_or_default();
        if opts.markdown {
            crate::wiki::to_markdown(s)
        } else {
            s.to_string()
        }
    };
    if p.versions.is_empty() {
        out.line("depends on no existing issue");
    }
    for (key, updated) in &p.versions {
        out.line(&format!("against {key} as updated {updated}"));
    }
    out.line("");
    let (cur, new) = (&p.current, &p.proposed);
    match p.kind {
        Kind::Update => {
            for field in UPDATE_SHOWN {
                // Two people may share a name; the account tells them apart.
                let same_account =
                    field != "assignee" || cur["assignee_ref"] == new["assignee_ref"];
                if cur[field] == new[field] && same_account {
                    continue;
                }
                out.line(&format!("{field}:"));
                match field {
                    "labels" => {
                        let (old, now) = (strings(&cur[field]), strings(&new[field]));
                        for l in old.iter().filter(|l| !now.contains(l)) {
                            out.removed(l);
                        }
                        for l in now.iter().filter(|l| !old.contains(l)) {
                            out.added(l);
                        }
                    }
                    "summary" | "description" => {
                        text_diff(out, &wiki(&cur[field]), &wiki(&new[field]))
                    }
                    _ => {
                        out.removed(&scalar(&cur[field]));
                        out.added(&scalar(&new[field]));
                    }
                }
            }
            if let Some(absent) = new["not_on_issue"].as_array() {
                let names: Vec<String> = absent.iter().map(scalar).collect();
                out.line(&format!(
                    "not on the issue, so not removed: {}",
                    names.join(", ")
                ));
            }
        }
        Kind::Transition if is_route(p) => render_route(out, p),
        Kind::Transition => {
            out.line("status:");
            out.removed(&scalar(&cur["status"]));
            out.added(&scalar(&new["status"]));
            out.line(&format!(
                "by transition {} ({})",
                scalar(&new["transition"]["id"]),
                scalar(&new["transition"]["name"])
            ));
            if let Some(fields) = new["fields"].as_object() {
                out.line("with fields:");
                for (k, v) in fields {
                    out.added(&format!("{k}: {v}"));
                }
            }
        }
        Kind::Link => {
            out.line(&format!("links of {}:", scalar(&p.args["from"]).trim()));
            let old = cur["links"].as_array().cloned().unwrap_or_default();
            let describe = |l: &Value| {
                let mut s = format!("{} {}", scalar(&l["label"]), scalar(&l["key"]));
                if l["summary"].is_string() {
                    s.push_str(&format!(
                        " ({}, {})",
                        scalar(&l["summary"]),
                        scalar(&l["status"])
                    ));
                }
                s
            };
            for l in new["links"].as_array().into_iter().flatten() {
                if old.contains(l) {
                    out.context(&describe(l));
                } else {
                    out.added(&describe(l));
                }
            }
        }
        Kind::Create => {
            out.line("new issues:");
            for (i, issue) in new["issues"].as_array().into_iter().flatten().enumerate() {
                out.added(&format!(
                    "[{i}] {} {}: {}",
                    scalar(&issue["project"]),
                    scalar(&issue["type"]),
                    scalar(&issue["summary"])
                ));
                for field in ["priority", "assignee", "duedate", "labels", "parent"] {
                    // An assignee or due date given as null is shown as none.
                    if issue.get(field).is_some() {
                        let v = match &issue[field] {
                            Value::Array(a) => a.iter().map(scalar).collect::<Vec<_>>().join(", "),
                            other => scalar(other),
                        };
                        out.context(&format!("  {field}: {v}"));
                    }
                }
                if issue["description"].is_string() {
                    out.context("  description:");
                    for line in wiki(&issue["description"]).lines() {
                        out.context(&format!("  | {line}"));
                    }
                }
            }
        }
        Kind::Comment => {
            match &cur["last_comment"] {
                Value::Null => out.line("no comments yet"),
                last => out.line(&format!(
                    "last comment by {} at {}",
                    scalar(&last["author"]),
                    scalar(&last["created"])
                )),
            }
            out.line("new comment:");
            for line in wiki(&new["body"]).lines() {
                out.added(line);
            }
        }
    }
}

/// A route: where the issue is and where it ends up, then each hop in order
/// with what it takes.
fn render_route(out: &mut Out, p: &Proposal) {
    let new = &p.proposed;
    let hops = new["hops"].as_array().cloned().unwrap_or_default();
    let unknown = |v: &Value, asked: &Value| match v {
        Value::Null => format!("(where {:?} leads)", asked.as_str().unwrap_or("?")),
        other => scalar(other),
    };
    out.line("status:");
    out.removed(&scalar(&p.current["status"]));
    let last = hops.last().cloned().unwrap_or(Value::Null);
    out.added(&unknown(&new["status"], &last["asked"]));
    let how = match (new["route"].as_str(), new["workflow"].as_str()) {
        (Some("workflow"), Some(w)) => format!(
            "the shortest route in workflow {w} that starts with a transition the issue offers \
             now"
        ),
        (Some("offered"), _) => "a transition the issue offers now".into(),
        (_, Some(w)) => format!("the path given, checked against workflow {w}"),
        _ => "the path given; the workflow could not be read, so each hop after the first is \
              checked only when the issue reaches it"
            .into(),
    };
    let n = hops.len();
    out.line(&format!(
        "by {n} transition{}, in order ({how}):",
        if n == 1 { "" } else { "s" }
    ));
    for (i, h) in hops.iter().enumerate() {
        let step = match h["transition"].as_object() {
            Some(t) => format!(
                "{} -> {} by {} ({})",
                scalar(&h["from"]),
                scalar(&h["to"]),
                scalar(&t["id"]),
                scalar(&t["name"])
            ),
            None => format!(
                "{} {:?}, as the issue offers it then",
                match h["from"].as_str() {
                    Some(from) => format!("from {from} by"),
                    None => "then by".into(),
                },
                h["asked"].as_str().unwrap_or("?")
            ),
        };
        out.line(&format!("  {}. {step}", i + 1));
        match h["required_fields"].as_array() {
            Some(r) if r.is_empty() => {}
            Some(r) => {
                let names: Vec<String> = r.iter().map(route::describe_field).collect();
                out.line(&format!("     requires {}", names.join("; ")));
            }
            None => out.line("     its required fields are known only once the issue gets there"),
        }
        if let Some(fields) = h["fields"].as_object() {
            for (k, v) in fields {
                out.added(&format!("   {k}: {v}"));
            }
        }
    }
    out.line(
        "each hop is checked again just before it is sent; one that cannot be sent stops the \
         route there, and the hops before it stay applied",
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update() -> Proposal {
        Proposal {
            jira_review: FORMAT,
            id: "0193-update-WRK-12".into(),
            kind: Kind::Update,
            jira: "https://example.atlassian.net".into(),
            created: "2026-11-17T10:00:00Z".into(),
            args: json!({ "key": "WRK-12", "summary": "New title", "priority": "High",
                          "add_labels": ["etl"], "remove_labels": ["old", "gone"] }),
            versions: Versions::from([("WRK-12".into(), "2026-11-16T09:12:33.000+0000".into())]),
            current: json!({ "summary": "Old title", "description": "one\ntwo",
                             "priority": "Low", "labels": ["old", "keep"], "parent": null }),
            proposed: json!({ "summary": "New title", "description": "one\ntwo",
                              "priority": "High", "labels": ["keep", "etl"], "parent": null,
                              "not_on_issue": ["gone"] }),
            outcome: None,
        }
    }

    #[test]
    fn a_change_file_round_trips_with_a_stable_key_order() {
        let p = update();
        let s = p.to_json();
        let back = Proposal::from_json(&s).unwrap();
        assert_eq!(back.to_json(), s);
        let keys: Vec<usize> = [
            "jira_review",
            "\"id\"",
            "kind",
            "\"jira\"",
            "created",
            "args",
            "versions",
            "current",
            "proposed",
        ]
        .iter()
        .map(|k| s.find(k).unwrap())
        .collect();
        assert!(keys.windows(2).all(|w| w[0] < w[1]), "{s}");
        assert!(!s.contains("outcome"));
        let e = Proposal::from_json(&s.replace("\"jira_review\": 1", "\"jira_review\": 2"))
            .unwrap_err();
        assert!(e.contains("format 2"), "{e}");
    }

    #[test]
    fn an_update_renders_each_changed_field() {
        assert_eq!(
            render(&update(), Render::default()),
            "update WRK-12 on https://example.atlassian.net\n\
             id 0193-update-WRK-12, proposed 2026-11-17T10:00:00Z\n\
             against WRK-12 as updated 2026-11-16T09:12:33.000+0000\n\
             \n\
             summary:\n  - Old title\n  + New title\n\
             priority:\n  - Low\n  + High\n\
             labels:\n  - old\n  + etl\n\
             not on the issue, so not removed: gone\n"
        );
    }

    #[test]
    fn colour_marks_only_the_changed_lines() {
        let s = render(
            &update(),
            Render {
                color: true,
                markdown: false,
            },
        );
        assert!(s.contains("\x1b[31m  - Old title\x1b[0m"), "{s}");
        assert!(s.contains("\x1b[32m  + New title\x1b[0m"), "{s}");
    }

    #[test]
    fn a_description_renders_as_a_line_diff() {
        let mut p = update();
        p.proposed["description"] = json!("one\nthree");
        let s = render(&p, Render::default());
        assert!(
            s.contains("description:\n    one\n  - two\n  + three\n"),
            "{s}"
        );
    }

    #[test]
    fn a_comment_renders_its_body_and_as_markdown_on_request() {
        let p = Proposal {
            kind: Kind::Comment,
            id: "x".into(),
            args: json!({ "key": "WRK-1", "body": "h2. Done\n*bold*" }),
            current: json!({ "last_comment": null }),
            proposed: json!({ "body": "h2. Done\n*bold*" }),
            ..update()
        };
        let s = render(&p, Render::default());
        assert!(
            s.ends_with("no comments yet\nnew comment:\n  + h2. Done\n  + *bold*\n"),
            "{s}"
        );
        let s = render(
            &p,
            Render {
                color: false,
                markdown: true,
            },
        );
        assert!(s.contains("  + ## Done\n"), "{s}");
        assert!(s.contains("  + **bold**\n"), "{s}");
    }

    #[test]
    fn a_stale_refusal_names_the_issue_and_both_stamps() {
        let e = ApplyError::Stale {
            key: "WRK-12".into(),
            was: "a".into(),
            now: "b".into(),
        };
        assert_eq!(
            e.to_string(),
            "WRK-12 moved since this change was proposed (was a, now b); propose again"
        );
        assert_eq!(Outcome::of(Err(e)).status, Status::Stale);
    }

    /// Creates as Jira would, numbering from WRK-10, refusing any summary
    /// named in `refused`, and keeping every `fields` it was sent.
    async fn run(
        plan: Vec<NewIssue>,
        refused: &[&str],
    ) -> (Result<Value, String>, Vec<Map<String, Value>>) {
        let sent = std::cell::RefCell::new(Vec::new());
        let result = create_in_order(plan, |fields: Map<String, Value>| {
            let refuse = refused.contains(&fields["summary"].as_str().unwrap_or_default());
            sent.borrow_mut().push(fields);
            let key = format!("WRK-{}", 9 + sent.borrow().len());
            async move {
                if refuse {
                    Err("jira refused the new issue (400 Bad Request): []".to_string())
                } else {
                    Ok(json!({ "key": key, "id": "1", "url": format!("https://j/browse/{key}") }))
                }
            }
        })
        .await;
        (result, sent.into_inner())
    }

    fn epic_and_children() -> Value {
        json!({ "issues": [
            { "project": "WRK", "type": "Epic", "summary": "Port the feed" },
            { "project": "WRK", "type": "Story", "summary": "Read it", "parent": "#0" },
            { "project": "WRK", "type": "Story", "summary": "Write it", "parent": "WRK-3",
              "labels": ["etl"] },
        ] })
    }

    #[tokio::test]
    async fn a_child_is_created_under_the_key_its_parent_was_given() {
        let (result, sent) = run(args::create_plan(&epic_and_children()).unwrap(), &[]).await;
        let result = result.unwrap();
        assert_eq!(sent[1]["parent"], json!({ "key": "WRK-10" }));
        assert_eq!(sent[2]["parent"], json!({ "key": "WRK-3" }));
        assert_eq!(
            result,
            json!({
                "created": [
                    { "index": 0, "key": "WRK-10", "url": "https://j/browse/WRK-10" },
                    { "index": 1, "key": "WRK-11", "url": "https://j/browse/WRK-11" },
                    { "index": 2, "key": "WRK-12", "url": "https://j/browse/WRK-12" },
                ],
                "failed": null,
                "skipped": [],
            })
        );
    }

    #[tokio::test]
    async fn a_refusal_stops_the_run_and_the_result_says_what_was_not_created() {
        let (result, sent) = run(
            args::create_plan(&epic_and_children()).unwrap(),
            &["Read it"],
        )
        .await;
        let result = result.unwrap();
        assert_eq!(sent.len(), 2, "nothing is sent after the refusal");
        assert_eq!(
            result["created"],
            json!([{ "index": 0, "key": "WRK-10", "url": "https://j/browse/WRK-10" }])
        );
        assert_eq!(result["failed"]["index"], json!(1));
        assert!(
            result["failed"]["error"]
                .as_str()
                .unwrap()
                .contains("refused")
        );
        assert_eq!(result["skipped"], json!([2]));
    }

    #[tokio::test]
    async fn a_run_that_created_nothing_is_an_error() {
        let (result, sent) = run(
            args::create_plan(&epic_and_children()).unwrap(),
            &["Port the feed"],
        )
        .await;
        let e = result.unwrap_err();
        assert_eq!(sent.len(), 1);
        assert!(e.starts_with("issues[0]: jira refused"), "{e}");
        assert!(
            e.contains("nothing was created; the other 2 were not tried"),
            "{e}"
        );
    }

    #[test]
    fn a_planned_issue_reads_by_name() {
        let plan = args::create_plan(&epic_and_children()).unwrap();
        assert_eq!(
            readable(&plan[1]),
            json!({ "project": "WRK", "type": "Story", "summary": "Read it", "parent": "#0" })
        );
        assert_eq!(
            readable(&plan[2]),
            json!({ "project": "WRK", "type": "Story", "summary": "Write it",
                    "labels": ["etl"], "parent": "WRK-3" })
        );
    }
}
