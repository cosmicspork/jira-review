//! A transition over several hops: an issue moved to a status its workflow
//! reaches only through others, proposed and approved as one change.
//!
//! Jira lists only the transitions an issue offers from where it is now, so
//! the hops after the first can be seen in advance only in the workflow
//! itself. Cloud shows that to a caller with the *View (read-only)
//! workflow* or *Administer projects* project permission, through
//! `POST /rest/api/3/workflows/preview`; Data Center has no REST read of a
//! workflow's transitions at all. So:
//!
//! - `to_status` names where the issue should end up. A status one
//!   transition away needs nothing more. Further away, the route is the
//!   shortest one through the workflow that starts with a transition the
//!   issue offers now, and it is refused when the workflow cannot be read,
//!   when there is none, or when there are several.
//! - `path` names each hop, as `transition` names one: by id, transition
//!   name, or the status it leads to. Every hop is checked against the
//!   workflow when it can be read; otherwise the first is checked against
//!   what the issue offers, and the rest only as the issue reaches them.
//!
//! Either way, `apply` checks the issue's `updated` stamp once, then before
//! each hop reads where the issue is and what it offers, and stops at the
//! first hop that is no longer what was approved. Nothing is rolled back.

use std::collections::{BTreeMap, VecDeque};

use serde_json::{Map, Value, json};

use crate::args;
use crate::change::{ApplyError, Proposal, Versions};
use crate::jira::Jira;
use crate::views;

/// The most hops one change takes: a route a reviewer can read at once.
pub const MAX_HOPS: usize = 20;
/// How many routes a refusal for an ambiguous `to_status` lists.
const LISTED_ROUTES: usize = 5;

/// Whether a transition's arguments ask for a route rather than one
/// transition, refusing a mix.
pub fn is_route(args: &Value) -> Result<bool, String> {
    let given: Vec<&str> = ["transition", "to_status", "path"]
        .into_iter()
        .filter(|k| args.get(*k).is_some_and(|v| !v.is_null()))
        .collect();
    match given.as_slice() {
        [] => Err(
            "transition is required (an id or a name, as transitions lists them), or \
                   to_status, or path"
                .into(),
        ),
        ["transition"] => Ok(false),
        [_] => Ok(true),
        _ => Err(format!(
            "give one of transition, to_status or path, not {}",
            given.join(" and ")
        )),
    }
}

/// A status as Jira names it: its id, when known, and its name.
#[derive(Debug, Clone, PartialEq)]
struct Status {
    id: Option<String>,
    name: String,
}

impl Status {
    fn of(v: &Value) -> Self {
        Self {
            id: v["id"].as_str().map(str::to_string),
            name: v["name"].as_str().unwrap_or("?").to_string(),
        }
    }

    /// The same status: by id when both have one, else by name.
    fn is(&self, other: &Status) -> bool {
        match (&self.id, &other.id) {
            (Some(a), Some(b)) => a == b,
            _ => self.name.eq_ignore_ascii_case(&other.name),
        }
    }
}

fn eq(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

/// The transitions a `transitions` read lists.
fn offered(raw: &Value) -> Vec<&Value> {
    raw["transitions"]
        .as_array()
        .into_iter()
        .flatten()
        .collect()
}

/// What an issue offers, for a refusal: `id name (to status)` each.
fn listing(offered: &[&Value]) -> String {
    let names: Vec<String> = offered
        .iter()
        .map(|t| {
            format!(
                "{} {} (to {})",
                t["id"].as_str().unwrap_or("?"),
                t["name"].as_str().unwrap_or("?"),
                t["to"]["name"].as_str().unwrap_or("?")
            )
        })
        .collect();
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join(", ")
    }
}

/// The one offered transition `asked` names: by id, else by its name or the
/// name of the status it leads to.
pub fn pick<'a>(offered_raw: &'a Value, asked: &str, key: &str) -> Result<&'a Value, String> {
    let offered = offered(offered_raw);
    let by_id: Vec<&Value> = offered
        .iter()
        .copied()
        .filter(|t| t["id"].as_str() == Some(asked))
        .collect();
    let found: Vec<&Value> = if by_id.is_empty() {
        offered
            .iter()
            .copied()
            .filter(|t| {
                [&t["name"], &t["to"]["name"]]
                    .into_iter()
                    .filter_map(Value::as_str)
                    .any(|n| eq(n, asked))
            })
            .collect()
    } else {
        by_id
    };
    match found.as_slice() {
        [t] => Ok(t),
        [] => Err(format!(
            "{key} offers no transition {asked:?} from where it is now; it offers {}",
            listing(&offered)
        )),
        _ => Err(format!(
            "{asked:?} names more than one of {key}'s transitions; name one by id: {}",
            listing(&offered)
        )),
    }
}

