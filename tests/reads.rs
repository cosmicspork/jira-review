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
                "priority": "Medium", "assignee": null, "duedate": null, "parent": null,
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

#[tokio::test]
async fn a_paged_search_answers_as_tracon_did_and_follows_its_cursor() {
    let fake = Fake::start().await;
    let jira = fake.client();
    let read = |args: serde_json::Value| {
        let jira = jira.clone();
        async move { jira_review::mcp::read("search", &args, &jira).await }
    };
    let first = read(json!({ "jql": "project = WRK", "page": true, "limit": 2 }))
        .await
        .unwrap();
    assert_eq!(first["site"], fake.url.as_str());
    assert_eq!(first["account_email"], "me@example.com");
    assert_eq!(first["next_cursor"], "page-2");
    assert_eq!(
        first["issues"][0],
        json!({ "id": "10000", "key": "WRK-1", "summary": "Port the feed", "status": "To Do",
                "status_category": "new", "priority": "Medium", "issue_type": "Task",
                "labels": ["etl", "Stale"], "parent": null })
    );
    assert_eq!(first["issues"].as_array().unwrap().len(), 2);

    let next = read(json!({ "jql": "project = WRK", "cursor": "page-2", "limit": 2 }))
        .await
        .unwrap();
    assert_eq!(next["issues"].as_array().unwrap().len(), 1);
    assert_eq!(next["next_cursor"], serde_json::Value::Null);

    let r = fake.requests();
    assert_eq!(r.len(), 2);
    assert!(r.iter().all(|r| r.path == "/rest/api/3/search/jql"));
    assert!(
        r[0].query
            .contains("fields=summary%2Cstatus%2Cpriority%2Cissuetype%2Clabels%2Cparent"),
        "{}",
        r[0].query
    );
    assert!(!r[0].query.contains("nextPageToken"), "{}", r[0].query);
    assert!(
        r[1].query.contains("nextPageToken=page-2"),
        "{}",
        r[1].query
    );

    fake.clear();
    let all = read(json!({ "jql": "project = WRK", "page": true, "limit": 500 }))
        .await
        .unwrap();
    assert_eq!(all["issues"].as_array().unwrap().len(), 3);
    assert_eq!(all["next_cursor"], serde_json::Value::Null);
    assert!(fake.requests()[0].query.contains("maxResults=100"));
    let unpaged = read(json!({ "jql": "project = WRK" })).await.unwrap();
    assert!(unpaged.get("next_cursor").is_none(), "{unpaged}");
}

#[tokio::test]
async fn a_paged_search_on_data_center_pages_by_offset() {
    let fake = Fake::start().await;
    fake.data_center();
    let jira = fake.data_center_client();
    let first = jira.search_page("project = WRK", 2, None).await.unwrap();
    assert_eq!(first["account_email"], serde_json::Value::Null);
    assert_eq!(first["issues"].as_array().unwrap().len(), 2);
    assert_eq!(first["next_cursor"], "2");
    let next = jira
        .search_page("project = WRK", 2, Some("2"))
        .await
        .unwrap();
    assert_eq!(next["issues"][0]["key"], "WRK-2");
    assert_eq!(next["next_cursor"], serde_json::Value::Null);
    let paths: Vec<String> = fake.requests().into_iter().map(|r| r.path).collect();
    assert_eq!(
        paths,
        [
            "/rest/api/3/search/jql",
            "/rest/api/2/search",
            "/rest/api/3/search/jql",
            "/rest/api/2/search"
        ]
    );
    assert!(fake.requests()[3].query.contains("startAt=2"));

    let e = jira
        .search_page("project = WRK", 2, Some("page-2"))
        .await
        .unwrap_err();
    assert_eq!(e, "bad cursor: page-2");
}

#[tokio::test]
async fn paging_arguments_are_checked_before_anything_is_sent() {
    let fake = Fake::start().await;
    let jira = fake.client();
    for (args, says) in [
        (
            json!({ "jql": "x", "cursor": 2 }),
            "cursor expects a string",
        ),
        (
            json!({ "jql": "x", "page": "yes" }),
            "page expects true or false",
        ),
        (json!({ "jql": " ", "page": true }), "jql is required"),
    ] {
        let e = jira_review::mcp::read("search", &args, &jira)
            .await
            .unwrap_err();
        assert_eq!(e, says);
    }
    assert!(fake.requests().is_empty());
}
