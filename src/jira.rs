//! A thin Jira REST v2 client: every request goes through one `send`, which
//! applies the credential and reads Jira's refusal. Descriptions and
//! comments travel as wiki markup, as v2 takes them; nothing here converts.

use reqwest::{Method, StatusCode};
use serde_json::{Map, Value, json};

use crate::args::{self, LinkType};
use crate::auth::{Auth, Credential};
use crate::views;

#[derive(Clone)]
pub struct Jira {
    http: reqwest::Client,
    cred: Credential,
}

impl Jira {
    pub fn new(cred: Credential) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(concat!("jira-review/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("an HTTP client with rustls builds");
        Self { http, cred }
    }

    /// The site, without a trailing slash.
    pub fn url(&self) -> &str {
        &self.cred.url
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Result<(StatusCode, Value), String> {
        let mut req = self
            .http
            .request(method, format!("{}{path}", self.cred.url))
            .header(reqwest::header::AUTHORIZATION, self.cred.header())
            .header(reqwest::header::ACCEPT, "application/json")
            .query(query);
        if let Some(body) = body {
            req = req.json(body);
        }
        let res = req.send().await.map_err(|e| format!("jira: {e}"))?;
        let status = res.status();
        // A write often answers 204 with no body: that reads as null.
        let v: Value = res.json().await.unwrap_or(Value::Null);
        Ok((status, v))
    }

    /// `send`, with anything but a 2xx turned into Jira's own refusal.
    async fn expect(
        &self,
        what: &str,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Result<Value, String> {
        let (status, v) = self.send(method, path, query, body).await?;
        if !status.is_success() {
            return Err(refusal(what, status, &v));
        }
        Ok(v)
    }

    /// Search with JQL: one row per issue.
    pub async fn search(&self, jql: &str, limit: u64) -> Result<Value, String> {
        let jql = jql.trim();
        if jql.is_empty() {
            return Err("jql is required".into());
        }
        let limit = limit.clamp(1, 50).to_string();
        let query = [
            ("jql", jql),
            ("fields", views::SEARCH_FIELDS),
            ("maxResults", limit.as_str()),
        ];
        // Cloud answers only the newer endpoint; Data Center only the older
        // one. Both return the same rows.
        let (mut status, mut v) = self
            .send(Method::GET, "/rest/api/3/search/jql", &query, None)
            .await?;
        if status == StatusCode::NOT_FOUND {
            (status, v) = self
                .send(Method::GET, "/rest/api/2/search", &query, None)
                .await?;
        }
        if !status.is_success() {
            return Err(refusal("jira refused the search", status, &v));
        }
        let issues: Vec<Value> = v["issues"]
            .as_array()
            .map(|a| a.iter().map(views::search_row).collect())
            .unwrap_or_default();
        Ok(json!({ "issues": issues }))
    }

    /// An issue as Jira returns it, with only `fields`.
    pub async fn get_issue(&self, key: &str, fields: &str) -> Result<Value, String> {
        self.expect(
            &format!("jira answered for {key}"),
            Method::GET,
            &format!("/rest/api/2/issue/{key}"),
            &[("fields", fields)],
            None,
        )
        .await
    }

    /// What `issue` answers.
    pub async fn issue(&self, key: &str) -> Result<Value, String> {
        Ok(views::issue_view(
            &self.get_issue(key, views::ISSUE_FIELDS).await?,
        ))
    }

    /// When `key` last changed: the version a change is checked against.
    pub async fn updated(&self, key: &str) -> Result<String, String> {
        let v = self.get_issue(key, "updated").await?;
        v["fields"]["updated"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| format!("jira did not say when {key} was updated"))
    }

    /// The transitions `key` offers, with their fields, as Jira lists them.
    pub async fn transitions_raw(&self, key: &str) -> Result<Value, String> {
        self.expect(
            "jira answered",
            Method::GET,
            &format!("/rest/api/2/issue/{key}/transitions"),
            &[("expand", "transitions.fields")],
            None,
        )
        .await
    }

    /// What `transitions` answers.
    pub async fn transitions(&self, key: &str) -> Result<Value, String> {
        Ok(views::transitions_view(
            key,
            &self.transitions_raw(key).await?,
        ))
    }

    /// The instance's link types.
    pub async fn link_types(&self) -> Result<Vec<LinkType>, String> {
        let v = self
            .expect(
                "jira did not list its link types",
                Method::GET,
                "/rest/api/2/issueLinkType",
                &[],
                None,
            )
            .await?;
        Ok(v["issueLinkTypes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|t| {
                Some(LinkType {
                    name: t["name"].as_str()?.to_string(),
                    outward: t["outward"].as_str().unwrap_or_default().to_string(),
                    inward: t["inward"].as_str().unwrap_or_default().to_string(),
                })
            })
            .collect())
    }

    /// The credential's own user.
    pub async fn myself(&self) -> Result<Value, String> {
        self.expect(
            "jira did not say who this credential is",
            Method::GET,
            "/rest/api/2/myself",
            &[],
            None,
        )
        .await
    }

    /// The user `asked` names, among those who can be assigned within
    /// `scope` (`issueKey` or `project`, and its value): `{ "name", "ref" }`,
    /// the display name a reviewer reads and the account Jira takes.
    ///
    /// Cloud finds users by `query` (name or email) and, failing that, by
    /// `accountId`; Data Center by `username` (username, name or email).
    pub async fn assignee(&self, asked: &str, scope: (&str, &str)) -> Result<Value, String> {
        let user = if asked.eq_ignore_ascii_case(args::ME) {
            self.myself().await?
        } else {
            let (by, value) = scope;
            let within = format!("assignable to {value}");
            let find = |param: &'static str| async move {
                self.expect(
                    "jira did not list assignable users",
                    Method::GET,
                    "/rest/api/2/user/assignable/search",
                    &[(by, value), (param, asked), ("maxResults", "20")],
                    None,
                )
                .await
                .map(|v| v.as_array().cloned().unwrap_or_default())
            };
            let mut users = match self.cred.auth {
                Auth::Basic { .. } => find("query").await?,
                Auth::Bearer { .. } => find("username").await?,
            };
            // An account id is not something Cloud's `query` matches.
            if users.is_empty()
                && matches!(self.cred.auth, Auth::Basic { .. })
                && !asked.contains(char::is_whitespace)
                && !asked.contains('@')
            {
                users = find("accountId").await.unwrap_or_default();
            }
            views::pick_user(&users, asked, &within)?.clone()
        };
        let reference = views::user_ref(&user)
            .ok_or_else(|| format!("jira answered for {asked:?} without an account id or name"))?;
        Ok(json!({ "name": user["displayName"], "ref": reference }))
    }

    /// Edit an issue: `fields` set, `update` edits applied.
    pub async fn edit(&self, key: &str, body: &Value) -> Result<(), String> {
        self.expect(
            "jira refused the update",
            Method::PUT,
            &format!("/rest/api/2/issue/{key}"),
            &[],
            Some(body),
        )
        .await
        .map(drop)
    }

    pub async fn comment(&self, key: &str, body: &str) -> Result<Value, String> {
        let v = self
            .expect(
                "jira refused the comment",
                Method::POST,
                &format!("/rest/api/2/issue/{key}/comment"),
                &[],
                Some(&json!({ "body": body })),
            )
            .await?;
        Ok(json!({ "key": key, "id": v["id"], "created": v["created"] }))
    }

    pub async fn transition(&self, key: &str, body: &Value) -> Result<(), String> {
        self.expect(
            "jira refused the transition",
            Method::POST,
            &format!("/rest/api/2/issue/{key}/transitions"),
            &[],
            Some(body),
        )
        .await
        .map(drop)
    }

    /// Link `from` to `to` so that `from` reads it in the type's outward
    /// wording ("from blocks to").
    pub async fn link(&self, kind: &str, from: &str, to: &str) -> Result<(), String> {
        // Jira's create names the sides the opposite way to how an issue
        // reads its own links: `from` shows the link as an `outwardIssue`
        // `to`, which makes `to` the link's outward side. The read-back in
        // `change::apply` is what holds the call to the wording asked for.
        self.expect(
            "jira refused the link",
            Method::POST,
            "/rest/api/2/issueLink",
            &[],
            Some(&json!({
                "type": { "name": kind },
                "inwardIssue": { "key": from },
                "outwardIssue": { "key": to },
            })),
        )
        .await
        .map(drop)
    }

    /// Create one issue: its key, id and link.
    pub async fn create(&self, fields: Map<String, Value>) -> Result<Value, String> {
        let v = self
            .expect(
                "jira refused the new issue",
                Method::POST,
                "/rest/api/2/issue",
                &[],
                Some(&json!({ "fields": fields })),
            )
            .await?;
        let key = v["key"].as_str().unwrap_or_default().to_string();
        Ok(json!({ "key": key, "id": v["id"], "url": format!("{}/browse/{key}", self.cred.url) }))
    }
}

/// What Jira said, including the per-field messages: `errorMessages` carries
/// the general refusal and `errors` the one naming the field that was wrong.
/// A rejected parent or an unknown priority only ever appears in the latter.
pub fn refusal(what: &str, status: StatusCode, v: &Value) -> String {
    let general = v["errorMessages"].to_string();
    let fields = &v["errors"];
    if fields.is_null() || fields.as_object().is_some_and(|o| o.is_empty()) {
        format!("{what} ({status}): {general}")
    } else {
        format!("{what} ({status}): {general} {fields}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_carries_the_field_messages_when_there_are_any() {
        let v = json!({ "errorMessages": [], "errors": { "priority": "unknown" } });
        let e = refusal("jira refused the update", StatusCode::BAD_REQUEST, &v);
        assert_eq!(
            e,
            "jira refused the update (400 Bad Request): [] {\"priority\":\"unknown\"}"
        );
        let v = json!({ "errorMessages": ["no"], "errors": {} });
        assert_eq!(
            refusal("x", StatusCode::NOT_FOUND, &v),
            "x (404 Not Found): [\"no\"]"
        );
    }
}
