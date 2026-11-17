mod support;

use std::collections::HashMap;
use std::path::Path;

use jira_review::auth::{self, Auth, Credential};
use jira_review::jira::Jira;
use support::Fake;

fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |k| map.get(k).cloned()
}

fn no_keyring() -> Result<Option<String>, String> {
    Ok(None)
}

#[tokio::test]
async fn jira_cloud_sends_basic_auth_from_email_and_token() {
    let fake = Fake::start().await;
    let cred = Credential::from_fields(
        env(&[
            ("JIRA_URL", &format!("{}/", fake.url)),
            ("JIRA_EMAIL", "me@x.com"),
            ("JIRA_TOKEN", "t0k"),
        ]),
        "test",
    )
    .unwrap();
    assert_eq!(cred.url, fake.url, "a trailing slash is trimmed");
    Jira::new(cred).issue("WRK-1").await.unwrap();
    // base64("me@x.com:t0k")
    assert_eq!(
        fake.requests()[0].auth.as_deref(),
        Some("Basic bWVAeC5jb206dDBr")
    );
}

#[tokio::test]
async fn data_center_sends_its_personal_access_token_as_bearer() {
    let fake = Fake::start().await;
    let cred = Credential::from_fields(
        env(&[("JIRA_URL", &fake.url), ("JIRA_PAT", "pat-123")]),
        "test",
    )
    .unwrap();
    assert!(matches!(cred.auth, Auth::Bearer { .. }));
    Jira::new(cred).issue("WRK-1").await.unwrap();
    assert_eq!(fake.requests()[0].auth.as_deref(), Some("Bearer pat-123"));
}

#[test]
fn both_schemes_or_neither_is_refused_by_name() {
    let both = Credential::from_fields(
        env(&[
            ("JIRA_URL", "https://j"),
            ("JIRA_EMAIL", "a"),
            ("JIRA_TOKEN", "b"),
            ("JIRA_PAT", "c"),
        ]),
        "test",
    )
    .unwrap_err();
    assert!(both.contains("both") && both.contains("JIRA_PAT"), "{both}");
    let neither = Credential::from_fields(env(&[("JIRA_URL", "https://j")]), "test").unwrap_err();
    assert!(
        neither.contains("JIRA_EMAIL and JIRA_TOKEN") && neither.contains("JIRA_PAT"),
        "{neither}"
    );
    let half = Credential::from_fields(
        env(&[("JIRA_URL", "https://j"), ("JIRA_EMAIL", "a")]),
        "test",
    )
    .unwrap_err();
    assert!(half.contains("JIRA_TOKEN"), "{half}");
    let url = Credential::from_fields(env(&[("JIRA_PAT", "c")]), "test").unwrap_err();
    assert!(url.contains("JIRA_URL"), "{url}");
}

#[test]
fn a_dot_env_file_is_read_when_the_environment_has_no_jira_url() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join(".env");
    std::fs::write(
        &file,
        "JIRA_URL=https://dc.example.com\nJIRA_PAT=from-file\n",
    )
    .unwrap();
    let cred = auth::resolve_from(env(&[]), None, &file, no_keyring, "default").unwrap();
    assert_eq!(cred.url, "https://dc.example.com");
    assert_eq!(cred.header(), "Bearer from-file");
}

#[test]
fn the_environment_wins_over_dot_env_whole_without_mixing_fields() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join(".env");
    std::fs::write(
        &file,
        "JIRA_URL=https://file\nJIRA_EMAIL=f@x\nJIRA_TOKEN=file\n",
    )
    .unwrap();
    let e = env(&[("JIRA_URL", "https://env"), ("JIRA_PAT", "env-pat")]);
    let cred = auth::resolve_from(e, None, &file, no_keyring, "default").unwrap();
    assert_eq!(cred.url, "https://env");
    assert_eq!(cred.header(), "Bearer env-pat");
    // The environment's JIRA_URL alone does not borrow the file's token.
    let e = env(&[("JIRA_URL", "https://env")]);
    assert!(auth::resolve_from(e, None, &file, no_keyring, "default").is_err());
}

#[test]
fn the_keyring_is_last_and_holds_the_same_fields() {
    let missing = Path::new("/nonexistent/.env");
    let block = || Ok(Some("JIRA_URL=https://k\nJIRA_PAT=kp\n".to_string()));
    let cred = auth::resolve_from(env(&[]), None, missing, block, "work").unwrap();
    assert_eq!(cred.header(), "Bearer kp");
    let e = auth::resolve_from(env(&[]), None, missing, no_keyring, "work").unwrap_err();
    assert!(e.contains("login --profile work"), "{e}");
    // A named env file that is missing is an error, not a fall-through.
    let e = auth::resolve_from(env(&[]), Some(missing), missing, block, "work").unwrap_err();
    assert!(e.contains("nonexistent"), "{e}");
}
