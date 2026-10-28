//! The published API as a snapshot (`api/schema.json`): every tool with its schemas and annotations,
//! every prompt with its arguments, and every resource template. The test fails on a breaking change
//! (something removed, a type changed, a new required input) and asks for the snapshot to be
//! regenerated (`make api`) on a compatible one, so each API change is visible in review.

use serde_json::{Map, Value, json};

use super::{GitServer, Policy};
use crate::{repo::RepoHandle, tools::testutil::fixture_repo};

const SNAPSHOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/api/schema.json");

/// The API of a server with every feature on.
fn current() -> Value {
    let (dir, _) = fixture_repo();
    let server = GitServer::new(
        RepoHandle::open_isolated(dir.path()),
        Policy {
            allow_write: true,
            watch_interval: None,
        },
    );
    let tools: Map<String, Value> = server
        .tool_router
        .list_all()
        .into_iter()
        .map(|tool| {
            let hints = tool.annotations.clone().unwrap_or_default();
            let entry = json!({
                "title": hints.title,
                "read_only": hints.read_only_hint,
                "destructive": hints.destructive_hint,
                "input": Value::Object(tool.input_schema.as_ref().clone()),
                "output": tool.output_schema.as_ref().map(|s| Value::Object(s.as_ref().clone())),
            });
            (tool.name.to_string(), entry)
        })
        .collect();
    let prompts: Map<String, Value> = server
        .prompt_router
        .list_all()
        .into_iter()
        .map(|prompt| {
            let arguments: Map<String, Value> = prompt
                .arguments
                .iter()
                .flatten()
                .map(|a| {
                    (
                        a.name.clone(),
                        json!({ "required": a.required == Some(true) }),
                    )
                })
                .collect();
            (prompt.name.clone(), json!({ "arguments": arguments }))
        })
        .collect();
    let templates: Vec<Value> = server
        .resource_templates()
        .resource_templates
        .into_iter()
        .map(|t| Value::String(t.uri_template.clone()))
        .collect();
    json!({ "tools": tools, "prompts": prompts, "resource_templates": templates })
}

/// Changes from `old` to `new` that would break a client written against `old`.
fn breaking(old: &Value, new: &Value) -> Vec<String> {
    let mut problems = Vec::new();
    for (name, before) in entries(&old["tools"]) {
        let Some(after) = new["tools"].get(name) else {
            problems.push(format!("tool `{name}` was removed"));
            continue;
        };
        if before["read_only"] == true && after["read_only"] != true {
            problems.push(format!("tool `{name}` is no longer read-only"));
        }
        let context = format!("tool `{name}`");
        compare_schema(
            &format!("{context} argument"),
            &before["input"],
            &after["input"],
            true,
            &mut problems,
        );
        compare_schema(
            &format!("{context} result field"),
            &before["output"],
            &after["output"],
            false,
            &mut problems,
        );
    }
    for (name, before) in entries(&old["prompts"]) {
        let Some(after) = new["prompts"].get(name) else {
            problems.push(format!("prompt `{name}` was removed"));
            continue;
        };
        for (argument, was) in entries(&before["arguments"]) {
            match after["arguments"].get(argument) {
                None => problems.push(format!("prompt `{name}` argument `{argument}` was removed")),
                Some(now) if now["required"] == true && was["required"] != true => {
                    problems.push(format!(
                        "prompt `{name}` argument `{argument}` is now required"
                    ));
                }
                Some(_) => {}
            }
        }
        for (argument, now) in entries(&after["arguments"]) {
            if now["required"] == true && before["arguments"].get(argument).is_none() {
                problems.push(format!(
                    "prompt `{name}` has a new required argument `{argument}`"
                ));
            }
        }
    }
    let templates = new["resource_templates"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for template in old["resource_templates"].as_array().into_iter().flatten() {
        if !templates.contains(template) {
            problems.push(format!("resource template {template} was removed"));
        }
    }
    problems
}

fn entries(value: &Value) -> impl Iterator<Item = (&String, &Value)> {
    value.as_object().into_iter().flatten()
}

/// Top-level properties: removing one or changing its type breaks clients either way; for inputs,
/// so does a newly required one.
fn compare_schema(
    what: &str,
    before: &Value,
    after: &Value,
    input: bool,
    problems: &mut Vec<String>,
) {
    for (property, was) in entries(&before["properties"]) {
        let Some(now) = after["properties"].get(property) else {
            problems.push(format!("{what} `{property}` was removed"));
            continue;
        };
        let kind = |schema: &Value| schema.get("type").or_else(|| schema.get("$ref")).cloned();
        if kind(was) != kind(now) {
            problems.push(format!(
                "{what} `{property}` changed type from {:?} to {:?}",
                kind(was),
                kind(now)
            ));
        }
    }
    if input {
        let required = |schema: &Value| -> Vec<Value> {
            schema["required"].as_array().cloned().unwrap_or_default()
        };
        let before_required = required(before);
        for property in required(after) {
            if !before_required.contains(&property) {
                problems.push(format!("{what} {property} is now required"));
            }
        }
    }
}

#[test]
fn the_published_api_only_grows() {
    let current = current();
    if std::env::var_os("UPDATE_API").is_some() {
        let text = serde_json::to_string_pretty(&current).unwrap() + "\n";
        std::fs::create_dir_all(std::path::Path::new(SNAPSHOT).parent().unwrap()).unwrap();
        std::fs::write(SNAPSHOT, text).unwrap();
        return;
    }
    let recorded = std::fs::read_to_string(SNAPSHOT)
        .unwrap_or_else(|_| panic!("{SNAPSHOT} is missing; run `make api` to create it"));
    let recorded: Value = serde_json::from_str(&recorded).unwrap();
    let problems = breaking(&recorded, &current);
    assert!(
        problems.is_empty(),
        "breaking API changes; these need a new major version (see docs/versioning.md):\n- {}",
        problems.join("\n- ")
    );
    assert!(
        recorded == current,
        "the API changed compatibly; run `make api` to record it in api/schema.json"
    );
}

#[test]
fn breaking_changes_are_detected() {
    let old = json!({
        "tools": {
            "log": {
                "read_only": true,
                "input": {"properties": {"limit": {"type": "integer"}, "rev": {"type": "string"}}},
                "output": {"properties": {"commits": {"type": "array"}}},
            },
            "gone": {"read_only": true, "input": {}, "output": null},
        },
        "prompts": {"p": {"arguments": {"path": {"required": true}, "repo": {"required": false}}}},
        "resource_templates": ["gitrs://commit/{rev}"],
    });
    let new = json!({
        "tools": {
            "log": {
                "read_only": false,
                "input": {
                    "properties": {"limit": {"type": "string"}, "extra": {"type": "string"}},
                    "required": ["extra"],
                },
                "output": {"properties": {}},
            },
            "added": {"read_only": true, "input": {}, "output": null},
        },
        "prompts": {"p": {"arguments": {"repo": {"required": true}, "new": {"required": true}}}},
        "resource_templates": [],
    });
    let problems = breaking(&old, &new).join("\n");
    for expected in [
        "tool `gone` was removed",
        "`log` is no longer read-only",
        "argument `limit` changed type",
        "argument `rev` was removed",
        "\"extra\" is now required",
        "result field `commits` was removed",
        "argument `path` was removed",
        "argument `repo` is now required",
        "new required argument `new`",
        "template \"gitrs://commit/{rev}\" was removed",
    ] {
        assert!(
            problems.contains(expected),
            "missing `{expected}` in:\n{problems}"
        );
    }
    assert!(
        breaking(&old, &old).is_empty(),
        "nothing changed, nothing breaks"
    );
}
