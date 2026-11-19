//! What the reads answer, shaped from Jira's issue JSON as it came back.

use serde_json::{Value, json};

use crate::args::LinkType;

/// What a search row carries: enough to pick an issue, not to read it.
pub const SEARCH_FIELDS: &str = "summary,status,assignee,duedate,priority,issuetype,parent,labels";

/// What `issue` reads: enough to write to the issue safely, since a label
/// list, a parent or a link is what a write would otherwise have to guess.
pub const ISSUE_FIELDS: &str = "summary,status,assignee,duedate,description,comment,issuetype,\
                                priority,labels,parent,issuelinks,subtasks,created,updated";

/// What an `update` is shown against.
pub const CURRENT_FIELDS: &str =
    "summary,description,priority,assignee,duedate,labels,parent,updated";

/// What a paged `search` reads.
pub const PAGE_FIELDS: &str = "summary,status,priority,issuetype,labels,parent";

/// One row of a paged `search`, as Tracon's `GET /api/jira/search` answers it.
pub fn page_row(i: &Value) -> Value {
    let f = &i["fields"];
    let p = &f["parent"];
    json!({
        "id": i["id"],
        "key": i["key"],
        "summary": f["summary"],
        "status": f["status"]["name"],
        "status_category": f["status"]["statusCategory"]["key"],
        "priority": f["priority"]["name"],
        "issue_type": f["issuetype"]["name"],
        "labels": f["labels"].as_array().cloned().unwrap_or_default(),
        "parent": if p.is_object() {
            json!({
                "key": p["key"],
                "summary": p["fields"]["summary"],
                "issue_type": p["fields"]["issuetype"]["name"],
            })
        } else {
            Value::Null
        },
    })
}

/// One `search` row.
pub fn search_row(i: &Value) -> Value {
    let f = &i["fields"];
    json!({
        "key": i["key"],
        "type": f["issuetype"]["name"],
        "summary": f["summary"],
        "status": f["status"]["name"],
        "priority": f["priority"]["name"],
        "assignee": f["assignee"]["displayName"],
        "duedate": f["duedate"],
        "parent": f["parent"]["key"],
        "labels": labels(f),
    })
}

