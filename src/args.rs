//! Each write's arguments, checked before anything is read or sent: the
//! fields a write may set are named rather than passed through, and a
//! mistake is refused with what would have been accepted.

use serde_json::{Map, Value, json};

/// The fields `update` and `create` may set. Status is deliberately absent;
/// so is everything else Jira would accept, because a field this list does
/// not name is one nobody decided a write should touch.
pub const EDITABLE: &[&str] = &[
    "summary",
    "description",
    "priority",
    "assignee",
    "duedate",
    "labels",
    "parent",
];

/// Label edits relative to whatever the issue has when the change applies,
/// which only `update` takes.
pub const RELATIVE: &[&str] = &["add_labels", "remove_labels"];

/// The most issues one `create` makes: a set a reviewer can read at once.
pub const MAX_ISSUES: usize = 10;

/// An issue key, validated before it reaches a URL or a field.
pub fn issue_key<'a>(v: Option<&'a Value>, what: &str) -> Result<&'a str, String> {
    v.and_then(Value::as_str)
        .map(str::trim)
        .filter(|k| !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        .ok_or_else(|| format!("{what} is required (e.g. WRK-123)"))
}

/// A required, non-blank string argument.
pub fn text<'a>(args: &'a Value, field: &str) -> Result<&'a str, String> {
    args.get(field)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| format!("{field} is required"))
}

/// Refuse any argument `allowed` does not name, so that a write never does
/// less than it was asked to without saying so.
pub fn only(args: &Value, verb: &str, allowed: &[&str]) -> Result<(), String> {
    if !args.is_object() {
        return Err(format!("{verb} expects an object of arguments"));
    }
    match args
        .as_object()
        .into_iter()
        .flatten()
        .map(|(k, _)| k)
        .find(|k| !allowed.contains(&k.as_str()))
    {
        Some(k) => Err(format!(
            "{verb} does not take {k}; it takes {}",
            allowed.join(", ")
        )),
        None => Ok(()),
    }
}

/// The `fields` object for a write, from the arguments the tool was given.
///
/// An argument outside the allowlist is refused rather than dropped: a
/// caller that asked to close a ticket should be told the verb does not
/// exist here, not watch the call succeed at doing less than it said.
pub fn fields_from(
    args: &Value,
    verb: &str,
    structural: &[&str],
) -> Result<Map<String, Value>, String> {
    let mut fields = Map::new();
    for (k, v) in args.as_object().into_iter().flatten() {
        if k == "key" || structural.contains(&k.as_str()) {
            continue;
        }
        // Read by `label_edits`, into the request's `update` rather than here.
        if verb == "update" && RELATIVE.contains(&k.as_str()) {
            continue;
        }
        if !EDITABLE.contains(&k.as_str()) {
            let mut writes = EDITABLE.to_vec();
            if verb == "update" {
                writes.extend(RELATIVE);
            }
            return Err(format!(
                "{verb} does not set {k}; it writes only {}. A status change is a \
                 transition: list them with transitions.",
                writes.join(", ")
            ));
        }
        match k.as_str() {
            "summary" | "description" => {
                let s = v.as_str().ok_or_else(|| format!("{k} expects a string"))?;
                if k == "summary" && s.trim().is_empty() {
                    return Err("summary cannot be empty".into());
                }
                fields.insert(k.clone(), json!(s));
            }
            "priority" => {
                let p = v
                    .as_str()
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .ok_or("priority expects a name, e.g. High")?;
                fields.insert(k.clone(), json!({ "name": p }));
            }
            "labels" => {
                fields.insert(k.clone(), json!(label_list(v, k)?));
            }
            "parent" => {
                let p = issue_key(Some(v), "parent")?;
                fields.insert(k.clone(), json!({ "key": p }));
            }
            "duedate" => {
                fields.insert(k.clone(), due_date(v)?);
            }
            // A user is named as a person would name them, and Jira takes an
            // account: `assignee_arg` reads it, and propose resolves it.
            "assignee" => {
                assignee_arg(args)?;
            }
            _ => unreachable!("every editable field is handled"),
        }
    }
    Ok(fields)
}

