//! A smoke test against a real Jira: reads only. Skipped unless
//! `JIRA_LIVE=1` and a credential is in the environment; run with
//! `JIRA_LIVE=1 JIRA_LIVE_KEY=WRK-1 cargo test --test live -- --ignored`.

use jira_review::auth;
use jira_review::jira::Jira;

#[tokio::test]
#[ignore = "needs a real Jira: JIRA_LIVE=1 with JIRA_URL and a token"]
async fn search_and_transitions_against_a_real_jira() {
    if std::env::var("JIRA_LIVE").as_deref() != Ok("1") {
        eprintln!("JIRA_LIVE is not 1; skipping");
        return;
    }
    let jira = Jira::new(auth::resolve(None, auth::DEFAULT_PROFILE).unwrap());
    let found = jira.search("order by updated desc", 1).await.unwrap();
    let key = std::env::var("JIRA_LIVE_KEY")
        .ok()
        .or_else(|| found["issues"][0]["key"].as_str().map(str::to_string))
        .expect("an issue to read");
    let transitions = jira.transitions(&key).await.unwrap();
    assert!(transitions["transitions"].is_array());
}