/// Refuse `given` unless the transition takes every field in it and is given
/// every field it requires.
pub fn check_fields(t: &Value, given: &Map<String, Value>) -> Result<(), String> {
    let name = t["name"].as_str().unwrap_or_default();
    let takes: Vec<&String> = t["fields"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(k, _)| k)
        .collect();
    if let Some(extra) = given.keys().find(|k| !takes.contains(k)) {
        let takes = if takes.is_empty() {
            "none".to_string()
        } else {
            takes
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        return Err(format!(
            "transition {name} does not take field {extra}; it takes {takes}"
        ));
    }
    let missing: Vec<String> = views::required_fields(t)
        .into_iter()
        .filter(|f| !f["id"].as_str().is_some_and(|id| given.contains_key(id)))
        .map(|f| describe_field(&f))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "transition {name} requires {}; pass them in fields",
            missing.join("; ")
        ));
    }
    Ok(())
}

/// A required field as a reviewer reads it: id, name and allowed values.
pub fn describe_field(f: &Value) -> String {
    let id = f["id"].as_str().unwrap_or_default();
    let mut s = format!("{id} ({})", f["name"].as_str().unwrap_or(id));
    if let Some(allowed) = f["allowed"].as_array() {
        let names: Vec<String> = allowed
            .iter()
            .map(|a| a.as_str().map_or(a.to_string(), str::to_string))
            .collect();
        s.push_str(&format!(": one of {}", names.join(", ")));
    }
    s
}

fn fields_arg(v: Option<&Value>, what: &str) -> Result<Map<String, Value>, String> {
    match v {
        None | Some(Value::Null) => Ok(Map::new()),
        Some(Value::Object(f)) => Ok(f.clone()),
        Some(_) => Err(format!("{what} expects an object of field id to value")),
    }
}

/// One edge of a workflow: a transition from one status to another, the
/// statuses named by their workflow references.
#[derive(Debug, Clone, PartialEq)]
struct Edge {
    id: String,
    name: String,
    from: String,
    to: String,
    /// A global transition, which leads from every other status.
    global: bool,
}

/// A workflow's statuses and the transitions between them, from Cloud's
/// workflow preview.
#[derive(Debug)]
struct Graph {
    name: String,
    /// Reference to status, for the statuses this workflow uses.
    statuses: BTreeMap<String, (Status, Option<String>)>,
    edges: Vec<Edge>,
}

