mod support;

use serde_json::json;
use support::Fake;

#[tokio::test]
async fn search_uses_the_cloud_endpoint_and_returns_rows() {
    let fake = Fake::start().await;
    let v = fake.client().search("project = WRK", 2).await.unwrap();
    assert_eq!(v["issues"].as_array().unwrap().len(), 2);
    assert_eq!(
        v["issues"][0],
        json!({ "key": "WRK-1", "type": "Task", "summary": "Port the feed", "status": "To Do",
                "priority": "Medium", "assignee": null, "parent": null,
                "labels": ["etl", "Stale"] })
    );
    let r = fake.requests();
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].path, "/rest/api/3/search/jql");
    assert!(r[0].query.contains("maxResults=2"), "{}", r[0].query);
    assert!(r[0].query.contains("jql=project"), "{}", r[0].query);
}

#[tokio::test]
async fn search_falls_back_to_v2_on_data_center() {
    let fake = Fake::start().await;
    fake.data_center();
    let v = fake.client().search("project = WRK", 20).await.unwrap();
    assert_eq!(v["issues"].as_array().unwrap().len(), 3);
    let paths: Vec<String> = fake.requests().into_iter().map(|r| r.path).collect();
    assert_eq!(paths, ["/rest/api/3/search/jql", "/rest/api/2/search"]);
}

#[tokio::test]
async fn an_empty_query_is_refused_before_anything_is_sent() {
    let fake = Fake::start().await;
    assert!(fake.client().search("  ", 20).await.is_err());
    assert!(fake.requests().is_empty());
}

#[tokio::test]
async fn an_issue_reads_with_its_comments_and_stamp() {
    let fake = Fake::start().await;
    let v = fake.client().issue("WRK-1").await.unwrap();
    assert_eq!(v["description"], "h2. Goal\nMove the feed.");
    assert_eq!(v["updated"], "2026-11-16T09:12:33.000+0000");
    assert_eq!(v["comments"][0]["author"], "Ada");
    let e = fake.client().issue("WRK-404").await.unwrap_err();
    assert!(e.contains("404") && e.contains("does not exist"), "{e}");
}

#[tokio::test]
async fn transitions_list_targets_and_required_fields() {
    let fake = Fake::start().await;
    let v = fake.client().transitions("WRK-1").await.unwrap();
    assert_eq!(
        v,
        json!({ "key": "WRK-1", "transitions": [
            { "id": "11", "name": "Start", "to": "In Progress", "category": "In Progress",
              "required_fields": [] },
            { "id": "31", "name": "Resolve", "to": "Done", "category": "Done",
              "required_fields": [ { "id": "resolution", "name": "Resolution",
                                     "allowed": ["Fixed", "Won't Do"] } ] },
        ] })
    );
    assert!(
        fake.requests()[0]
            .query
            .contains("expand=transitions.fields")
    );
}
