//! What the reads answer, shaped from Jira's issue JSON as it came back.

use serde_json::{Value, json};

use crate::args::LinkType;

/// What a search row carries: enough to pick an issue, not to read it.
pub const SEARCH_FIELDS: &str = "summary,status,assignee,priority,issuetype,parent,labels";

/// What `issue` reads: enough to write to the issue safely, since a label
/// list, a parent or a link is what a write would otherwise have to guess.
pub const ISSUE_FIELDS: &str = "summary,status,assignee,description,comment,issuetype,priority,\
                                labels,parent,issuelinks,subtasks,created,updated";

/// What an `update` is shown against.
pub const CURRENT_FIELDS: &str = "summary,description,priority,labels,parent,updated";

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
/// `update` takes it, so a change compares like with like.
pub fn current_view(v: &Value) -> Value {
    let f = &v["fields"];
    json!({
        "summary": f["summary"],
        "description": f["description"],
        "priority": f["priority"]["name"],
        "labels": labels(f),
        "parent": f["parent"]["key"],
    })
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
                "assignee": null,
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
                    "priority": "Minor", "labels": ["etl", "fusion"], "parent": "WRK-10" })
        );
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