impl Graph {
    /// The workflow `issue_type` uses, from a `workflows/preview` answer.
    fn from_preview(v: &Value, issue_type: &str) -> Result<Self, String> {
        let workflows: Vec<&Value> = v["workflows"].as_array().into_iter().flatten().collect();
        let for_type: Vec<&Value> = workflows
            .iter()
            .copied()
            .filter(|w| {
                w["queryContext"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .flat_map(|c| c["issueTypes"].as_array().into_iter().flatten())
                    .any(|t| t.as_str() == Some(issue_type))
            })
            .collect();
        let w = match (for_type.as_slice(), workflows.as_slice()) {
            ([w], _) => *w,
            ([], [w]) => *w,
            ([], []) => return Err("jira answered with no workflow".into()),
            _ => return Err("jira answered with more than one workflow for the issue type".into()),
        };
        let mut known = BTreeMap::new();
        for s in v["statuses"].as_array().into_iter().flatten() {
            if let Some(reference) = s["statusReference"].as_str() {
                let raw = s["rawName"].as_str().map(str::to_string);
                known.insert(reference.to_string(), (Status::of(s), raw));
            }
        }
        let mut statuses = BTreeMap::new();
        for s in w["statuses"].as_array().into_iter().flatten() {
            let Some(reference) = s["statusReference"].as_str() else {
                continue;
            };
            let status = known.get(reference).cloned().unwrap_or_else(|| {
                (
                    Status {
                        id: Some(reference.to_string()),
                        name: reference.to_string(),
                    },
                    None,
                )
            });
            statuses.insert(reference.to_string(), status);
        }
        let mut edges = Vec::new();
        for t in w["transitions"].as_array().into_iter().flatten() {
            let (Some(id), Some(to)) = (t["id"].as_str(), t["toStatusReference"].as_str()) else {
                continue;
            };
            let global = t["type"].as_str() == Some("GLOBAL");
            let edge = |from: &str| Edge {
                id: id.to_string(),
                name: t["name"].as_str().unwrap_or_default().to_string(),
                from: from.to_string(),
                to: to.to_string(),
                global,
            };
            match t["type"].as_str() {
                Some("DIRECTED") => {
                    for link in t["links"].as_array().into_iter().flatten() {
                        if let Some(from) = link["fromStatusReference"].as_str() {
                            edges.push(edge(from));
                        }
                    }
                }
                // A global transition leads from every other status.
                Some("GLOBAL") => {
                    for from in statuses.keys().filter(|s| s.as_str() != to) {
                        edges.push(edge(from));
                    }
                }
                // INITIAL creates the issue; it leads from nowhere.
                _ => {}
            }
        }
        Ok(Self {
            name: w["name"].as_str().unwrap_or("?").to_string(),
            statuses,
            edges,
        })
    }

    fn status(&self, reference: &str) -> Status {
        self.statuses
            .get(reference)
            .map(|(s, _)| s.clone())
            .unwrap_or(Status {
                id: Some(reference.to_string()),
                name: reference.to_string(),
            })
    }

    /// The reference of the status `s` is, by id, else by name.
    fn reference_of(&self, s: &Status) -> Option<String> {
        let by = |f: &dyn Fn(&Status, &Option<String>) -> bool| {
            self.statuses
                .iter()
                .find(|(_, (status, raw))| f(status, raw))
                .map(|(r, _)| r.clone())
        };
        if let Some(id) = &s.id
            && let Some(r) = by(&|status, _| status.id.as_deref() == Some(id.as_str()))
        {
            return Some(r);
        }
        by(&|status, raw| {
            eq(&status.name, &s.name) || raw.as_deref().is_some_and(|r| eq(r, &s.name))
        })
    }

    /// The references of the statuses `asked` names: by id, name or the name
    /// before translation.
    fn named(&self, asked: &str) -> Vec<String> {
        self.statuses
            .iter()
            .filter(|(r, (s, raw))| {
                r.as_str() == asked
                    || s.id.as_deref() == Some(asked)
                    || eq(&s.name, asked)
                    || raw.as_deref().is_some_and(|n| eq(n, asked))
            })
            .map(|(r, _)| r.clone())
            .collect()
    }

    fn names(&self) -> String {
        self.statuses
            .values()
            .map(|(s, _)| s.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Every shortest route from `from` to `to` whose first transition
    /// `starts` allows, up to `cap` of them, and how many there are.
    fn shortest(
        &self,
        from: &str,
        to: &str,
        globals: bool,
        starts: &dyn Fn(&Edge) -> bool,
        cap: usize,
    ) -> (Vec<Vec<Edge>>, u64) {
        let usable =
            |e: &Edge| e.to != from && (globals || !e.global) && (e.from != from || starts(e));
        let mut dist: BTreeMap<&str, usize> = BTreeMap::from([(from, 0)]);
        let mut order = vec![from];
        let mut queue = VecDeque::from([from]);
        while let Some(u) = queue.pop_front() {
            for e in self.edges.iter().filter(|e| e.from == u && usable(e)) {
                if !dist.contains_key(e.to.as_str()) {
                    dist.insert(&e.to, dist[u] + 1);
                    order.push(&e.to);
                    queue.push_back(&e.to);
                }
            }
        }
        if !dist.contains_key(to) {
            return (Vec::new(), 0);
        }
        let on_route = |e: &Edge| {
            usable(e)
                && match (dist.get(e.from.as_str()), dist.get(e.to.as_str())) {
                    (Some(a), Some(b)) => a + 1 == *b,
                    _ => false,
                }
        };
        let mut count: BTreeMap<&str, u64> = BTreeMap::from([(from, 1)]);
        for v in order.iter().skip(1) {
            let n = self
                .edges
                .iter()
                .filter(|e| e.to == *v && on_route(e))
                .fold(0u64, |n, e| {
                    n.saturating_add(count.get(e.from.as_str()).copied().unwrap_or(0))
                });
            count.insert(v, n);
        }
        // Walk back from `to`: on the shortest-route graph every step back
        // ends at `from`, so nothing is explored in vain.
        fn back<'g>(
            g: &'g Graph,
            v: &str,
            from: &str,
            on_route: &dyn Fn(&Edge) -> bool,
            tail: &mut Vec<&'g Edge>,
            out: &mut Vec<Vec<Edge>>,
            cap: usize,
        ) {
            if out.len() >= cap {
                return;
            }
            if v == from {
                out.push(tail.iter().rev().map(|e| (*e).clone()).collect());
                return;
            }
            for e in g.edges.iter().filter(|e| e.to == v && on_route(e)) {
                tail.push(e);
                back(g, &e.from, from, on_route, tail, out, cap);
                tail.pop();
            }
        }
        let mut routes = Vec::new();
        back(self, to, from, &on_route, &mut Vec::new(), &mut routes, cap);
        (routes, count[to])
    }

    /// The routes `to_status` takes: the shortest through directed
    /// transitions alone, or only when there is none, the shortest that
    /// also takes global ones. A global transition (to On-hold, say) is a
    /// way out of the flow from anywhere, so a route of the same length
    /// through it is not the one anybody means.
    fn routes(
        &self,
        from: &str,
        to: &str,
        starts: &dyn Fn(&Edge) -> bool,
        cap: usize,
    ) -> (Vec<Vec<Edge>>, u64) {
        let directed = self.shortest(from, to, false, starts, cap);
        if directed.1 > 0 {
            return directed;
        }
        self.shortest(from, to, true, starts, cap)
    }

    fn describe(&self, route: &[Edge]) -> String {
        let mut s = self.status(&route[0].from).name;
        for e in route {
            s.push_str(&format!(
                " -[{} {}]-> {}",
                e.id,
                e.name,
                self.status(&e.to).name
            ));
        }
        s
    }
}

/// One hop of a route as `propose` plans it.
#[derive(Debug, Clone)]
struct Hop {
    from: Option<Status>,
    /// The transition, once known: its id and name.
    transition: Option<(String, String)>,
    /// What the caller named it by, while the transition is not known.
    asked: Option<String>,
    to: Option<Status>,
    fields: Map<String, Value>,
    /// The fields it requires, when the issue offers it now.
    required: Option<Vec<Value>>,
}

impl Hop {
    /// A hop the issue offers now, through transition `t`.
    fn offered(from: &Status, t: &Value, fields: Map<String, Value>) -> Self {
        Self {
            from: Some(from.clone()),
            transition: Some((
                t["id"].as_str().unwrap_or_default().to_string(),
                t["name"].as_str().unwrap_or_default().to_string(),
            )),
            asked: None,
            to: Some(Status::of(&t["to"])),
            fields,
            required: Some(views::required_fields(t)),
        }
    }

    fn of_edge(g: &Graph, e: &Edge) -> Self {
        Self {
            from: Some(g.status(&e.from)),
            transition: Some((e.id.clone(), e.name.clone())),
            asked: None,
            to: Some(g.status(&e.to)),
            fields: Map::new(),
            required: None,
        }
    }

    fn to_json(&self) -> Value {
        let status = |s: &Option<Status>| match s {
            Some(s) => (json!(s.name), json!(s.id)),
            None => (Value::Null, Value::Null),
        };
        let (from, from_id) = status(&self.from);
        let (to, to_id) = status(&self.to);
        let mut v = json!({
            "from": from,
            "from_id": from_id,
            "transition": match &self.transition {
                Some((id, name)) => json!({ "id": id, "name": name }),
                None => Value::Null,
            },
            "to": to,
            "to_id": to_id,
        });
        if let Some(asked) = &self.asked {
            v["asked"] = json!(asked);
        }
        if !self.fields.is_empty() {
            v["fields"] = Value::Object(self.fields.clone());
        }
        if let Some(required) = &self.required {
            v["required_fields"] = json!(required);
        }
        v
    }
}

/// The workflow the issue moves through, when Jira shows it to this
/// credential.
async fn workflow(jira: &Jira, issue: &Value) -> Result<Graph, String> {
    let f = &issue["fields"];
    let (Some(project), Some(kind)) = (f["project"]["id"].as_str(), f["issuetype"]["id"].as_str())
    else {
        return Err("jira did not say the issue's project and type".into());
    };
    let v = jira.workflow_preview(project, kind).await?;
    Graph::from_preview(&v, kind)
}

/// Propose a route: `to_status`, or a `path` of hops. `current` is where the
/// issue is; `proposed` the status it ends in, how the route was found and
/// each hop.
pub async fn propose(args: &Value, jira: &Jira) -> Result<(Versions, Value, Value), String> {
    // `is_route` has made sure any of the others given is null.
    args::only(
        args,
        "transition",
        &["key", "transition", "to_status", "path", "fields"],
    )?;
    let key = args::issue_key(args.get("key"), "key")?;
    let fields = fields_arg(args.get("fields"), "fields")?;
    let issue = jira
        .get_issue(key, "status,updated,project,issuetype")
        .await?;
    let updated = issue["fields"]["updated"]
        .as_str()
        .ok_or_else(|| format!("jira did not say when {key} was updated"))?;
    let versions = Versions::from([(key.to_string(), updated.to_string())]);
    let here = Status::of(&issue["fields"]["status"]);
    let raw = jira.transitions_raw(key).await?;
    let (hops, how, wf) = match args.get("path").filter(|p| !p.is_null()) {
        Some(path) => plan_path(key, path, fields, &here, &raw, &issue, jira).await?,
        None => {
            let target = args
                .get("to_status")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or("to_status expects a status name or id")?;
            plan_to(key, target, fields, &here, &raw, &issue, jira).await?
        }
    };
    let last = hops.last().expect("a route has a hop");
    let mut proposed = json!({
        "status": last.to.as_ref().map(|s| s.name.clone()),
        "route": how,
        "hops": hops.iter().map(Hop::to_json).collect::<Vec<_>>(),
    });
    if let Some(w) = wf {
        proposed["workflow"] = json!(w);
    }
    Ok((versions, json!({ "status": here.name }), proposed))
}

type Planned = (Vec<Hop>, &'static str, Option<String>);

async fn plan_to(
    key: &str,
    target: &str,
    fields: Map<String, Value>,
    here: &Status,
    raw: &Value,
    issue: &Value,
    jira: &Jira,
) -> Result<Planned, String> {
    let offered = offered(raw);
    if here.id.as_deref() == Some(target) || eq(&here.name, target) {
        return Err(format!("{key} is already in {}", here.name));
    }
    let direct: Vec<&Value> = offered
        .iter()
        .copied()
        .filter(|t| {
            t["to"]["id"].as_str() == Some(target)
                || t["to"]["name"].as_str().is_some_and(|n| eq(n, target))
        })
        .collect();
    match direct.as_slice() {
        [t] => {
            check_fields(t, &fields)?;
            return Ok((vec![Hop::offered(here, t, fields)], "offered", None));
        }
        [] => {}
        several => {
            return Err(format!(
                "{key} offers more than one transition to {target}: {}; name one with transition",
                listing(several)
            ));
        }
    }
    let g = workflow(jira, issue).await.map_err(|e| {
        format!(
            "{key} offers no transition to {target} from {}, and its workflow could not be read \
             to find a route ({e}). Name each hop with path instead, e.g. path: [\"<next status>\", \
             ..., \"{target}\"]; {key} offers now: {}",
            here.name,
            listing(&offered)
        )
    })?;
    let from = g.reference_of(here).ok_or_else(|| {
        format!(
            "{key} is in {}, which its workflow {} does not list",
            here.name, g.name
        )
    })?;
    let to = match g.named(target).as_slice() {
        [to] => to.clone(),
        [] => {
            return Err(format!(
                "{target} is not a status in {key}'s workflow {}; its statuses are {}",
                g.name,
                g.names()
            ));
        }
        _ => return Err(format!("{target} names more than one status in {}", g.name)),
    };
    let starts = |e: &Edge| offered.iter().any(|t| t["id"].as_str() == Some(&e.id));
    let (routes, count) = g.routes(&from, &to, &starts, LISTED_ROUTES);
    let route = match routes.as_slice() {
        [] => {
            return Err(format!(
                "{key}'s workflow {} has no route from {} to {target} that starts with a \
                 transition {key} offers now ({})",
                g.name,
                here.name,
                listing(&offered)
            ));
        }
        [route] if count == 1 => route,
        _ => {
            let mut list: Vec<String> = routes.iter().map(|r| g.describe(r)).collect();
            if count > routes.len() as u64 {
                list.push(format!("and {} more", count - routes.len() as u64));
            }
            return Err(format!(
                "{key}'s workflow {} has {count} shortest routes from {} to {target}; name the \
                 hops of one with path: {}",
                g.name,
                here.name,
                list.join("; ")
            ));
        }
    };
    if route.len() > MAX_HOPS {
        return Err(format!(
            "the route to {target} takes {} hops, more than {MAX_HOPS}",
            route.len()
        ));
    }
    let first = offered
        .iter()
        .find(|t| t["id"].as_str() == Some(&route[0].id))
        .expect("the first hop is offered");
    let mut hops: Vec<Hop> = route.iter().map(|e| Hop::of_edge(&g, e)).collect();
    hops[0] = Hop::offered(here, first, Map::new());
    // Fields go with the hop into the status asked for.
    if hops.len() > 1
        && let Some(required) = hops[0].required.as_ref().filter(|r| !r.is_empty())
    {
        let names: Vec<String> = required.iter().map(describe_field).collect();
        return Err(format!(
            "the first hop, transition {}, requires {}; name the hops with path and give that \
             hop its fields",
            first["name"].as_str().unwrap_or_default(),
            names.join("; ")
        ));
    }
    let last = hops.len() - 1;
    if last == 0 {
        check_fields(first, &fields)?;
    }
    hops[last].fields = fields;
    Ok((hops, "workflow", Some(g.name)))
}

/// A hop of a `path`: what names it and the fields it is given.
type PathItem = (String, Map<String, Value>);

/// Each hop of a `path`: what names it and the fields it is given.
fn path_items(path: &Value, mut fields: Map<String, Value>) -> Result<Vec<PathItem>, String> {
    let items = path
        .as_array()
        .filter(|a| !a.is_empty())
        .ok_or("path expects a list of hops: transition ids or names, or statuses")?;
    if items.len() > MAX_HOPS {
        return Err(format!("path takes at most {MAX_HOPS} hops"));
    }
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let named = |v: Option<&Value>| {
            v.and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .ok_or_else(|| {
                    format!(
                        "path[{i}] expects a transition id or name, a status, or {{transition, \
                         fields}}"
                    )
                })
        };
        out.push(match item {
            Value::String(_) => (named(Some(item))?, Map::new()),
            Value::Object(o) => {
                args::only(item, &format!("path[{i}]"), &["transition", "fields"])?;
                (
                    named(o.get("transition"))?,
                    fields_arg(o.get("fields"), &format!("path[{i}].fields"))?,
                )
            }
            _ => return Err(named(None).unwrap_err()),
        });
    }
    // Fields given beside the path go with its last hop.
    if !fields.is_empty() {
        let last = out.last_mut().expect("a path has a hop");
        if !last.1.is_empty() {
            return Err("give the last hop's fields either in path or in fields, not both".into());
        }
        std::mem::swap(&mut last.1, &mut fields);
    }
    Ok(out)
}

async fn plan_path(
    key: &str,
    path: &Value,
    fields: Map<String, Value>,
    here: &Status,
    raw: &Value,
    issue: &Value,
    jira: &Jira,
) -> Result<Planned, String> {
    let mut items = path_items(path, fields)?.into_iter();
    let (asked, given) = items.next().expect("a path has a hop");
    let first = pick(raw, &asked, key).map_err(|e| format!("path[0]: {e}"))?;
    check_fields(first, &given).map_err(|e| format!("path[0]: {e}"))?;
    let mut hops = vec![Hop::offered(here, first, given)];
    let rest: Vec<PathItem> = items.collect();
    if rest.is_empty() {
        return Ok((hops, "path", None));
    }
    let g = match workflow(jira, issue).await {
        Ok(g) => g,
        // Without the workflow, what follows the first hop is known only
        // once the issue gets there.
        Err(_) => {
            for (asked, fields) in rest {
                let from = hops.last().and_then(|h| h.to.clone());
                hops.push(Hop {
                    from,
                    transition: None,
                    asked: Some(asked),
                    to: None,
                    fields,
                    required: None,
                });
            }
            return Ok((hops, "path", None));
        }
    };
    for (i, (asked, fields)) in rest.into_iter().enumerate() {
        let at = hops.last().and_then(|h| h.to.clone()).expect("a known hop");
        let from = g.reference_of(&at).ok_or_else(|| {
            format!(
                "path[{}]: {} is not a status in {key}'s workflow {}",
                i + 1,
                at.name,
                g.name
            )
        })?;
        let leaving: Vec<&Edge> = g.edges.iter().filter(|e| e.from == from).collect();
        let by_id: Vec<&Edge> = leaving.iter().copied().filter(|e| e.id == asked).collect();
        let found = if by_id.is_empty() {
            leaving
                .iter()
                .copied()
                .filter(|e| eq(&e.name, &asked) || g.named(&asked).contains(&e.to))
                .collect()
        } else {
            by_id
        };
        let list = || {
            let names: Vec<String> = leaving
                .iter()
                .map(|e| format!("{} {} (to {})", e.id, e.name, g.status(&e.to).name))
                .collect();
            if names.is_empty() {
                "none".into()
            } else {
                names.join(", ")
            }
        };
        let e = match found.as_slice() {
            [e] => *e,
            [] => {
                return Err(format!(
                    "path[{}]: workflow {} has no transition {asked:?} from {}; it has {}",
                    i + 1,
                    g.name,
                    at.name,
                    list()
                ));
            }
            _ => {
                return Err(format!(
                    "path[{}]: {asked:?} names more than one transition from {}; name one by \
                     id: {}",
                    i + 1,
                    at.name,
                    list()
                ));
            }
        };
        let mut hop = Hop::of_edge(&g, e);
        hop.fields = fields;
        hops.push(hop);
    }
    Ok((hops, "path", Some(g.name)))
}

/// What identifies a route: each hop's statuses and transition, or what
/// named it while it is not known. An approved call is applied only along
/// the route its card showed.
pub fn identity(p: &Proposal) -> Option<Value> {
    let hops = p.proposed["hops"].as_array()?;
    let pick = |a: &Value, b: &Value| if a.is_null() { b.clone() } else { a.clone() };
    Some(json!(
        hops.iter()
            .map(|h| json!([
                pick(&h["from_id"], &h["from"]),
                pick(&h["transition"]["id"], &h["asked"]),
                pick(&h["to_id"], &h["to"]),
            ]))
            .collect::<Vec<_>>()
    ))
}

/// The checks `apply` makes of a route before sending anything.
pub fn validate(p: &Proposal) -> Result<(), String> {
    args::issue_key(p.args.get("key"), "key")?;
    let hops = p.proposed["hops"]
        .as_array()
        .filter(|h| !h.is_empty())
        .ok_or("the change names no hops")?;
    if hops.len() > MAX_HOPS {
        return Err(format!("the change has more than {MAX_HOPS} hops"));
    }
    for (i, h) in hops.iter().enumerate() {
        if !h["transition"]["id"].is_string() && !h["asked"].is_string() {
            return Err(format!("hop {} names no transition", i + 1));
        }
    }
    Ok(())
}

fn stored(v: &Value, id: &str, name: &str) -> Option<Status> {
    v[name].as_str().map(|n| Status {
        id: v[id].as_str().map(str::to_string),
        name: n.to_string(),
    })
}

/// A hop as a result or a refusal shows it: its statuses and transition.
fn shown(h: &Value) -> Value {
    let transition = if h["transition"].is_object() {
        json!({ "id": h["transition"]["id"], "name": h["transition"]["name"] })
    } else {
        json!({ "asked": h["asked"] })
    };
    json!({ "from": h["from"], "transition": transition, "to": h["to"] })
}

/// Send one hop, having read where the issue is and what it offers: what
/// was done, or why it stopped (with what the hop looked like then).
async fn step(key: &str, hop: &Value, jira: &Jira) -> Result<Value, (Value, String)> {
    let mut record = shown(hop);
    record["ok"] = json!(false);
    let stop = |record: &Value, why: String| Err((record.clone(), why));
    let issue = match jira.get_issue(key, "status").await {
        Ok(i) => i,
        Err(e) => return stop(&record, e),
    };
    let now = Status::of(&issue["fields"]["status"]);
    record["from"] = json!(now.name);
    if let Some(from) = stored(hop, "from_id", "from")
        && !now.is(&from)
    {
        return stop(
            &record,
            format!(
                "{key} is in {}, not {}, so this hop was not sent",
                now.name, from.name
            ),
        );
    }
    let raw = match jira.transitions_raw(key).await {
        Ok(r) => r,
        Err(e) => return stop(&record, e),
    };
    let t = match hop["transition"]["id"].as_str() {
        Some(id) => match offered(&raw)
            .into_iter()
            .find(|t| t["id"].as_str() == Some(id))
        {
            Some(t) => t,
            None => {
                return stop(
                    &record,
                    format!(
                        "{key} no longer offers transition {id} ({}) from {}; it offers {}; \
                         this hop was not sent",
                        hop["transition"]["name"].as_str().unwrap_or("?"),
                        now.name,
                        listing(&offered(&raw))
                    ),
                );
            }
        },
        None => match pick(&raw, hop["asked"].as_str().unwrap_or_default(), key) {
            Ok(t) => t,
            Err(e) => return stop(&record, format!("{e}; this hop was not sent")),
        },
    };
    record["transition"] = json!({ "id": t["id"], "name": t["name"] });
    let to = Status::of(&t["to"]);
    if let Some(planned) = stored(hop, "to_id", "to")
        && !to.is(&planned)
    {
        return stop(
            &record,
            format!(
                "transition {} now leads to {}, not {}; this hop was not sent",
                t["id"].as_str().unwrap_or("?"),
                to.name,
                planned.name
            ),
        );
    }
    record["to"] = json!(to.name);
    let fields = hop["fields"].as_object().cloned().unwrap_or_default();
    if let Err(e) = check_fields(t, &fields) {
        return stop(&record, format!("{e}; this hop was not sent"));
    }
    let mut body = json!({ "transition": { "id": t["id"] } });
    if !fields.is_empty() {
        body["fields"] = Value::Object(fields);
    }
    if let Err(e) = jira.transition(key, &body).await {
        return stop(&record, e);
    }
    record["ok"] = json!(true);
    Ok(record)
}

/// Apply a route whose stale check has passed: each hop in order, stopping
/// at the first that cannot be sent as approved. Nothing is rolled back.
///
/// The result says where the issue is now, as Jira reads it after the last
/// hop sent, each hop tried, and when it stopped, which hop stopped it and
/// what was not tried. A route that moved nothing is an error.
pub async fn apply(p: &Proposal, jira: &Jira) -> Result<Value, ApplyError> {
    let key = args::issue_key(p.args.get("key"), "key").map_err(ApplyError::Invalid)?;
    let planned = p.proposed["hops"].as_array().cloned().unwrap_or_default();
    let mut hops = Vec::new();
    let mut stopped = None;
    for (i, hop) in planned.iter().enumerate() {
        match step(key, hop, jira).await {
            Ok(done) => hops.push(done),
            Err((tried, why)) => {
                let mut tried = tried;
                tried["error"] = json!(why);
                hops.push(tried);
                stopped = Some(i);
                break;
            }
        }
    }
    let read = jira.get_issue(key, "status").await;
    let final_status = match &read {
        Ok(v) => v["fields"]["status"]["name"].clone(),
        Err(_) => hops
            .iter()
            .rev()
            .find(|h| h["ok"] == json!(true))
            .map_or_else(|| p.current["status"].clone(), |h| h["to"].clone()),
    };
    let n = planned.len();
    if stopped == Some(0) {
        let why = hops[0]["error"].as_str().unwrap_or_default();
        return Err(ApplyError::Jira(format!(
            "hop 1 of {n} stopped: {why}; nothing was moved, and {key} is in {}",
            final_status.as_str().unwrap_or("?")
        )));
    }
    let mut v = json!({ "key": key, "final_status": final_status, "hops": hops });
    if let Err(e) = read {
        v["final_status_unconfirmed"] = json!(e);
    }
    if let Some(i) = stopped {
        v["stopped_at"] = json!(i);
        v["not_tried"] = json!(planned[i + 1..].iter().map(shown).collect::<Vec<_>>());
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// To Do → Requirements Gathering → In Progress → UAT → Rollout → Done,
    /// plus Canceled and On-hold from anywhere, and back from On-hold to In
    /// Progress.
    fn strict() -> Value {
        let names = [
            ("1", "To Do"),
            ("2", "Requirements Gathering"),
            ("3", "In Progress"),
            ("4", "UAT"),
            ("5", "Rollout"),
            ("6", "Done"),
            ("7", "Canceled"),
            ("8", "On-hold"),
        ];
        let directed = |id: &str, name: &str, from: &str, to: &str| {
            json!({ "id": id, "name": name, "type": "DIRECTED", "toStatusReference": to,
                    "links": [{ "fromStatusReference": from, "fromPort": 0, "toPort": 0 }] })
        };
        json!({
            "statuses": names.iter().map(|(id, n)| json!({
                "id": id, "name": n, "statusReference": id })).collect::<Vec<_>>(),
            "workflows": [{
                "name": "Strict",
                "queryContext": [{ "project": "10000", "issueTypes": ["10001"] }],
                "statuses": names.iter().map(|(id, _)| json!({ "statusReference": id }))
                    .collect::<Vec<_>>(),
                "transitions": [
                    { "id": "1", "name": "Create", "type": "INITIAL", "toStatusReference": "1",
                      "links": [] },
                    directed("11", "Gather", "1", "2"),
                    directed("21", "Start", "2", "3"),
                    directed("31", "Test", "3", "4"),
                    directed("41", "Roll out", "4", "5"),
                    directed("51", "Finish", "5", "6"),
                    directed("81", "Resume", "8", "3"),
                    { "id": "61", "name": "Cancel", "type": "GLOBAL", "toStatusReference": "7",
                      "links": [] },
                    { "id": "71", "name": "Hold", "type": "GLOBAL", "toStatusReference": "8",
                      "links": [] },
                ],
            }],
        })
    }

    fn ids(route: &[Edge]) -> Vec<&str> {
        route.iter().map(|e| e.id.as_str()).collect()
    }

    #[test]
    fn a_strict_chain_has_one_shortest_route() {
        let g = Graph::from_preview(&strict(), "10001").unwrap();
        // On hold and back to In Progress is as short, but not the flow.
        let (_, count) = g.shortest("1", "6", true, &|_| true, 5);
        assert_eq!(count, 2);
        let (routes, count) = g.routes("1", "6", &|_| true, 5);
        assert_eq!(count, 1);
        assert_eq!(ids(&routes[0]), ["11", "21", "31", "41", "51"]);
        assert_eq!(
            g.describe(&routes[0]),
            "To Do -[11 Gather]-> Requirements Gathering -[21 Start]-> In Progress \
             -[31 Test]-> UAT -[41 Roll out]-> Rollout -[51 Finish]-> Done"
        );
    }

    #[test]
    fn a_global_transition_leads_from_every_other_status() {
        let g = Graph::from_preview(&strict(), "10001").unwrap();
        let (routes, count) = g.routes("4", "3", &|_| true, 5);
        assert_eq!(count, 1);
        assert_eq!(ids(&routes[0]), ["71", "81"]);
        // The route starts only with a transition the issue offers.
        let (routes, count) = g.routes("4", "3", &|_| false, 5);
        assert!(routes.is_empty());
        assert_eq!(count, 0);
    }

    #[test]
    fn parallel_transitions_make_several_routes_and_all_are_counted() {
        let mut wf = strict();
        wf["workflows"][0]["transitions"]
            .as_array_mut()
            .unwrap()
            .push(
                json!({ "id": "22", "name": "Skip ahead", "type": "DIRECTED",
                          "toStatusReference": "3",
                          "links": [{ "fromStatusReference": "2" }] }),
            );
        let g = Graph::from_preview(&wf, "10001").unwrap();
        let (routes, count) = g.routes("1", "6", &|_| true, 1);
        assert_eq!(count, 2);
        assert_eq!(routes.len(), 1, "listing stops at the cap");
    }

    #[test]
    fn a_status_is_found_by_id_name_or_raw_name() {
        let mut wf = strict();
        wf["statuses"][5]["name"] = json!("Fertig");
        wf["statuses"][5]["rawName"] = json!("Done");
        let g = Graph::from_preview(&wf, "10001").unwrap();
        assert_eq!(g.named("done"), ["6"]);
        assert_eq!(g.named("Fertig"), ["6"]);
        assert_eq!(g.named("6"), ["6"]);
        let here = Status {
            id: Some("2".into()),
            name: "whatever".into(),
        };
        assert_eq!(g.reference_of(&here).as_deref(), Some("2"));
    }

    #[test]
    fn a_route_is_asked_for_by_one_of_three_arguments() {
        assert!(!is_route(&json!({ "transition": "Start" })).unwrap());
        assert!(is_route(&json!({ "to_status": "Done" })).unwrap());
        assert!(is_route(&json!({ "path": ["Start"], "transition": null })).unwrap());
        let e = is_route(&json!({ "transition": "Start", "to_status": "Done" })).unwrap_err();
        assert_eq!(
            e,
            "give one of transition, to_status or path, not transition and to_status"
        );
        assert!(
            is_route(&json!({}))
                .unwrap_err()
                .contains("to_status, or path")
        );
    }

    #[test]
    fn path_fields_go_with_the_last_hop_unless_it_has_its_own() {
        let fields = Map::from_iter([("resolution".to_string(), json!({ "name": "Fixed" }))]);
        let items =
            path_items(&json!(["Start", { "transition": "Done" }]), fields.clone()).unwrap();
        assert_eq!(items[1].1, fields);
        assert!(items[0].1.is_empty());
        let e = path_items(
            &json!([{ "transition": "Done", "fields": { "x": 1 } }]),
            fields,
        )
        .unwrap_err();
        assert!(e.contains("not both"), "{e}");
        assert!(path_items(&json!([]), Map::new()).is_err());
        let e = path_items(&json!([7]), Map::new()).unwrap_err();
        assert!(e.starts_with("path[0] expects"), "{e}");
    }
}
