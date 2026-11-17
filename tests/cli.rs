//! The CLI's change files: propose writes one, show renders it, apply sends
//! it once and records the outcome, and a stale one exits 3 untouched.

mod support;

use std::path::Path;

use serde_json::Value;
use support::Fake;
use tokio::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_jira-review");

async fn run(fake: &Fake, dir: &Path, args: &[&str]) -> (i32, String) {
    let out = Command::new(BIN)
        .env_clear()
        .envs(fake.cloud())
        .env("JIRA_REVIEW_STATE", dir.join("state"))
        .current_dir(dir)
        .args(args)
        .output()
        .await
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_change_file_is_proposed_shown_and_applied_once() {
    let fake = Fake::start().await;
    let dir = tempfile::tempdir().unwrap();
    let (code, out) = run(
        &fake,
        dir.path(),
        &[
            "update",
            "WRK-1",
            "--summary",
            "Ship it",
            "--remove-label",
            "stale",
            "--out",
            "c.json",
        ],
    )
    .await;
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("  + Ship it") && out.contains("jira-review apply c.json"),
        "{out}"
    );
    assert!(fake.writes().is_empty());

    let (code, shown) = run(&fake, dir.path(), &["show", "c.json", "--no-color"]).await;
    assert_eq!(code, 0);
    assert!(shown.contains("labels:\n  - Stale\n"), "{shown}");

    let (code, out) = run(&fake, dir.path(), &["apply", "c.json"]).await;
    assert_eq!(code, 0, "{out}");
    assert_eq!(fake.writes().len(), 1);
    let file: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("c.json")).unwrap()).unwrap();
    assert_eq!(file["outcome"]["status"], "approved");

    let (code, out) = run(&fake, dir.path(), &["apply", "c.json"]).await;
    assert_eq!(code, 1);
    assert!(out.contains("already decided"), "{out}");
    assert_eq!(fake.writes().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stale_change_file_exits_3_and_is_left_as_it_was() {
    let fake = Fake::start().await;
    let dir = tempfile::tempdir().unwrap();
    let (code, out) = run(
        &fake,
        dir.path(),
        &[
            "comment",
            "WRK-2",
            "--body",
            "**Done**",
            "--markdown",
            "--out",
            "c.json",
        ],
    )
    .await;
    assert_eq!(code, 0, "{out}");
    let before = std::fs::read_to_string(dir.path().join("c.json")).unwrap();
    assert!(before.contains("\"body\": \"*Done*\""), "{before}");
    fake.bump("WRK-2");
    let (code, out) = run(&fake, dir.path(), &["apply", "c.json"]).await;
    assert_eq!(code, 3, "{out}");
    assert!(
        out.contains("WRK-2 moved since this change was proposed"),
        "{out}"
    );
    assert!(fake.writes().is_empty());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("c.json")).unwrap(),
        before
    );
}
