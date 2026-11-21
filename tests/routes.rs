//! A transition over several hops, against a fake Jira with a strict
//! workflow: the route is found at propose, shown hop by hop, and applied in
//! order, each hop checked again just before it is sent.

mod support;

use jira_review::change::{self, ApplyError, Kind, Proposal, Render};
use serde_json::{Value, json};
use support::Fake;

async fn strict() -> Fake {
    let fake = Fake::start().await;
    fake.strict_workflow();
    fake
}

/// Propose `args` as a transition, checking that only reads were sent.
async fn propose(fake: &Fake, jira: &jira_review::jira::Jira, args: Value) -> Proposal {
    let p = change::propose(Kind::Transition, &args, jira)
        .await
        .unwrap();
    assert!(fake.writes().is_empty(), "propose sent {:?}", fake.writes());
    let p = Proposal::from_json(&p.to_json()).unwrap();
    fake.clear();
    p
}

async fn refused(fake: &Fake, args: Value) -> String {
    let e = change::propose(Kind::Transition, &args, &fake.client())
        .await
        .unwrap_err();
    assert!(fake.writes().is_empty(), "propose sent {:?}", fake.writes());
    e
}

fn show(p: &Proposal) -> String {
    change::render(p, Render::default())
}

/// The transition ids the fake was sent, in order.
fn sent(fake: &Fake) -> Vec<Value> {
    fake.writes()
        .iter()
        .map(|w| {
            assert_eq!(w.path, "/rest/api/2/issue/WRK-1/transitions");
            w.body["transition"]["id"].clone()
        })
        .collect()
}

fn fixed() -> Value {
    json!({ "resolution": { "name": "Fixed" } })
}

#[tokio::test]
async fn a_strict_chain_is_one_change_applied_hop_by_hop() {
    let fake = strict().await;
    let p = propose(
        &fake,
        &fake.client(),
        json!({ "key": "WRK-1", "to_status": "done", "fields": fixed() }),
    )
    .await;
    assert_eq!(p.versions["WRK-1"], "2026-11-16T09:12:33.000+0000");
    assert_eq!(p.proposed["route"], "workflow");
    assert_eq!(p.proposed["workflow"], "Strict");
    assert_eq!(p.proposed["status"], "Done");
    let hops = p.proposed["hops"].as_array().unwrap();
    assert_eq!(hops.len(), 5);
    assert_eq!(hops[4]["fields"], fixed());
    let shown = show(&p);
    for line in [
        "status:\n  - To Do\n  + Done\n",
        "by 5 transitions, in order (the shortest route in workflow Strict",
        "  1. To Do -> Requirements Gathering by 11 (Gather)\n",
        "  2. Requirements Gathering -> In Progress by 21 (Start)\n     its required fields \
         are known only once the issue gets there\n",
        "  5. Rollout -> Done by 51 (Finish)\n",
        "  +    resolution: {\"name\":\"Fixed\"}\n",
    ] {
        assert!(shown.contains(line), "{line:?} not in\n{shown}");
    }
    // The On-hold detour is as short, but takes a global transition.
    assert!(!shown.contains("Hold"), "{shown}");

    let result = change::apply(&p, &fake.client(), None).await.unwrap();
    assert_eq!(sent(&fake), ["11", "21", "31", "41", "51"]);
    assert_eq!(fake.writes()[4].body["fields"], fixed());
    assert_eq!(result["final_status"], "Done");
    assert!(result.get("stopped_at").is_none(), "{result}");
    assert_eq!(
        result["hops"][0],
        json!({ "from": "To Do", "transition": { "id": "11", "name": "Gather" },
                "to": "Requirements Gathering", "ok": true })
    );
    assert_eq!(result["hops"].as_array().unwrap().len(), 5);
    assert_eq!(fake.issue("WRK-1")["fields"]["status"]["name"], "Done");
}

#[tokio::test]
async fn a_status_one_hop_away_needs_no_workflow() {
    let fake = strict().await;
    fake.hide_workflow();
    let p = propose(
        &fake,
        &fake.client(),
        json!({ "key": "WRK-1", "to_status": "Requirements Gathering", "transition": null,
                "path": null }),
    )
    .await;
    assert_eq!(p.proposed["route"], "offered");
    assert_eq!(p.proposed["hops"][0]["required_fields"], json!([]));
    assert!(
        fake.requests()
            .iter()
            .all(|r| !r.path.contains("workflows")),
        "a direct hop does not read the workflow"
    );
    let result = change::apply(&p, &fake.client(), None).await.unwrap();
    assert_eq!(result["final_status"], "Requirements Gathering");
    assert_eq!(sent(&fake), ["11"]);
}