/// A due date as Jira takes it: `YYYY-MM-DD`, a day that exists, or null to
/// clear it.
pub fn due_date(v: &Value) -> Result<Value, String> {
    if v.is_null() {
        return Ok(Value::Null);
    }
    let wrong = || "duedate expects a date as YYYY-MM-DD, or null to clear it".to_string();
    let s = v.as_str().map(str::trim).ok_or_else(wrong)?;
    let parts: Vec<&str> = s.split('-').collect();
    let [y, m, d] = parts.as_slice() else {
        return Err(wrong());
    };
    if y.len() != 4 || m.len() != 2 || d.len() != 2 {
        return Err(wrong());
    }
    let number = |p: &str| -> Result<u16, String> {
        if p.chars().all(|c| c.is_ascii_digit()) {
            p.parse().map_err(|_| wrong())
        } else {
            Err(wrong())
        }
    };
    let (y, m, d) = (number(y)?, number(m)?, number(d)?);
    let month = u8::try_from(m)
        .ok()
        .and_then(|m| time::Month::try_from(m).ok())
        .ok_or_else(|| format!("duedate {s}: there is no month {m}"))?;
    time::Date::from_calendar_date(i32::from(y), month, d as u8)
        .map_err(|_| format!("duedate {s}: {month} {y} has no day {d}"))?;
    Ok(json!(s))
}

/// The user word that means the credential's own user.
pub const ME: &str = "me";

/// The `assignee` a write names: absent (`None`), null to unassign
/// (`Some(None)`), or a user to find (`Some(Some(..))`): an account id, a
/// username, an email, a display name, or `me`.
pub fn assignee_arg(args: &Value) -> Result<Option<Option<&str>>, String> {
    match args.get("assignee") {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(v) => v
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| Some(Some(s)))
            .ok_or_else(|| {
                "assignee expects a user (account id, username, email or display name), \
                 \"me\", or null to unassign"
                    .into()
            }),
    }
}

/// Jira's `update` verbs for `add_labels` and `remove_labels`, in that order:
/// edits applied to the labels the issue has. Naming either with `labels`,
/// or one label both ways, is refused rather than guessed at.
pub fn label_edits(args: &Value) -> Result<Vec<Value>, String> {
    let list = |field: &str| match args.get(field) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(v) => label_list(v, field),
    };
    let (add, remove) = (list("add_labels")?, list("remove_labels")?);
    if add.is_empty() && remove.is_empty() {
        return Ok(Vec::new());
    }
    if args.get("labels").is_some_and(|l| !l.is_null()) {
        return Err(
            "labels replaces every label, so it is not taken with add_labels or \
                    remove_labels; name one or the other"
                .into(),
        );
    }
    if let Some(both) = add.iter().find(|l| remove.contains(l)) {
        return Err(format!("label {both} is both added and removed"));
    }
    let add = add.into_iter().map(|l| json!({ "add": l }));
    let remove = remove.into_iter().map(|l| json!({ "remove": l }));
    Ok(add.chain(remove).collect())
}

/// `edits` with each removal spelled as the issue has it, ignoring case, and
/// the removals the issue does not have, which are dropped from the edit:
/// Jira removes a label only as it is spelled, and answers 204 for one the
/// issue lacks.
pub fn match_removals(edits: Vec<Value>, present: &[String]) -> (Vec<Value>, Vec<String>) {
    let mut absent = Vec::new();
    let edits = edits
        .into_iter()
        .filter_map(|e| {
            let Some(asked) = e.get("remove").and_then(Value::as_str) else {
                return Some(e);
            };
            match present.iter().find(|l| l.eq_ignore_ascii_case(asked)) {
                Some(l) => Some(json!({ "remove": l })),
                None => {
                    absent.push(asked.to_string());
                    None
                }
            }
        })
        .collect();
    (edits, absent)
}

/// A list of Jira labels, each a single token, as `field` gave them.
pub fn label_list(v: &Value, field: &str) -> Result<Vec<String>, String> {
    v.as_array()
        .ok_or_else(|| format!("{field} expects a list of strings"))?
        .iter()
        .map(|l| {
            let l = l
                .as_str()
                .ok_or_else(|| format!("{field} expects a list of strings"))?;
            if l.trim().is_empty() {
                return Err("a label cannot be empty".to_string());
            }
            if l.chars().any(char::is_whitespace) {
                return Err(format!(
                    "label {l:?} has whitespace; a Jira label is a single token"
                ));
            }
            Ok(l.to_string())
        })
        .collect()
}

