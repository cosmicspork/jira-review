//! A stateful fake Jira on 127.0.0.1: issues with `updated` stamps that move
//! on every write, transitions with required fields, link types, and a
//! record of every request it was sent.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: String,
    pub auth: Option<String>,
    pub body: Value,
}

#[derive(Default)]
struct Jira {
    issues: BTreeMap<String, Value>,
    transitions: Vec<Value>,
    link_types: Vec<Value>,
    requests: Vec<Request>,
    data_center: bool,
    refuse_write: Vec<(u16, Value)>,
    clock: u32,
    next_issue: u32,
}

impl Jira {
    fn stamp(&mut self) -> String {
        self.clock += 1;
        format!(
            "2026-11-17T{:02}:{:02}:00.000+0000",
            10 + self.clock / 60,
            self.clock % 60
        )
    }

    fn touch(&mut self, key: &str) {
        let stamp = self.stamp();
        if let Some(i) = self.issues.get_mut(key) {
            i["fields"]["updated"] = json!(stamp);
        }
    }

    fn summary_of(&self, key: &str) -> Value {
        let f = &self.issues[key]["fields"];
        json!({ "summary": f["summary"], "status": f["status"] })
    }
}

#[derive(Clone)]
pub struct Fake {
    pub url: String,
    state: Arc<Mutex<Jira>>,
}

fn status(name: &str) -> Value {
    json!({ "name": name, "statusCategory": { "name": name } })
}

fn issue(key: &str, summary: &str, kind: &str, labels: &[&str]) -> Value {
    json!({
        "key": key,
        "fields": {
            "summary": summary,
            "description": null,
            "status": status("To Do"),
            "issuetype": { "name": kind },
            "priority": { "name": "Medium" },
            "assignee": null,
            "labels": labels,
            "parent": null,
            "issuelinks": [],
            "subtasks": [],
            "comment": { "comments": [] },
            "created": "2026-11-01T09:00:00.000+0000",
            "updated": "2026-11-16T09:12:33.000+0000",
        },
    })
}