#[tokio::test]
async fn no_route_is_refused_with_what_there_is() {
    let fake = strict().await;
    let e = refused(&fake, json!({ "key": "WRK-1", "to_status": "To Do" })).await;
    assert_eq!(e, "WRK-1 is already in To Do");
    let e = refused(&fake, json!({ "key": "WRK-1", "to_status": "Nowhere" })).await;
    assert!(
        e.contains("Nowhere is not a status in WRK-1's workflow Strict; its statuses are"),
        "{e}"
    );
    // Nothing leads back to Requirements Gathering once past it.
    fake.set_status("WRK-1", "Done");
    let e = refused(
        &fake,
        json!({ "key": "WRK-1", "to_status": "Requirements Gathering" }),
    )
    .await;
    assert!(
        e.contains("has no route from Done to Requirements Gathering")
            && e.contains("61 Cancel (to Canceled), 71 Hold (to On-hold)"),
        "{e}"
    );
}

#[tokio::test]
async fn an_ambiguous_route_is_refused_with_each_route() {
    let fake = strict().await;
    fake.add_transition(
        "22",
        "Skip ahead",
        &["Requirements Gathering"],
        "In Progress",
    );
    let e = refused(&fake, json!({ "key": "WRK-1", "to_status": "UAT" })).await;
    assert!(e.contains("has 2 shortest routes from To Do to UAT"), "{e}");
    assert!(
        e.contains(
            "To Do -[11 Gather]-> Requirements Gathering -[21 Start]-> In Progress -[31 Test]-> UAT"
        ) && e.contains("-[22 Skip ahead]-> In Progress"),
        "{e}"
    );
    // A path picks one.
    let p = propose(
        &fake,
        &fake.client(),
        json!({ "key": "WRK-1", "path": ["Gather", "22", "UAT"] }),
    )
    .await;
    let ids: Vec<&Value> = p.proposed["hops"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| &h["transition"]["id"])
        .collect();
    assert_eq!(ids, ["11", "22", "31"]);
    let e = refused(
        &fake,
        json!({ "key": "WRK-1", "path": ["Gather", "In Progress"] }),
    )
    .await;
    assert!(
        e.contains("path[1]: \"In Progress\" names more than one"),
        "{e}"
    );
}

#[tokio::test]
async fn a_path_is_checked_against_the_workflow_when_it_can_be_read() {
    let fake = strict().await;
    let e = refused(&fake, json!({ "key": "WRK-1", "path": ["Gather", "UAT"] })).await;
    assert!(
        e.starts_with(
            "path[1]: workflow Strict has no transition \"UAT\" from Requirements \
                       Gathering; it has 21 Start (to In Progress)"
        ),
        "{e}"
    );
    let e = refused(&fake, json!({ "key": "WRK-1", "path": ["Start"] })).await;
    assert!(
        e.starts_with("path[0]: WRK-1 offers no transition \"Start\""),
        "{e}"
    );
    let e = refused(
        &fake,
        json!({ "key": "WRK-1", "path": ["Gather"], "transition": "Gather" }),
    )
    .await;
    assert!(
        e.starts_with("give one of transition, to_status or path"),
        "{e}"
    );
}

/// Without the workflow (no permission on Cloud, or Data Center), a status
/// past the next is refused toward path, and a path's later hops are
/// resolved only when the issue reaches them.
async fn without_the_workflow(fake: &Fake, jira: jira_review::jira::Jira) {
    let e = change::propose(
        Kind::Transition,
        &json!({ "key": "WRK-1", "to_status": "Done" }),
        &jira,
    )
    .await
    .unwrap_err();
    assert!(
        e.contains("offers no transition to Done from To Do, and its workflow could not be read")
            && e.contains("Name each hop with path instead"),
        "{e}"
    );
    fake.clear();
    let p = propose(
        fake,
        &jira,
        json!({ "key": "WRK-1",
                "path": ["Gather", "In Progress", "Test", "Rollout",
                         { "transition": "Done", "fields": fixed() }] }),
    )
    .await;
    assert_eq!(p.proposed["route"], "path");
    assert!(p.proposed.get("workflow").is_none());
    assert_eq!(p.proposed["status"], Value::Null);
    assert_eq!(p.proposed["hops"][1]["from"], "Requirements Gathering");
    assert_eq!(p.proposed["hops"][1]["asked"], "In Progress");
    assert_eq!(p.proposed["hops"][1]["transition"], Value::Null);
    let shown = show(&p);
    assert!(
        shown.contains("  + (where \"Done\" leads)\n")
            && shown.contains("the workflow could not be read")
            && shown.contains(
                "  2. from Requirements Gathering by \"In Progress\", as the issue \
                               offers it then\n"
            )
            && shown.contains("  3. then by \"Test\", as the issue offers it then\n"),
        "{shown}"
    );
    let result = change::apply(&p, &jira, None).await.unwrap();
    assert_eq!(sent(fake), ["11", "21", "31", "41", "51"]);
    assert_eq!(result["final_status"], "Done");
    assert_eq!(
        result["hops"][1],
        json!({ "from": "Requirements Gathering", "transition": { "id": "21", "name": "Start" },
                "to": "In Progress", "ok": true })
    );
}

