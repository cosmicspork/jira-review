//! Shared by the integration tests; each uses what it needs.
#![allow(dead_code)]

pub mod fake_jira;

pub use fake_jira::Fake;

use serde_json::Value;

/// The properties `tools/list` marks with `x-tracon-format: jira_wiki`.
pub const WIKI_FIELDS: [&str; 4] = [
    "update.description",
    "create.description",
    "create.issues[].description",
    "comment.body",
];

/// Each `tool.property` (or `tool.array[].property`) whose schema carries
/// an `x-tracon-format`, which must be `jira_wiki`.
pub fn formatted(tools: &[Value]) -> Vec<String> {
    let mut found = Vec::new();
    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        let props = tool["inputSchema"]["properties"].as_object().unwrap();
        for (prop, schema) in props {
            let mut marked = vec![(prop.clone(), schema)];
            if let Some(items) = schema["items"]["properties"].as_object() {
                marked.extend(items.iter().map(|(k, v)| (format!("{prop}[].{k}"), v)));
            }
            for (path, schema) in marked {
                if let Some(format) = schema.get("x-tracon-format") {
                    assert_eq!(format, "jira_wiki", "{name}.{path}");
                    assert_eq!(schema["type"], "string", "{name}.{path}");
                    found.push(format!("{name}.{path}"));
                }
            }
        }
    }
    found
}