impl Fake {
    pub async fn start() -> Self {
        let mut jira = Jira::default();
        let mut wrk1 = issue("WRK-1", "Port the feed", "Task", &["etl", "Stale"]);
        wrk1["fields"]["description"] = json!("h2. Goal\nMove the feed.");
        wrk1["fields"]["comment"]["comments"] = json!([{
            "id": "100", "author": { "displayName": "Ada" },
            "created": "2026-11-15T08:00:00.000+0000", "body": "Started on *this*.",
        }]);
        jira.issues.insert("WRK-1".into(), wrk1);
        jira.issues
            .insert("WRK-2".into(), issue("WRK-2", "Clean up", "Task", &[]));
        jira.issues
            .insert("WRK-10".into(), issue("WRK-10", "The epic", "Epic", &[]));
        jira.next_issue = 11;
        jira.transitions = vec![
            json!({ "id": "11", "name": "Start", "to": status("In Progress"), "fields": {} }),
            json!({ "id": "31", "name": "Resolve", "to": status("Done"), "fields": {
                "resolution": { "required": true, "name": "Resolution",
                    "allowedValues": [ { "id": "1", "name": "Fixed" }, { "id": "2", "name": "Won't Do" } ] },
                "comment": { "required": false, "name": "Comment" },
            } }),
        ];
        jira.link_types = vec![
            json!({ "id": "1", "name": "Blocks", "inward": "is blocked by", "outward": "blocks" }),
            json!({ "id": "2", "name": "Relates", "inward": "relates to", "outward": "relates to" }),
        ];
        let state = Arc::new(Mutex::new(jira));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().fallback(handle).with_state(state.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { url, state }
    }

    /// Fields for a Jira Cloud credential pointed at the fake.
    pub fn cloud(&self) -> Vec<(String, String)> {
        vec![
            ("JIRA_URL".into(), self.url.clone()),
            ("JIRA_EMAIL".into(), "me@example.com".into()),
            ("JIRA_TOKEN".into(), "cloud-token".into()),
        ]
    }

    pub fn credential(&self) -> jira_review::auth::Credential {
        jira_review::auth::Credential::from_fields(
            |k| {
                self.cloud()
                    .into_iter()
                    .find(|(name, _)| name == k)
                    .map(|(_, v)| v)
            },
            "fake",
        )
        .unwrap()
    }

    pub fn client(&self) -> jira_review::jira::Jira {
        jira_review::jira::Jira::new(self.credential())
    }

    /// Answer as Data Center does: no v3 search.
    pub fn data_center(&self) {
        self.state.lock().unwrap().data_center = true;
    }

    /// Move `key` on, as a person editing it would.
    pub fn bump(&self, key: &str) {
        self.state.lock().unwrap().touch(key);
    }

    /// Refuse the next write with `status` and `body`; queued in order.
    pub fn refuse_next_write(&self, status: u16, body: Value) {
        self.state.lock().unwrap().refuse_write.push((status, body));
    }

    pub fn issue(&self, key: &str) -> Value {
        self.state.lock().unwrap().issues[key].clone()
    }

    pub fn requests(&self) -> Vec<Request> {
        self.state.lock().unwrap().requests.clone()
    }

    /// Every request that was not a GET.
    pub fn writes(&self) -> Vec<Request> {
        self.requests()
            .into_iter()
            .filter(|r| r.method != "GET")
            .collect()
    }

    pub fn clear(&self) {
        self.state.lock().unwrap().requests.clear();
    }
}

fn answer(status: u16, body: Value) -> Response {
    let status = StatusCode::from_u16(status).unwrap();
    if body.is_null() {
        status.into_response()
    } else {
        (status, axum::Json(body)).into_response()
    }
}

fn missing(key: &str) -> Response {
    answer(
        404,
        json!({ "errorMessages": [format!("Issue {key} does not exist")], "errors": {} }),
    )
}

async fn handle(
    State(state): State<Arc<Mutex<Jira>>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let mut j = state.lock().unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let auth = headers
        .get("authorization")
        .map(|v| v.to_str().unwrap().to_string());
    let path = uri.path().to_string();
    let query = uri.query().unwrap_or_default().to_string();
    j.requests.push(Request {
        method: method.to_string(),
        path: path.clone(),
        query: query.clone(),
        auth: auth.clone(),
        body: body.clone(),
    });
    if auth.is_none() {
        return answer(401, json!({ "errorMessages": ["not logged in"] }));
    }
    if method != Method::GET && !j.refuse_write.is_empty() {
        let (status, body) = j.refuse_write.remove(0);
        return answer(status, body);
    }
    let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let limit = query
        .split('&')
        .find_map(|p| p.strip_prefix("maxResults="))
        .and_then(|n| n.parse().ok())
        .unwrap_or(50);
    match (method.as_str(), parts.as_slice()) {
        ("GET", ["rest", "api", "3", "search", "jql"]) if j.data_center => {
            answer(404, json!({ "errorMessages": ["no such endpoint"] }))
        }
        ("GET", ["rest", "api", "3", "search", "jql"] | ["rest", "api", "2", "search"]) => {
            let issues: Vec<Value> = j.issues.values().take(limit).cloned().collect();
            answer(200, json!({ "issues": issues }))
        }
        ("GET", ["rest", "api", "2", "issue", key]) => match j.issues.get(*key) {
            Some(i) => answer(200, i.clone()),
            None => missing(key),
        },
        ("PUT", ["rest", "api", "2", "issue", key]) => {
            let key = key.to_string();
            let Some(issue) = j.issues.get_mut(&key) else {
                return missing(&key);
            };
            let f = &mut issue["fields"];
            for (k, v) in body["fields"].as_object().into_iter().flatten() {
                f[k] = v.clone();
            }
            for edit in body["update"]["labels"].as_array().into_iter().flatten() {
                let labels = f["labels"].as_array_mut().unwrap();
                if let Some(add) = edit.get("add")
                    && !labels.contains(add)
                {
                    labels.push(add.clone());
                }
                if let Some(remove) = edit.get("remove") {
                    labels.retain(|l| l != remove);
                }
            }
            j.touch(&key);
            answer(204, Value::Null)
        }
        ("POST", ["rest", "api", "2", "issue"]) => {
            let f = &body["fields"];
            let project = f["project"]["key"].as_str().unwrap_or("X").to_string();
            let key = format!("{project}-{}", j.next_issue);
            j.next_issue += 1;
            let mut new = issue(
                &key,
                f["summary"].as_str().unwrap_or_default(),
                f["issuetype"]["name"].as_str().unwrap_or_default(),
                &[],
            );
            for (k, v) in f.as_object().into_iter().flatten() {
                new["fields"][k] = v.clone();
            }
            j.issues.insert(key.clone(), new);
            answer(201, json!({ "id": "9", "key": key, "self": "x" }))
        }
        ("POST", ["rest", "api", "2", "issue", key, "comment"]) => {
            let key = key.to_string();
            if !j.issues.contains_key(&key) {
                return missing(&key);
            }
            let created = j.stamp();
            let comment = json!({ "id": "200", "author": { "displayName": "Me" },
                                  "created": created, "body": body["body"] });
            j.issues.get_mut(&key).unwrap()["fields"]["comment"]["comments"]
                .as_array_mut()
                .unwrap()
                .push(comment);
            j.touch(&key);
            answer(201, json!({ "id": "200", "created": created }))
        }
        ("GET", ["rest", "api", "2", "issue", key, "transitions"]) => {
            if !j.issues.contains_key(*key) {
                return missing(key);
            }
            let expand = query.contains("expand=transitions.fields");
            let list: Vec<Value> = j
                .transitions
                .iter()
                .map(|t| {
                    let mut t = t.clone();
                    if !expand {
                        t.as_object_mut().unwrap().remove("fields");
                    }
                    t
                })
                .collect();
            answer(200, json!({ "transitions": list }))
        }
        ("POST", ["rest", "api", "2", "issue", key, "transitions"]) => {
            let key = key.to_string();
            if !j.issues.contains_key(&key) {
                return missing(&key);
            }
            let id = body["transition"]["id"].as_str().unwrap_or_default();
            let Some(t) = j.transitions.iter().find(|t| t["id"] == id).cloned() else {
                return answer(400, json!({ "errorMessages": ["no such transition"] }));
            };
            j.issues.get_mut(&key).unwrap()["fields"]["status"] = t["to"].clone();
            j.touch(&key);
            answer(204, Value::Null)
        }
        ("GET", ["rest", "api", "2", "issueLinkType"]) => {
            answer(200, json!({ "issueLinkTypes": j.link_types }))
        }
        ("POST", ["rest", "api", "2", "issueLink"]) => {
            let from = body["inwardIssue"]["key"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let to = body["outwardIssue"]["key"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let name = body["type"]["name"].as_str().unwrap_or_default();
            let Some(kind) = j.link_types.iter().find(|t| t["name"] == name).cloned() else {
                return answer(404, json!({ "errorMessages": ["no such link type"] }));
            };
            if !j.issues.contains_key(&from) || !j.issues.contains_key(&to) {
                return missing(&from);
            }
            // Jira lists a link on both issues, each naming the other side.
            let (to_side, from_side) = (j.summary_of(&to), j.summary_of(&from));
            j.issues.get_mut(&from).unwrap()["fields"]["issuelinks"]
                .as_array_mut()
                .unwrap()
                .push(json!({ "type": kind, "outwardIssue": { "key": to, "fields": to_side } }));
            j.issues.get_mut(&to).unwrap()["fields"]["issuelinks"]
                .as_array_mut()
                .unwrap()
                .push(json!({ "type": kind, "inwardIssue": { "key": from, "fields": from_side } }));
            j.touch(&from);
            j.touch(&to);
            answer(201, Value::Null)
        }
        _ => answer(
            404,
            json!({ "errorMessages": [format!("fake has no {method} {path}")] }),
        ),
    }
}
