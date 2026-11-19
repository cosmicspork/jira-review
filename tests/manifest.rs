//! `tracon-plugin.toml` must mirror what `serve` lists: the node checks it
//! against `tools/list` at start.

use jira_review::mcp;

fn manifest() -> toml::Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tracon-plugin.toml");
    toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn the_manifest_tools_are_the_ones_serve_lists() {
    let m = manifest();
    let declared: Vec<(String, String)> = m["tool"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t["name"].as_str().unwrap().to_string(),
                t["hint"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let listed: Vec<(String, String)> = mcp::definitions(true, "")
        .iter()
        .map(|t| {
            let read = t["annotations"]["readOnlyHint"] == true;
            (
                t["name"].as_str().unwrap().to_string(),
                if read { "read" } else { "write" }.to_string(),
            )
        })
        .collect();
    assert_eq!(declared, listed);
    let consequential: Vec<&str> = m["tool"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t.get("consequential").and_then(toml::Value::as_bool) == Some(true))
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(consequential, ["transition"]);
}

#[test]
fn the_manifest_is_contract_1_and_versioned_with_the_crate() {
    let m = manifest();
    assert_eq!(m["name"].as_str(), Some("jira-review"));
    assert_eq!(m["version"].as_str(), Some(env!("CARGO_PKG_VERSION")));
    assert_eq!(m["contract"].as_integer(), Some(1));
    assert_eq!(
        m["exec"].as_array().unwrap(),
        &[
            toml::Value::from("bin/jira-review"),
            toml::Value::from("serve")
        ]
    );
    let credential = &m["credential"].as_array().unwrap()[0];
    assert_eq!(credential["name"].as_str(), Some("jira"));
    assert_eq!(credential["pattern"].as_str(), Some("jira-*"));
    assert!(credential.get("lend").is_none());
    assert_eq!(
        m["formats"].as_array().unwrap(),
        &[toml::Value::from("jira_wiki")]
    );
}
