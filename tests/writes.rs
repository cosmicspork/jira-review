mod support;

use jira_review::change::{self, ApplyError, Kind, Proposal, Render};
use serde_json::{Value, json};
use support::Fake;

/// Propose `args` as `kind`, checking that only reads were sent.
async fn propose(fake: &Fake, kind: Kind, args: Value) -> Proposal {
    let p = change::propose(kind, &args, &fake.client()).await.unwrap();
    assert!(fake.writes().is_empty(), "propose sent {:?}", fake.writes());
    // What a reviewer reads in git is what gets applied.
    let p = Proposal::from_json(&p.to_json()).unwrap();
    fake.clear();
    p
}

fn show(p: &Proposal) -> String {
    change::render(p, Render::default())
}

#[tokio::test]
async fn an_update_is_proposed_shown_and_applied() {
    let fake = Fake::start().await;
    let p = propose(
        &fake,
        Kind::Update,
        json!({ "key": "WRK-1", "summary": "Port the feed to S3", "priority": "High",
                "add_labels": ["s3"], "remove_labels": ["stale", "gone"] }),
    )
    .await;
    assert_eq!(p.versions["WRK-1"], "2026-11-16T09:12:33.000+0000");
    assert_eq!(p.current["labels"], json!(["etl", "Stale"]));
    assert_eq!(p.proposed["labels"], json!(["etl", "s3"]));
    assert_eq!(p.proposed["not_on_issue"], json!(["gone"]));
    let shown = show(&p);
    for line in [
        "summary:\n  - Port the feed\n  + Port the feed to S3\n",
        "priority:\n  - Medium\n  + High\n",
        "labels:\n  - Stale\n  + s3\n",
        "not on the issue, so not removed: gone\n",
    ] {
        assert!(shown.contains(line), "{shown}");
    }

    let result = change::apply(&p, &fake.client(), None).await.unwrap();
    assert_eq!(
        result,
        json!({ "key": "WRK-1", "updated": ["priority", "summary", "labels"],
                "not_on_issue": ["gone"] })
    );
    let writes = fake.writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].method, "PUT");
    assert_eq!(writes[0].path, "/rest/api/2/issue/WRK-1");
    assert_eq!(
        writes[0].body,
        json!({ "fields": { "summary": "Port the feed to S3", "priority": { "name": "High" } },
                "update": { "labels": [ { "add": "s3" }, { "remove": "Stale" } ] } })
    );
    assert_eq!(
        fake.issue("WRK-1")["fields"]["labels"],
        json!(["etl", "s3"])
    );
}

#[tokio::test]
async fn an_update_that_changes_nothing_is_refused_at_propose() {
    let fake = Fake::start().await;
    let e = change::propose(
        Kind::Update,
        &json!({ "key": "WRK-1", "remove_labels": ["gone"] }),
        &fake.client(),
    )
    .await
    .unwrap_err();
    assert!(
        e.contains("nothing would change") && e.contains("gone"),
        "{e}"
    );
    let e = change::propose(
        Kind::Update,
        &json!({ "key": "WRK-1", "status": "Done" }),
        &fake.client(),
    )
    .await
    .unwrap_err();
    assert!(e.contains("does not set status"), "{e}");
    assert!(fake.requests().iter().all(|r| r.method == "GET"));
}

#[tokio::test]
async fn a_stale_change_is_refused_and_nothing_is_written() {
    let fake = Fake::start().await;
    let p = propose(
        &fake,
        Kind::Update,
        json!({ "key": "WRK-1", "summary": "New" }),
    )
    .await;
    fake.bump("WRK-1");
    let e = change::apply(&p, &fake.client(), None).await.unwrap_err();
    match &e {
        ApplyError::Stale { key, was, now } => {
            assert_eq!(key, "WRK-1");
            assert_eq!(was, "2026-11-16T09:12:33.000+0000");
            assert_ne!(now, was);
        }
        other => panic!("not stale: {other:?}"),
    }
    assert!(e.to_string().contains("propose again"), "{e}");
    assert!(fake.writes().is_empty(), "{:?}", fake.writes());
    assert_eq!(fake.issue("WRK-1")["fields"]["summary"], "Port the feed");
}

#[tokio::test]
async fn an_expected_version_token_is_checked_in_place_of_the_proposals() {
    let fake = Fake::start().await;
    let p = propose(
        &fake,
        Kind::Comment,
        json!({ "key": "WRK-2", "body": "hi" }),
    )
    .await;
    let token = change::Versions::from([("WRK-2".to_string(), "older".to_string())]);
    let e = change::apply(&p, &fake.client(), Some(&token))
        .await
        .unwrap_err();
    assert!(matches!(e, ApplyError::Stale { .. }), "{e:?}");
    assert!(fake.writes().is_empty());
}