/// One issue a `create` makes, checked before anything is sent: its fields
/// as Jira takes them, and the earlier item of the same call that is its
/// parent, when it names one as `#N`.
#[derive(Debug, Clone)]
pub struct NewIssue {
    pub fields: Map<String, Value>,
    pub parent_item: Option<usize>,
    /// The user it is to be assigned, as named (see [`assignee_arg`]); the
    /// account it resolved to is the proposal's, put in `fields` at apply.
    pub assignee: Option<Option<String>>,
}

/// What a `create` would make, in order, every item checked up front so that
/// a mistake in the fifth is refused before the first exists. `issues` is the
/// alternative to the single-issue fields, never an addition to them.
pub fn create_plan(args: &Value) -> Result<Vec<NewIssue>, String> {
    let Some(issues) = args.get("issues") else {
        return Ok(vec![new_issue(args, None)?]);
    };
    if let Some(other) = args
        .as_object()
        .into_iter()
        .flatten()
        .map(|(k, _)| k)
        .find(|k| *k != "issues")
    {
        return Err(format!(
            "pass either issues or the single-issue fields, not both (got issues and {other})"
        ));
    }
    let items = issues
        .as_array()
        .filter(|items| !items.is_empty())
        .ok_or("issues expects a list of at least one issue")?;
    if items.len() > MAX_ISSUES {
        return Err(format!(
            "issues has {} items; one call creates at most {MAX_ISSUES}",
            items.len()
        ));
    }
    items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            if !item.is_object() {
                return Err(format!("issues[{i}]: expects an object"));
            }
            new_issue(item, Some(i)).map_err(|e| format!("issues[{i}]: {e}"))
        })
        .collect()
}

/// One issue's fields, from the single-issue arguments or from the item at
/// `index` of `issues`, where alone a parent may be `#N`.
fn new_issue(args: &Value, index: Option<usize>) -> Result<NewIssue, String> {
    let project = args
        .get("project")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        .ok_or("project is required (a project key, e.g. WRK)")?;
    let kind = args
        .get("type")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or("type is required (an issue type name, e.g. Task)")?;
    let reference = args
        .get("parent")
        .and_then(Value::as_str)
        .map(str::trim)
        .and_then(|p| p.strip_prefix('#'));
    let parent_item = match (reference, index) {
        (None, _) => None,
        (Some(_), None) => {
            return Err("parent #N names an item of issues; this call has none".into());
        }
        (Some(n), Some(own)) => {
            let n: usize = n
                .parse()
                .map_err(|_| format!("parent #{n} is not an item index"))?;
            if n == own {
                return Err(format!(
                    "parent #{n} is this item; an issue is not its own parent"
                ));
            }
            if n > own {
                return Err(format!(
                    "parent #{n} comes later; a parent must be an earlier item, created first"
                ));
            }
            Some(n)
        }
    };
    let mut structural = vec!["project", "type"];
    if parent_item.is_some() {
        structural.push("parent");
    }
    let mut fields = fields_from(args, "create", &structural)?;
    if !fields.contains_key("summary") {
        return Err("summary is required".into());
    }
    fields.insert("project".into(), json!({ "key": project }));
    fields.insert("issuetype".into(), json!({ "name": kind }));
    let assignee = assignee_arg(args)?.map(|a| a.map(str::to_string));
    Ok(NewIssue {
        fields,
        parent_item,
        assignee,
    })
}

/// `from`, `to` and `type` of a `link`, checked before anything is read.
pub fn link_args(args: &Value) -> Result<(&str, &str, &str), String> {
    let from = issue_key(args.get("from"), "from")?;
    let to = issue_key(args.get("to"), "to")?;
    if from.eq_ignore_ascii_case(to) {
        return Err(format!("{from} cannot be linked to itself"));
    }
    let kind = args
        .get("type")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or("type is required (a link type name, e.g. Blocks)")?;
    Ok((from, to, kind))
}

/// One of the instance's issue link types: its name, and how each side
/// reads it ("blocks", "is blocked by").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkType {
    pub name: String,
    pub outward: String,
    pub inward: String,
}

/// The link type named `asked`, ignoring case, or a refusal listing the
/// names this Jira has, so the caller can choose without another read.
pub fn link_type(types: &[LinkType], asked: &str) -> Result<LinkType, String> {
    types
        .iter()
        .find(|t| t.name.eq_ignore_ascii_case(asked))
        .cloned()
        .ok_or_else(|| {
            let names: Vec<&str> = types.iter().map(|t| t.name.as_str()).collect();
            if names.is_empty() {
                return format!("this Jira has no link type {asked:?}; it lists none");
            }
            format!(
                "this Jira has no link type {asked:?}; it has {}",
                names.join(", ")
            )
        })
}

