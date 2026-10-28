//! Editing JSON config files (`mcpServers` or `servers`), keeping other entries and key order.
//!
//! Files with comments or trailing commas (VS Code accepts them) don't parse as JSON, so they are
//! refused rather than rewritten: a rewrite would drop the comments.

use serde_json::{Map, Value, json};

use super::document::{Document, Existing, Registered};

pub(crate) struct JsonDocument {
    root: Map<String, Value>,
    /// The top-level object holding the servers.
    servers_key: &'static str,
    /// Whether entries carry `"type": "stdio"`, which Cursor and VS Code expect.
    typed: bool,
}

impl JsonDocument {
    pub fn empty(servers_key: &'static str, typed: bool) -> Self {
        Self {
            root: Map::new(),
            servers_key,
            typed,
        }
    }

    /// Parse `text`; a missing or blank file is an empty document.
    pub fn parse(text: &str, servers_key: &'static str, typed: bool) -> Result<Self, String> {
        let root = if text.trim().is_empty() {
            Map::new()
        } else {
            match serde_json::from_str(text) {
                Ok(Value::Object(root)) => root,
                Ok(_) => return Err("its top level isn't a JSON object".to_owned()),
                Err(err) => {
                    return Err(format!(
                        "it isn't valid JSON ({err}); comments and trailing commas aren't supported"
                    ));
                }
            }
        };
        if root.get(servers_key).is_some_and(|v| !v.is_object()) {
            return Err(format!("`{servers_key}` isn't a JSON object"));
        }
        Ok(Self {
            root,
            servers_key,
            typed,
        })
    }

    fn servers(&self) -> Option<&Map<String, Value>> {
        self.root.get(self.servers_key)?.as_object()
    }

    fn entry_value(&self, server: &Registered) -> Value {
        let mut entry = Map::new();
        if self.typed {
            entry.insert("type".into(), json!("stdio"));
        }
        entry.insert("command".into(), json!(server.command));
        entry.insert("args".into(), json!(server.args));
        Value::Object(entry)
    }
}

impl Document for JsonDocument {
    fn get(&self, name: &str) -> Existing {
        let Some(value) = self.servers().and_then(|s| s.get(name)) else {
            return Existing::Missing;
        };
        match as_registered(value) {
            Some(entry) => Existing::Entry(entry),
            None => Existing::Other(value.to_string()),
        }
    }

    fn set(&mut self, name: &str, server: &Registered) {
        let entry = self.entry_value(server);
        let servers = self
            .root
            .entry(self.servers_key)
            .or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(servers) = servers {
            servers.insert(name.to_owned(), entry);
        }
    }

    fn remove(&mut self, name: &str) -> bool {
        let Some(Value::Object(servers)) = self.root.get_mut(self.servers_key) else {
            return false;
        };
        let removed = servers.shift_remove(name).is_some();
        if servers.is_empty() {
            self.root.shift_remove(self.servers_key);
        }
        removed
    }

    fn is_empty(&self) -> bool {
        self.root.is_empty()
    }

    fn render(&self) -> String {
        // Two-space indentation, which is what these tools write themselves.
        let mut text = serde_json::to_string_pretty(&self.root).unwrap_or_default();
        text.push('\n');
        text
    }
}

fn as_registered(value: &Value) -> Option<Registered> {
    let entry = value.as_object()?;
    let command = entry.get("command")?.as_str()?.to_owned();
    let args = match entry.get("args") {
        None => Vec::new(),
        Some(args) => args
            .as_array()?
            .iter()
            .map(|a| a.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()?,
    };
    Some(Registered { command, args })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> Registered {
        Registered {
            command: "/bin/gitrs-mcp".into(),
            args: vec!["--repo".into(), "/work".into()],
        }
    }

    fn parse(text: &str) -> JsonDocument {
        JsonDocument::parse(text, "mcpServers", false).unwrap()
    }

    #[test]
    fn a_blank_file_gets_the_entry_and_nothing_else() {
        let mut doc = parse("  \n");
        assert_eq!(doc.get("gitrs"), Existing::Missing);
        doc.set("gitrs", &server());
        assert_eq!(
            doc.render(),
            "{\n  \"mcpServers\": {\n    \"gitrs\": {\n      \"command\": \"/bin/gitrs-mcp\",\n      \"args\": [\n        \"--repo\",\n        \"/work\"\n      ]\n    }\n  }\n}\n"
        );
    }

    #[test]
    fn other_servers_and_key_order_are_kept() {
        let text = "{\n  \"theme\": \"dark\",\n  \"mcpServers\": {\n    \"zeta\": {\n      \"command\": \"z\"\n    },\n    \"alpha\": {\n      \"url\": \"https://example.com\"\n    }\n  },\n  \"last\": 1\n}\n";
        let mut doc = parse(text);
        doc.set("gitrs", &server());
        let rendered = doc.render();
        let order = |s: &str| rendered.find(s).unwrap();
        assert!(order("theme") < order("zeta") && order("zeta") < order("alpha"));
        assert!(order("alpha") < order("gitrs") && order("gitrs") < order("last"));
        assert!(doc.remove("gitrs"));
        assert_eq!(doc.render(), text, "removing restores the original bytes");
    }

    #[test]
    fn entries_are_read_back_and_odd_ones_reported() {
        let doc = parse(
            "{\"mcpServers\": {\"gitrs\": {\"type\": \"stdio\", \"command\": \"g\", \"args\": [\"-x\"], \"env\": {}}, \"web\": {\"url\": \"u\"}}}",
        );
        assert_eq!(
            doc.get("gitrs"),
            Existing::Entry(Registered {
                command: "g".into(),
                args: vec!["-x".into()]
            })
        );
        assert!(matches!(doc.get("web"), Existing::Other(raw) if raw.contains("\"url\"")));
        assert_eq!(
            parse("{\"mcpServers\": {\"g\": {\"command\": \"c\"}}}").get("g"),
            Existing::Entry(Registered {
                command: "c".into(),
                args: vec![]
            }),
            "args are optional"
        );
    }

    #[test]
    fn typed_entries_say_stdio() {
        let mut doc = JsonDocument::parse("{}", "servers", true).unwrap();
        doc.set("gitrs", &server());
        assert!(doc.render().contains("\"type\": \"stdio\""));
        assert!(doc.render().starts_with("{\n  \"servers\""));
    }

    #[test]
    fn removing_the_only_entry_empties_the_document() {
        let mut doc = parse("{\"mcpServers\": {\"gitrs\": {\"command\": \"g\"}}}");
        assert!(doc.remove("gitrs") && doc.is_empty());
        assert!(!doc.remove("gitrs"));
        assert!(!parse("{}").remove("gitrs"));
    }

    #[test]
    fn unparsable_files_are_refused_with_a_reason() {
        let comment = JsonDocument::parse("{ // note\n}", "mcpServers", false);
        assert!(comment.err().unwrap().contains("comments"));
        assert!(JsonDocument::parse("[]", "mcpServers", false).is_err());
        assert!(JsonDocument::parse("{\"mcpServers\": []}", "mcpServers", false).is_err());
    }
}