#[tokio::test]
async fn a_change_for_another_site_is_refused() {
    let fake = Fake::start().await;
    let mut p = propose(
        &fake,
        Kind::Comment,
        json!({ "key": "WRK-2", "body": "hi" }),
    )
    .await;
    p.jira = "https://elsewhere.atlassian.net".into();
    let e = change::apply(&p, &fake.client(), None).await.unwrap_err();
    assert!(matches!(e, ApplyError::Invalid(_)), "{e:?}");
    assert!(fake.requests().is_empty());
}

#[tokio::test]
async fn a_transition_is_named_by_status_and_requires_its_fields() {
    let fake = Fake::start().await;
    let e = change::propose(
        Kind::Transition,
        &json!({ "key": "WRK-1", "transition": "done" }),
        &fake.client(),
    )
    .await
    .unwrap_err();
    assert!(
        e.contains("requires resolution (Resolution): one of Fixed, Won't Do"),
        "{e}"
    );
    let e = change::propose(
        Kind::Transition,
        &json!({ "key": "WRK-1", "transition": "Close" }),
        &fake.client(),
    )
    .await
    .unwrap_err();
    assert!(
        e.contains("offers no transition") && e.contains("11 Start (to In Progress)"),
        "{e}"
    );
    let e = change::propose(
        Kind::Transition,
        &json!({ "key": "WRK-1", "transition": "11", "fields": { "resolution": {} } }),
        &fake.client(),
    )
    .await
    .unwrap_err();
    assert!(
        e.contains("does not take field resolution; it takes none"),
        "{e}"
    );

    fake.clear();
    let p = propose(
        &fake,
        Kind::Transition,
        json!({ "key": "WRK-1", "transition": "Done",
                "fields": { "resolution": { "name": "Fixed" } } }),
    )
    .await;
    assert!(
        show(&p).contains("status:\n  - To Do\n  + Done\nby transition 31 (Resolve)\n"),
        "{}",
        show(&p)
    );
    let result = change::apply(&p, &fake.client(), None).await.unwrap();
    assert_eq!(
        result,
        json!({ "key": "WRK-1", "transition": "31", "status": "Done" })
    );
    let writes = fake.writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].path, "/rest/api/2/issue/WRK-1/transitions");
    assert_eq!(
        writes[0].body,
        json!({ "transition": { "id": "31" }, "fields": { "resolution": { "name": "Fixed" } } })
    );
    assert_eq!(fake.issue("WRK-1")["fields"]["status"]["name"], "Done");
}

#[tokio::test]
async fn a_link_depends_on_both_issues_and_reads_back_its_wording() {
    let fake = Fake::start().await;
    let p = propose(
        &fake,
        Kind::Link,
        json!({ "from": "WRK-1", "to": "WRK-2", "type": "blocks" }),
    )
    .await;
    assert_eq!(p.versions.len(), 2);
    assert!(
        show(&p).contains("links of WRK-1:\n  + blocks WRK-2 (Clean up, To Do)\n"),
        "{}",
        show(&p)
    );
    let result = change::apply(&p, &fake.client(), None).await.unwrap();
    assert_eq!(
        result,
        json!({ "from": "WRK-1", "to": "WRK-2", "type": "Blocks", "label": "blocks" })
    );
    let writes = fake.writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(
        writes[0].body,
        json!({ "type": { "name": "Blocks" }, "inwardIssue": { "key": "WRK-1" },
                "outwardIssue": { "key": "WRK-2" } })
    );
    fake.clear();
    // Linking again is refused at propose: it is already there.
    let e = change::propose(
        Kind::Link,
        &json!({ "from": "WRK-1", "to": "WRK-2", "type": "Blocks" }),
        &fake.client(),
    )
    .await
    .unwrap_err();
    assert!(e.contains("already blocks"), "{e}");
    // Moving the other side makes the change stale too.
    let p = propose(
        &fake,
        Kind::Link,
        json!({ "from": "WRK-1", "to": "WRK-10", "type": "Relates" }),
    )
    .await;
    fake.bump("WRK-10");
    let e = change::apply(&p, &fake.client(), None).await.unwrap_err();
    assert!(
        matches!(&e, ApplyError::Stale { key, .. } if key == "WRK-10"),
        "{e:?}"
    );
}

#[tokio::test]
async fn a_single_issue_is_created_against_nothing() {
    let fake = Fake::start().await;
    let p = propose(
        &fake,
        Kind::Create,
        json!({ "project": "WRK", "type": "Task", "summary": "New thing", "parent": "WRK-10",
                "labels": ["etl"] }),
    )
    .await;
    assert!(p.versions.is_empty());
    assert!(p.current.is_null());
    let shown = show(&p);
    assert!(shown.contains("depends on no existing issue"), "{shown}");
    assert!(
        shown.contains("  + [0] WRK Task: New thing\n      labels: etl\n      parent: WRK-10\n"),
        "{shown}"
    );
    let result = change::apply(&p, &fake.client(), None).await.unwrap();
    assert_eq!(result["key"], "WRK-11");
    assert_eq!(result["url"], format!("{}/browse/WRK-11", fake.url));
    let writes = fake.writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(
        writes[0].body,
        json!({ "fields": { "summary": "New thing", "labels": ["etl"],
                            "parent": { "key": "WRK-10" }, "project": { "key": "WRK" },
                            "issuetype": { "name": "Task" } } })
    );
}