/// A transition as a caller names it: an id, or the name of the transition
/// or of the status it leads to.
pub fn transition_arg(args: &Value) -> Result<&str, String> {
    args.get("transition")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| "transition is required (an id or a name, as transitions lists them)".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocks() -> LinkType {
        LinkType {
            name: "Blocks".into(),
            outward: "blocks".into(),
            inward: "is blocked by".into(),
        }
    }

    #[test]
    fn a_link_type_is_found_by_name_or_refused_with_the_names_there_are() {
        let relates = LinkType {
            name: "Relates".into(),
            outward: "relates to".into(),
            inward: "relates to".into(),
        };
        let types = [blocks(), relates];
        assert_eq!(link_type(&types, "blocks").unwrap(), blocks());
        let e = link_type(&types, "Precedes").unwrap_err();
        assert!(e.contains("Blocks, Relates"), "{e}");
        let e = link_type(&[], "Blocks").unwrap_err();
        assert!(e.contains("lists none"), "{e}");
    }

    #[test]
    fn a_due_date_is_a_day_that_exists_or_null() {
        assert_eq!(due_date(&json!("2026-12-01")).unwrap(), json!("2026-12-01"));
        assert_eq!(
            due_date(&json!(" 2028-02-29 ")).unwrap(),
            json!("2028-02-29")
        );
        assert_eq!(due_date(&Value::Null).unwrap(), Value::Null);
        for wrong in [
            json!("12/01/2026"),
            json!("2026-1-01"),
            json!("next week"),
            json!(3),
        ] {
            let e = due_date(&wrong).unwrap_err();
            assert!(e.contains("YYYY-MM-DD"), "{wrong}: {e}");
        }
        let e = due_date(&json!("2026-02-29")).unwrap_err();
        assert_eq!(e, "duedate 2026-02-29: February 2026 has no day 29");
        let e = due_date(&json!("2026-13-01")).unwrap_err();
        assert_eq!(e, "duedate 2026-13-01: there is no month 13");
        let f = fields_from(&json!({ "duedate": null }), "update", &[]).unwrap();
        assert_eq!(f["duedate"], Value::Null);
    }

    #[test]
    fn an_assignee_is_a_user_to_find_or_null_and_not_a_field_as_given() {
        assert_eq!(assignee_arg(&json!({})).unwrap(), None);
        assert_eq!(
            assignee_arg(&json!({ "assignee": null })).unwrap(),
            Some(None)
        );
        assert_eq!(
            assignee_arg(&json!({ "assignee": " ada@example.com " })).unwrap(),
            Some(Some("ada@example.com"))
        );
        for wrong in [json!(""), json!(" "), json!({ "accountId": "x" })] {
            let e = assignee_arg(&json!({ "assignee": wrong })).unwrap_err();
            assert!(e.contains("null to unassign"), "{e}");
        }
        // Found by propose, so never sent as the caller spelled it.
        let f = fields_from(&json!({ "assignee": "ada", "summary": "s" }), "update", &[]).unwrap();
        assert!(!f.contains_key("assignee"));
        let plan = create_plan(&json!({ "issues": [
            { "project": "WRK", "type": "Task", "summary": "a", "assignee": "me" },
            { "project": "WRK", "type": "Task", "summary": "b", "assignee": null },
            { "project": "WRK", "type": "Task", "summary": "c" },
        ] }))
        .unwrap();
        assert_eq!(plan[0].assignee, Some(Some("me".to_string())));
        assert_eq!(plan[1].assignee, Some(None));
        assert_eq!(plan[2].assignee, None);
        assert!(plan.iter().all(|i| !i.fields.contains_key("assignee")));
    }

    #[test]
    fn a_link_names_two_different_issues_and_a_type() {
        let e =
            link_args(&json!({ "from": "WRK-1", "to": "wrk-1", "type": "Blocks" })).unwrap_err();
        assert!(e.contains("itself"), "{e}");
        let e = link_args(&json!({ "from": "WRK-1", "to": "not a key", "type": "Blocks" }))
            .unwrap_err();
        assert!(e.starts_with("to"), "{e}");
        let e = link_args(&json!({ "from": "WRK-1", "to": "WRK-2" })).unwrap_err();
        assert!(e.contains("type"), "{e}");
    }

    #[test]
    fn a_field_the_tools_do_not_write_is_refused_by_name() {
        let e =
            fields_from(&json!({ "key": "WRK-1", "status": "Done" }), "update", &[]).unwrap_err();
        assert!(e.contains("status"), "{e}");
        assert!(e.contains("transition"), "{e}");
    }

    #[test]
    fn a_label_is_a_single_token_and_a_parent_is_a_key() {
        let e = fields_from(&json!({ "labels": ["needs review"] }), "update", &[]).unwrap_err();
        assert!(e.contains("whitespace"), "{e}");
        let e = fields_from(&json!({ "parent": "not a key" }), "update", &[]).unwrap_err();
        assert!(e.contains("parent"), "{e}");
    }

    #[test]
    fn the_written_fields_carry_jiras_own_shapes() {
        let f = fields_from(
            &json!({ "key": "WRK-1", "priority": "High", "parent": "WRK-9", "labels": ["a", "b"] }),
            "update",
            &[],
        )
        .unwrap();
        assert_eq!(f["priority"], json!({ "name": "High" }));
        assert_eq!(f["parent"], json!({ "key": "WRK-9" }));
        assert_eq!(f["labels"], json!(["a", "b"]));
        assert!(!f.contains_key("key"));
    }

    #[test]
    fn summary_description_and_labels_are_sent_as_given() {
        let f = fields_from(
            &json!({ "summary": "  Fix it ", "description": "\n body \n", "labels": ["a"] }),
            "update",
            &[],
        )
        .unwrap();
        assert_eq!(f["summary"], json!("  Fix it "));
        assert_eq!(f["description"], json!("\n body \n"));
        let e = fields_from(&json!({ "summary": "  " }), "update", &[]).unwrap_err();
        assert!(e.contains("summary cannot be empty"), "{e}");
        let e = fields_from(&json!({ "labels": [" "] }), "update", &[]).unwrap_err();
        assert!(e.contains("cannot be empty"), "{e}");
        // A padded label is refused, not trimmed into a different one.
        let e = fields_from(&json!({ "labels": [" a"] }), "update", &[]).unwrap_err();
        assert!(e.contains("whitespace"), "{e}");
    }

    #[test]
    fn labels_can_be_added_and_removed_relative_to_what_the_issue_has() {
        let args =
            json!({ "key": "WRK-1", "add_labels": ["nead", "etl"], "remove_labels": ["stale"] });
        assert_eq!(
            label_edits(&args).unwrap(),
            vec![
                json!({ "add": "nead" }),
                json!({ "add": "etl" }),
                json!({ "remove": "stale" })
            ]
        );
        // They are an `update`, not a field: the replacement list is not sent.
        assert!(fields_from(&args, "update", &[]).unwrap().is_empty());
        assert_eq!(
            label_edits(&json!({ "remove_labels": ["stale"] })).unwrap(),
            vec![json!({ "remove": "stale" })]
        );
        assert!(label_edits(&json!({ "summary": "x" })).unwrap().is_empty());
    }

    #[test]
    fn a_removal_is_spelled_as_the_issue_has_it_and_one_it_lacks_is_dropped() {
        let edits = vec![
            json!({ "add": "nead" }),
            json!({ "remove": "STALE" }),
            json!({ "remove": "gone" }),
        ];
        let (edits, absent) = match_removals(edits, &["etl".into(), "stale".into()]);
        assert_eq!(
            edits,
            vec![json!({ "add": "nead" }), json!({ "remove": "stale" })]
        );
        assert_eq!(absent, vec!["gone".to_string()]);
    }

    #[test]
    fn a_relative_label_edit_is_refused_alongside_a_replacement_or_against_itself() {
        let e = label_edits(&json!({ "labels": ["a"], "add_labels": ["b"] })).unwrap_err();
        assert!(e.contains("not taken with"), "{e}");
        let e = label_edits(&json!({ "add_labels": ["a"], "remove_labels": ["a"] })).unwrap_err();
        assert!(e.contains("both added and removed"), "{e}");
        let e = label_edits(&json!({ "add_labels": ["needs review"] })).unwrap_err();
        assert!(e.contains("whitespace"), "{e}");
        let e = label_edits(&json!({ "remove_labels": "stale" })).unwrap_err();
        assert!(e.contains("remove_labels expects a list"), "{e}");
    }

    #[test]
    fn a_new_issue_takes_no_relative_label_edit() {
        let e = fields_from(
            &json!({ "summary": "x", "add_labels": ["a"] }),
            "create",
            &[],
        )
        .unwrap_err();
        assert!(e.contains("does not set add_labels"), "{e}");
    }

    pub(crate) fn epic_and_children() -> Value {
        json!({ "issues": [
            { "project": "WRK", "type": "Epic", "summary": "Port the feed" },
            { "project": "WRK", "type": "Story", "summary": "Read it", "parent": "#0" },
            { "project": "WRK", "type": "Story", "summary": "Write it", "parent": "WRK-3",
              "labels": ["etl"] },
        ] })
    }

    #[test]
    fn a_child_may_name_an_earlier_item_as_its_parent() {
        let plan = create_plan(&epic_and_children()).unwrap();
        assert_eq!(plan.len(), 3);
        assert_eq!(plan[1].parent_item, Some(0));
        assert!(!plan[1].fields.contains_key("parent"));
        assert_eq!(plan[2].parent_item, None);
        assert_eq!(plan[2].fields["parent"], json!({ "key": "WRK-3" }));
        assert_eq!(plan[0].fields["issuetype"], json!({ "name": "Epic" }));
    }

    #[test]
    fn a_parent_reference_must_name_an_earlier_item() {
        let with_parent = |own: usize, parent: &str| {
            let mut args = epic_and_children();
            args["issues"][own]["parent"] = json!(parent);
            create_plan(&args).unwrap_err()
        };
        let e = with_parent(1, "#2");
        assert!(
            e.starts_with("issues[1]:") && e.contains("comes later"),
            "{e}"
        );
        let e = with_parent(1, "#1");
        assert!(e.contains("its own parent"), "{e}");
        let e = with_parent(0, "#x");
        assert!(e.contains("not an item index"), "{e}");
        // Outside issues there is no item to name.
        let e = create_plan(
            &json!({ "project": "WRK", "type": "Task", "summary": "s", "parent": "#0" }),
        )
        .unwrap_err();
        assert!(e.contains("this call has none"), "{e}");
    }

    #[test]
    fn every_item_is_checked_before_anything_is_created() {
        let mut args = epic_and_children();
        args["issues"][2]["status"] = json!("Done");
        let e = create_plan(&args).unwrap_err();
        assert!(e.starts_with("issues[2]:") && e.contains("status"), "{e}");
        let mut args = epic_and_children();
        args["issues"][1].as_object_mut().unwrap().remove("summary");
        assert!(
            create_plan(&args)
                .unwrap_err()
                .contains("summary is required")
        );
        let e = create_plan(&json!({ "issues": [] })).unwrap_err();
        assert!(e.contains("at least one"), "{e}");
        let many = vec![json!({ "project": "WRK", "type": "Task", "summary": "s" }); 11];
        let e = create_plan(&json!({ "issues": many })).unwrap_err();
        assert!(e.contains("at most 10"), "{e}");
    }

    #[test]
    fn issues_replaces_the_single_issue_fields_rather_than_joining_them() {
        let mut args = epic_and_children();
        args["summary"] = json!("Which one?");
        let e = create_plan(&args).unwrap_err();
        assert!(e.contains("not both") && e.contains("summary"), "{e}");
    }

    #[test]
    fn a_single_issue_is_planned_as_before() {
        let plan = create_plan(
            &json!({ "project": "WRK", "type": "Task", "summary": "s", "parent": "WRK-1" }),
        )
        .unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].fields["parent"], json!({ "key": "WRK-1" }));
        assert_eq!(plan[0].fields["project"], json!({ "key": "WRK" }));
        let e = create_plan(&json!({ "type": "Task", "summary": "s" })).unwrap_err();
        assert!(e.starts_with("project is required"), "{e}");
    }

    #[test]
    fn arguments_outside_a_verbs_list_are_refused() {
        let e = only(
            &json!({ "key": "WRK-1", "bdy": "x" }),
            "comment",
            &["key", "body"],
        )
        .unwrap_err();
        assert!(e.contains("does not take bdy"), "{e}");
        assert!(only(&json!({ "key": "WRK-1" }), "comment", &["key", "body"]).is_ok());
    }
}