#[tokio::test]
async fn cloud_without_permission_to_view_the_workflow_takes_a_path() {
    let fake = strict().await;
    fake.hide_workflow();
    without_the_workflow(&fake, fake.client()).await;
}

#[tokio::test]
async fn data_center_takes_a_path() {
    let fake = strict().await;
    fake.data_center();
    let jira = fake.data_center_client();
    without_the_workflow(&fake, jira).await;
    assert!(
        fake.requests()
            .iter()
            .all(|r| r.auth.as_deref() == Some("Bearer dc-pat"))
    );
}

#[tokio::test]
async fn a_hop_that_disappears_mid_route_stops_it_there() {
    let fake = strict().await;
    let p = propose(
        &fake,
        &fake.client(),
        json!({ "key": "WRK-1", "to_status": "Done", "fields": fixed() }),
    )
    .await;
    // An admin retires Test while the route is under way.
    fake.retire_on_entering("In Progress", "31");
    let result = change::apply(&p, &fake.client(), None).await.unwrap();
    assert_eq!(sent(&fake), ["11", "21"], "nothing is sent after the stop");
    assert_eq!(result["final_status"], "In Progress");
    assert_eq!(result["stopped_at"], 2);
    let stop = &result["hops"][2];
    assert_eq!(stop["ok"], false);
    assert!(
        stop["error"]
            .as_str()
            .unwrap()
            .starts_with("WRK-1 no longer offers transition 31 (Test) from In Progress; it offers"),
        "{stop}"
    );
    assert_eq!(
        result["not_tried"],
        json!([
            { "from": "UAT", "transition": { "id": "41", "name": "Roll out" }, "to": "Rollout" },
            { "from": "Rollout", "transition": { "id": "51", "name": "Finish" }, "to": "Done" },
        ])
    );
    assert_eq!(
        fake.issue("WRK-1")["fields"]["status"]["name"],
        "In Progress",
        "nothing is rolled back"
    );
}

#[tokio::test]
async fn a_required_field_found_only_on_the_way_stops_the_route_before_it() {
    let fake = strict().await;
    let p = propose(
        &fake,
        &fake.client(),
        json!({ "key": "WRK-1", "to_status": "Done" }),
    )
    .await;
    let result = change::apply(&p, &fake.client(), None).await.unwrap();
    assert_eq!(sent(&fake), ["11", "21", "31", "41"]);
    assert_eq!(result["final_status"], "Rollout");
    assert_eq!(result["stopped_at"], 4);
    assert!(
        result["hops"][4]["error"]
            .as_str()
            .unwrap()
            .starts_with("transition Finish requires resolution (Resolution): one of Fixed"),
        "{result}"
    );
    assert_eq!(result["not_tried"], json!([]));
}

#[tokio::test]
async fn a_route_that_moved_nothing_is_an_error() {
    let fake = strict().await;
    let p = propose(
        &fake,
        &fake.client(),
        json!({ "key": "WRK-1", "to_status": "UAT" }),
    )
    .await;
    fake.refuse_next_write(
        400,
        json!({ "errorMessages": ["Gather is closed"], "errors": {} }),
    );
    let e = change::apply(&p, &fake.client(), None).await.unwrap_err();
    assert_eq!(
        e.to_string(),
        "hop 1 of 3 stopped: jira refused the transition (400 Bad Request): [\"Gather is \
         closed\"]; nothing was moved, and WRK-1 is in To Do"
    );
    assert!(matches!(e, ApplyError::Jira(_)));
}

#[tokio::test]
async fn a_stale_route_sends_nothing() {
    let fake = strict().await;
    let p = propose(
        &fake,
        &fake.client(),
        json!({ "key": "WRK-1", "to_status": "UAT" }),
    )
    .await;
    fake.set_status("WRK-1", "Requirements Gathering");
    fake.clear();
    let e = change::apply(&p, &fake.client(), None).await.unwrap_err();
    assert!(matches!(e, ApplyError::Stale { .. }), "{e:?}");
    assert!(fake.writes().is_empty());
}

#[tokio::test]
async fn a_moved_issue_stops_the_route_at_the_next_hop() {
    let fake = strict().await;
    let p = propose(
        &fake,
        &fake.client(),
        json!({ "key": "WRK-1", "to_status": "UAT" }),
    )
    .await;
    // Someone puts it on hold after the stale check: the expected status is
    // checked again before each hop.
    let shown = p.versions.clone();
    fake.set_status("WRK-1", "On-hold");
    let now = fake.issue("WRK-1")["fields"]["updated"]
        .as_str()
        .unwrap()
        .to_string();
    let expected = jira_review::change::Versions::from([("WRK-1".to_string(), now)]);
    assert_ne!(expected, shown);
    let e = change::apply(&p, &fake.client(), Some(&expected))
        .await
        .unwrap_err();
    assert!(
        e.to_string().starts_with(
            "hop 1 of 3 stopped: WRK-1 is in On-hold, not To Do, so this hop was not sent"
        ),
        "{e}"
    );
    assert!(fake.writes().is_empty());
}