#[tokio::test]
async fn a_parent_named_by_key_must_exist_at_propose() {
    let fake = Fake::start().await;
    let e = change::propose(
        Kind::Create,
        &json!({ "project": "WRK", "type": "Task", "summary": "x", "parent": "WRK-404" }),
        &fake.client(),
    )
    .await
    .unwrap_err();
    assert!(
        e.contains("parent WRK-404") && e.contains("does not exist"),
        "{e}"
    );
}

fn epic_and_children() -> Value {
    json!({ "issues": [
        { "project": "WRK", "type": "Epic", "summary": "Port the feed" },
        { "project": "WRK", "type": "Story", "summary": "Read it", "parent": "#0" },
        { "project": "WRK", "type": "Story", "summary": "Write it", "parent": "#0" },
    ] })
}

#[tokio::test]
async fn a_set_is_created_in_order_under_its_new_parent() {
    let fake = Fake::start().await;
    let p = propose(&fake, Kind::Create, epic_and_children()).await;
    assert!(
        show(&p).contains("  + [1] WRK Story: Read it\n      parent: #0\n"),
        "{}",
        show(&p)
    );
    let result = change::apply(&p, &fake.client(), None).await.unwrap();
    assert_eq!(result["failed"], Value::Null);
    let keys: Vec<&str> = result["created"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, ["WRK-11", "WRK-12", "WRK-13"]);
    let writes = fake.writes();
    assert_eq!(
        writes[1].body["fields"]["parent"],
        json!({ "key": "WRK-11" })
    );
    assert_eq!(
        writes[2].body["fields"]["parent"],
        json!({ "key": "WRK-11" })
    );
}

#[tokio::test]
async fn a_refusal_mid_set_says_what_was_created_failed_and_skipped() {
    let fake = Fake::start().await;
    let p = propose(&fake, Kind::Create, epic_and_children()).await;
    fake.refuse_next_write(201, json!({ "id": "9", "key": "WRK-11" }));
    fake.refuse_next_write(
        400,
        json!({ "errorMessages": [], "errors": { "parent": "not an epic" } }),
    );
    let result = change::apply(&p, &fake.client(), None).await.unwrap();
    assert_eq!(result["created"][0]["key"], "WRK-11");
    assert_eq!(result["failed"]["index"], 1);
    assert!(
        result["failed"]["error"]
            .as_str()
            .unwrap()
            .contains("not an epic"),
        "{result}"
    );
    assert_eq!(result["skipped"], json!([2]));
    assert_eq!(fake.writes().len(), 2, "nothing is sent after the refusal");
}

#[tokio::test]
async fn a_comment_shows_the_last_one_and_posts_the_body() {
    let fake = Fake::start().await;
    let p = propose(
        &fake,
        Kind::Comment,
        json!({ "key": "WRK-1", "body": "Done with *this*.\nNext: {{deploy}}" }),
    )
    .await;
    let shown = show(&p);
    assert!(
        shown.contains(
            "last comment by Ada at 2026-11-15T08:00:00.000+0000\nnew comment:\n  \
             + Done with *this*.\n  + Next: {{deploy}}\n"
        ),
        "{shown}"
    );
    let md = change::render(
        &p,
        Render {
            color: false,
            markdown: true,
        },
    );
    assert!(
        md.contains("  + Done with **this**.\\\n  + Next: `deploy`\n"),
        "{md}"
    );
    let result = change::apply(&p, &fake.client(), None).await.unwrap();
    assert_eq!(result["id"], "200");
    let writes = fake.writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].path, "/rest/api/2/issue/WRK-1/comment");
    assert_eq!(
        writes[0].body,
        json!({ "body": "Done with *this*.\nNext: {{deploy}}" })
    );
}

#[tokio::test]
async fn a_jira_refusal_at_apply_is_reported_with_its_field_messages() {
    let fake = Fake::start().await;
    let p = propose(
        &fake,
        Kind::Update,
        json!({ "key": "WRK-2", "priority": "Bogus" }),
    )
    .await;
    fake.refuse_next_write(
        400,
        json!({ "errorMessages": [], "errors": { "priority": "unknown priority" } }),
    );
    let e = change::apply(&p, &fake.client(), None).await.unwrap_err();
    assert!(
        matches!(&e, ApplyError::Jira(m) if m.contains("unknown priority")),
        "{e:?}"
    );
}