/// What `issue` answers, from Jira's issue as it came back.
pub fn issue_view(v: &Value) -> Value {
    let f = &v["fields"];
    let comments: Vec<Value> = f["comment"]["comments"]
        .as_array()
        .map(|c| {
            c.iter()
                .rev()
                .take(10)
                .map(|c| {
                    json!({
                        "author": c["author"]["displayName"],
                        "created": c["created"],
                        "body": c["body"],
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let subtasks: Vec<Value> = f["subtasks"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|t| {
                    json!({
                        "key": t["key"],
                        "summary": t["fields"]["summary"],
                        "status": t["fields"]["status"]["name"],
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let p = &f["parent"];
    json!({
        "key": v["key"],
        "type": f["issuetype"]["name"],
        "summary": f["summary"],
        "status": f["status"]["name"],
        "priority": f["priority"]["name"],
        "assignee": f["assignee"]["displayName"],
        "duedate": f["duedate"],
        "labels": labels(f),
        "parent": if p.is_object() {
            json!({ "key": p["key"], "summary": p["fields"]["summary"] })
        } else {
            Value::Null
        },
        "links": links(f),
        "subtasks": subtasks,
        "created": f["created"],
        "updated": f["updated"],
        "description": f["description"],
        "comments": comments,
    })
}

/// The fields `update` writes, as the issue has them now, each in the shape
/// `update` takes it, so a change compares like with like. The assignee is
/// shown by name, and `assignee_ref` is the account Jira takes, so that two
/// people with one name still compare as different.
pub fn current_view(v: &Value) -> Value {
    let f = &v["fields"];
    json!({
        "summary": f["summary"],
        "description": f["description"],
        "priority": f["priority"]["name"],
        "assignee": f["assignee"]["displayName"],
        "assignee_ref": user_ref(&f["assignee"]).unwrap_or(Value::Null),
        "duedate": f["duedate"],
        "labels": labels(f),
        "parent": f["parent"]["key"],
    })
}

/// How Jira takes a user in a write: Cloud by `accountId`, Data Center by
/// `name`. `None` for anything that is not a user.
pub fn user_ref(user: &Value) -> Option<Value> {
    if let Some(id) = user["accountId"].as_str() {
        return Some(json!({ "accountId": id }));
    }
    user["name"].as_str().map(|n| json!({ "name": n }))
}

/// A user as a reviewer tells them apart: the name and whatever else Jira
/// shows of them.
pub fn describe_user(user: &Value) -> String {
    let name = user["displayName"].as_str().unwrap_or("?");
    let id = ["emailAddress", "name", "accountId"]
        .into_iter()
        .find_map(|k| user[k].as_str().filter(|s| !s.is_empty()));
    match id {
        Some(id) => format!("{name} ({id})"),
        None => name.to_string(),
    }
}

/// The one user of `users` that `asked` names: the one it matches exactly
/// (account id, username, key, email or display name, ignoring case), or the
/// only one Jira found. Anything else is refused with who was found, so the
/// caller can name one exactly.
pub fn pick_user<'a>(users: &'a [Value], asked: &str, scope: &str) -> Result<&'a Value, String> {
    let exact: Vec<&Value> = users
        .iter()
        .filter(|u| {
            ["accountId", "name", "key", "emailAddress", "displayName"]
                .into_iter()
                .filter_map(|k| u[k].as_str())
                .any(|v| v.eq_ignore_ascii_case(asked))
        })
        .collect();
    let found: Vec<&Value> = match (exact.as_slice(), users) {
        ([one], _) => return Ok(one),
        ([], [one]) => return Ok(one),
        ([], _) => users.iter().collect(),
        (some, _) => some.to_vec(),
    };
    if found.is_empty() {
        return Err(format!("no user {scope} matches {asked:?}"));
    }
    let names: Vec<String> = found.iter().take(10).map(|u| describe_user(u)).collect();
    let more = if found.len() > names.len() {
        ", …"
    } else {
        ""
    };
    Err(format!(
        "{asked:?} matches more than one user {scope}: {}{more}; name one by email, \
         username or account id",
        names.join(", ")
    ))
}

/// An issue's links to other issues, each read from this issue's side.
///
/// Jira lists a link on each of its two issues, naming the other one as the
/// side it is on: `outwardIssue` X on this issue reads "this blocks X",
/// `inwardIssue` X reads "this is blocked by X".
pub fn links(fields: &Value) -> Vec<Value> {
    fields["issuelinks"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|l| {
                    let (direction, other) = if l["outwardIssue"].is_object() {
                        ("outward", &l["outwardIssue"])
                    } else if l["inwardIssue"].is_object() {
                        ("inward", &l["inwardIssue"])
                    } else {
                        return None;
                    };
                    Some(json!({
                        "type": l["type"]["name"],
                        "direction": direction,
                        "label": l["type"][direction],
                        "key": other["key"],
                        "summary": other["fields"]["summary"],
                        "status": other["fields"]["status"]["name"],
                    }))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// What `link` answers, from `from`'s links as Jira reads them after the
/// create: the link toward `to` in the type's outward wording, or an error
/// when it reads the other way. A reversed link is left for a person to
/// remove; deleting it here would be a second write nobody reviewed.
pub fn linked(links: &[Value], from: &str, to: &str, kind: &LinkType) -> Result<Value, String> {
    let toward: Vec<&Value> = links
        .iter()
        .filter(|l| {
            l["type"].as_str() == Some(kind.name.as_str())
                && l["key"]
                    .as_str()
                    .is_some_and(|k| k.eq_ignore_ascii_case(to))
        })
        .collect();
    // A type that reads the same both ways ("relates to") has no wrong way.
    let symmetric = kind.inward == kind.outward;
    if let Some(l) = toward
        .iter()
        .find(|l| symmetric || l["direction"] == "outward")
    {
        return Ok(json!({ "from": from, "to": to, "type": kind.name, "label": l["label"] }));
    }
    if !toward.is_empty() {
        return Err(format!(
            "jira created the {} link reversed: {from} {} {to}, not {from} {} {to}. Nothing \
             was removed; delete it from either issue.",
            kind.name, kind.inward, kind.outward
        ));
    }
    Err(format!(
        "jira accepted the {} link but {from} does not show it toward {to}",
        kind.name
    ))
}

/// The transitions an issue offers, from Jira's
/// `transitions?expand=transitions.fields`: each one's id, name, the status
/// it leads to, and the fields it requires.
pub fn transitions_view(key: &str, v: &Value) -> Value {
    let transitions: Vec<Value> = v["transitions"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|t| {
                    json!({
                        "id": t["id"],
                        "name": t["name"],
                        "to": t["to"]["name"],
                        "category": t["to"]["statusCategory"]["name"],
                        "required_fields": required_fields(t),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    json!({ "key": key, "transitions": transitions })
}

/// The fields a transition marks `required`, each with the names of the
/// values it allows when Jira lists them.
pub fn required_fields(t: &Value) -> Vec<Value> {
    t["fields"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, f)| f["required"].as_bool() == Some(true))
        .map(|(id, f)| {
            let mut field = json!({ "id": id, "name": f["name"] });
            if let Some(allowed) = f["allowedValues"].as_array() {
                let names: Vec<&Value> = allowed
                    .iter()
                    .map(|a| {
                        [&a["name"], &a["value"]]
                            .into_iter()
                            .find(|v| v.is_string())
                            .unwrap_or(&a["id"])
                    })
                    .collect();
                field["allowed"] = json!(names);
            }
            field
        })
        .collect()
}

/// Where the transition `id` in a `transitions` listing leads: the status,
/// or the transition's own name when the status is not given.
pub fn target_of(listed: &Value, id: &str) -> Option<String> {
    let found = listed["transitions"]
        .as_array()?
        .iter()
        .find(|t| t["id"].as_str() == Some(id))?;
    [&found["to"], &found["name"]]
        .into_iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .find(|name| !name.is_empty())
        .map(str::to_string)
}

/// An issue's labels, or none when Jira leaves the field out.
pub fn labels(fields: &Value) -> Value {
    json!(fields["labels"].as_array().cloned().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_transition_is_named_by_the_status_it_leads_to() {
        let listed = json!({ "key": "WRK-1", "transitions": [
            { "id": "21", "name": "Start review", "to": "In Review" },
            { "id": "31", "name": "Close", "to": null },
            { "id": "41", "name": " ", "to": "" },
        ] });
        assert_eq!(target_of(&listed, "21").as_deref(), Some("In Review"));
        assert_eq!(target_of(&listed, "31").as_deref(), Some("Close"));
        assert_eq!(target_of(&listed, "41"), None);
        assert_eq!(target_of(&listed, "99"), None);
        assert_eq!(target_of(&json!({}), "21"), None);
    }

    /// Trimmed from a real v2 issue: one link each way, a parent, labels and
    /// a subtask, with the fields Jira sends that `issue` does not keep.
    pub(crate) fn linked_issue() -> Value {
        let status = |name: &str| json!({ "name": name, "statusCategory": { "key": "new" } });
        json!({
            "key": "WRK-16",
            "fields": {
                "summary": "Replace the scripts",
                "status": status("In Progress"),
                "issuetype": { "name": "Task" },
                "priority": { "name": "Minor" },
                "assignee": { "accountId": "5b10a2844c20165700ede21g",
                              "displayName": "Ada Lovelace", "active": true },
                "duedate": "2026-12-01",
                "description": "h2. Goal",
                "labels": ["etl", "fusion"],
                "parent": { "id": "1", "key": "WRK-10", "fields": {
                    "summary": "The epic", "status": status("Open"),
                    "issuetype": { "name": "Epic" } } },
                "issuelinks": [
                    { "id": "1", "type": { "id": "10000", "name": "Blocks",
                        "inward": "is blocked by", "outward": "blocks" },
                      "outwardIssue": { "key": "WRK-20", "fields": {
                        "summary": "Clean up", "status": status("Open") } } },
                    { "id": "2", "type": { "id": "10000", "name": "Blocks",
                        "inward": "is blocked by", "outward": "blocks" },
                      "inwardIssue": { "key": "WRK-15", "fields": {
                        "summary": "Add a flavor", "status": status("To Do") } } },
                ],
                "subtasks": [ { "id": "3", "key": "WRK-17", "fields": {
                    "summary": "Port one", "status": status("Done"),
                    "issuetype": { "name": "Subtask" } } } ],
                "created": "2025-10-21T13:13:10.850-0500",
                "updated": "2026-08-13T15:51:11.203-0500",
                "comment": { "comments": [] },
            },
        })
    }

    #[test]
    fn an_issue_carries_what_a_write_to_it_needs() {
        let view = issue_view(&linked_issue());
        assert_eq!(view["labels"], json!(["etl", "fusion"]));
        assert_eq!(
            view["parent"],
            json!({ "key": "WRK-10", "summary": "The epic" })
        );
        assert_eq!(
            view["links"],
            json!([
                { "type": "Blocks", "direction": "outward", "label": "blocks",
                  "key": "WRK-20", "summary": "Clean up", "status": "Open" },
                { "type": "Blocks", "direction": "inward", "label": "is blocked by",
                  "key": "WRK-15", "summary": "Add a flavor", "status": "To Do" },
            ])
        );
        assert_eq!(
            view["subtasks"],
            json!([{ "key": "WRK-17", "summary": "Port one", "status": "Done" }])
        );
        assert_eq!(view["created"], json!("2025-10-21T13:13:10.850-0500"));
        assert_eq!(view["updated"], json!("2026-08-13T15:51:11.203-0500"));
        assert_eq!(view["status"], json!("In Progress"));
        assert_eq!(view["comments"], json!([]));
    }

    #[test]
    fn an_issue_with_none_of_them_says_so_rather_than_leaving_them_out() {
        let view = issue_view(&json!({ "key": "WRK-1", "fields": { "summary": "Bare" } }));
        assert_eq!(view["labels"], json!([]));
        assert_eq!(view["parent"], Value::Null);
        assert_eq!(view["links"], json!([]));
        assert_eq!(view["subtasks"], json!([]));
    }

    #[test]
    fn a_search_row_carries_labels_and_the_parent_key() {
        let row = search_row(&linked_issue());
        assert_eq!(row["labels"], json!(["etl", "fusion"]));
        assert_eq!(row["parent"], json!("WRK-10"));
        assert_eq!(search_row(&json!({ "fields": {} }))["labels"], json!([]));
    }

    #[test]
    fn the_current_fields_are_in_the_shape_update_takes() {
        let current = current_view(&linked_issue());
        assert_eq!(
            current,
            json!({ "summary": "Replace the scripts", "description": "h2. Goal",
                    "priority": "Minor", "assignee": "Ada Lovelace",
                    "assignee_ref": { "accountId": "5b10a2844c20165700ede21g" },
                    "duedate": "2026-12-01", "labels": ["etl", "fusion"], "parent": "WRK-10" })
        );
        let bare = current_view(&json!({ "fields": { "assignee": null } }));
        assert_eq!(bare["assignee"], Value::Null);
        assert_eq!(bare["assignee_ref"], Value::Null);
        assert_eq!(bare["duedate"], Value::Null);
    }

    #[test]
    fn a_user_is_written_by_account_id_on_cloud_and_by_name_on_data_center() {
        assert_eq!(
            user_ref(&json!({ "accountId": "abc", "name": "ignored" })),
            Some(json!({ "accountId": "abc" }))
        );
        assert_eq!(
            user_ref(&json!({ "name": "ada", "key": "JIRAUSER1" })),
            Some(json!({ "name": "ada" }))
        );
        assert_eq!(user_ref(&Value::Null), None);
    }

    #[test]
    fn a_user_is_picked_exactly_or_as_the_only_one_found() {
        let users = [
            json!({ "accountId": "1", "displayName": "Grace Hopper",
                    "emailAddress": "grace@example.com" }),
            json!({ "accountId": "2", "displayName": "Grace Kelly" }),
        ];
        let pick = |asked| pick_user(&users, asked, "assignable to WRK-1");
        assert_eq!(pick("GRACE@example.com").unwrap()["accountId"], "1");
        assert_eq!(pick("grace kelly").unwrap()["accountId"], "2");
        assert_eq!(pick("2").unwrap()["accountId"], "2");
        let e = pick("grace").unwrap_err();
        assert_eq!(
            e,
            "\"grace\" matches more than one user assignable to WRK-1: Grace Hopper \
             (grace@example.com), Grace Kelly (2); name one by email, username or account id"
        );
        assert_eq!(
            pick_user(&users[1..], "Kel", "x").unwrap()["accountId"],
            "2"
        );
        let e = pick_user(&[], "nobody", "assignable to WRK-1").unwrap_err();
        assert_eq!(e, "no user assignable to WRK-1 matches \"nobody\"");
        // Two people with the name asked are not told apart by guessing.
        let twins = [
            json!({ "accountId": "1", "displayName": "Sam" }),
            json!({ "accountId": "2", "displayName": "Sam" }),
            json!({ "accountId": "3", "displayName": "Samantha" }),
        ];
        let e = pick_user(&twins, "sam", "x").unwrap_err();
        assert!(e.contains("Sam (1), Sam (2);"), "{e}");
    }

    fn blocks() -> LinkType {
        LinkType {
            name: "Blocks".into(),
            outward: "blocks".into(),
            inward: "is blocked by".into(),
        }
    }

    #[test]
    fn a_link_read_back_the_way_it_was_asked_is_returned_in_its_wording() {
        let mut issue = linked_issue();
        let read = links(&issue["fields"]);
        assert_eq!(
            linked(&read, "WRK-16", "WRK-20", &blocks()).unwrap(),
            json!({ "from": "WRK-16", "to": "WRK-20", "type": "Blocks", "label": "blocks" })
        );
        // WRK-15 shows on WRK-16 as blocking it: asked the other way round.
        let e = linked(&read, "WRK-16", "WRK-15", &blocks()).unwrap_err();
        assert!(e.contains("reversed"), "{e}");
        assert!(e.contains("WRK-16 is blocked by WRK-15"), "{e}");
        issue["fields"]["issuelinks"] = json!([]);
        let e = linked(&links(&issue["fields"]), "WRK-16", "WRK-20", &blocks()).unwrap_err();
        assert!(e.contains("does not show it"), "{e}");
    }

    #[test]
    fn a_link_type_that_reads_the_same_both_ways_cannot_be_reversed() {
        let relates = LinkType {
            name: "Relates".into(),
            outward: "relates to".into(),
            inward: "relates to".into(),
        };
        let read = vec![json!({ "type": "Relates", "direction": "inward",
                                "label": "relates to", "key": "WRK-2" })];
        assert_eq!(
            linked(&read, "WRK-1", "WRK-2", &relates).unwrap()["label"],
            "relates to"
        );
    }

    #[test]
    fn a_transition_lists_the_fields_it_requires_with_their_allowed_values() {
        let raw = json!({ "transitions": [
            { "id": "31", "name": "Resolve", "to": { "name": "Done",
                "statusCategory": { "name": "Done" } },
              "fields": {
                "resolution": { "required": true, "name": "Resolution",
                    "allowedValues": [ { "id": "1", "name": "Fixed" }, { "id": "2", "name": "Won't Do" } ] },
                "comment": { "required": false, "name": "Comment" },
              } },
            { "id": "11", "name": "Start", "to": { "name": "In Progress",
                "statusCategory": { "name": "In Progress" } } },
        ] });
        let view = transitions_view("WRK-1", &raw);
        assert_eq!(
            view["transitions"][0],
            json!({ "id": "31", "name": "Resolve", "to": "Done", "category": "Done",
                    "required_fields": [ { "id": "resolution", "name": "Resolution",
                                           "allowed": ["Fixed", "Won't Do"] } ] })
        );
        assert_eq!(view["transitions"][1]["required_fields"], json!([]));
        assert_eq!(target_of(&view, "11").as_deref(), Some("In Progress"));
    }
}
