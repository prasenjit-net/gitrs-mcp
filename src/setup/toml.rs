//! Editing Codex's `config.toml` (`[mcp_servers.<name>]`) with comments and layout kept.

use toml_edit::{Array, DocumentMut, Item, Table, value};

use super::document::{Document, Existing, Registered};

const TABLE: &str = "mcp_servers";

pub(crate) struct TomlDocument(DocumentMut);

impl TomlDocument {
    pub fn empty() -> Self {
        Self(DocumentMut::new())
    }

    /// Parse `text`; a missing or blank file is an empty document.
    pub fn parse(text: &str) -> Result<Self, String> {
        let doc: DocumentMut = text
            .parse()
            .map_err(|err| format!("it isn't valid TOML ({err})"))?;
        if doc.get(TABLE).is_some_and(|item| !item.is_table()) {
            return Err(format!("`{TABLE}` isn't a TOML table"));
        }
        Ok(Self(doc))
    }

    fn servers(&self) -> Option<&Table> {
        self.0.get(TABLE)?.as_table()
    }
}

impl Document for TomlDocument {
    fn get(&self, name: &str) -> Existing {
        let Some(item) = self.servers().and_then(|s| s.get(name)) else {
            return Existing::Missing;
        };
        match as_registered(item) {
            Some(entry) => Existing::Entry(entry),
            None => Existing::Other(item.to_string().trim().to_owned()),
        }
    }

    fn set(&mut self, name: &str, server: &Registered) {
        let servers = self.0.entry(TABLE).or_insert_with(|| {
            let mut table = Table::new();
            // Only `[mcp_servers.gitrs]` is written, not a bare `[mcp_servers]` header too.
            table.set_implicit(true);
            Item::Table(table)
        });
        let Some(servers) = servers.as_table_mut() else {
            return;
        };
        // Update a plain command entry where it stands, so the comments around it stay.
        if let Some(existing) = servers.get_mut(name).filter(|i| as_registered(i).is_some()) {
            fill(existing, server);
            return;
        }
        let mut entry = Item::Table(Table::new());
        fill(&mut entry, server);
        servers.insert(name, entry);
    }

    fn remove(&mut self, name: &str) -> bool {
        let Some(servers) = self.0.get_mut(TABLE).and_then(Item::as_table_mut) else {
            return false;
        };
        let removed = servers.remove(name).is_some();
        if servers.is_empty() {
            self.0.remove(TABLE);
        }
        removed
    }

    fn is_empty(&self) -> bool {
        self.0.to_string().trim().is_empty()
    }

    fn render(&self) -> String {
        self.0.to_string()
    }
}

fn fill(entry: &mut Item, server: &Registered) {
    entry["command"] = value(server.command.as_str());
    entry["args"] = value(server.args.iter().collect::<Array>());
}

fn as_registered(item: &Item) -> Option<Registered> {
    let entry = item.as_table_like()?;
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

    #[test]
    fn a_blank_file_gets_a_single_table() {
        let mut doc = TomlDocument::parse("").unwrap();
        assert_eq!(doc.get("gitrs"), Existing::Missing);
        doc.set("gitrs", &server());
        assert_eq!(
            doc.render(),
            "[mcp_servers.gitrs]\ncommand = \"/bin/gitrs-mcp\"\nargs = [\"--repo\", \"/work\"]\n"
        );
    }

    #[test]
    fn comments_and_other_settings_survive_an_add_and_a_remove() {
        let text = "# my settings\nmodel = \"o3\"  # the model\n\n[mcp_servers.other]\ncommand = \"x\" # keep\n";
        let mut doc = TomlDocument::parse(text).unwrap();
        doc.set("gitrs", &server());
        let rendered = doc.render();
        assert!(rendered.starts_with("# my settings\nmodel = \"o3\"  # the model\n"));
        assert!(rendered.contains("command = \"x\" # keep"));
        assert!(doc.remove("gitrs"));
        assert_eq!(doc.render(), text);
    }

    #[test]
    fn replacing_an_entry_keeps_the_comments_around_it() {
        let mut doc =
            TomlDocument::parse("# mine\n[mcp_servers.gitrs]\ncommand = \"/old\"\n").unwrap();
        doc.set("gitrs", &server());
        assert!(
            doc.render()
                .starts_with("# mine\n[mcp_servers.gitrs]\ncommand = \"/bin/gitrs-mcp\"")
        );
        let mut other = TomlDocument::parse("[mcp_servers.gitrs]\nurl = \"u\"\n").unwrap();
        other.set("gitrs", &server());
        assert!(
            !other.render().contains("url"),
            "a different kind of entry is replaced whole"
        );
    }

    #[test]
    fn entries_are_read_back() {
        let doc = TomlDocument::parse(
            "[mcp_servers.gitrs]\ncommand = \"g\"\nargs = [\"-x\"]\nenv = { A = \"1\" }\n\n[mcp_servers.web]\nurl = \"u\"\n",
        )
        .unwrap();
        assert_eq!(
            doc.get("gitrs"),
            Existing::Entry(Registered {
                command: "g".into(),
                args: vec!["-x".into()]
            })
        );
        assert!(matches!(doc.get("web"), Existing::Other(raw) if raw.contains("url")));
    }

    #[test]
    fn removing_the_only_entry_empties_the_document() {
        let mut doc = TomlDocument::parse("[mcp_servers.gitrs]\ncommand = \"g\"\n").unwrap();
        assert!(doc.remove("gitrs") && doc.is_empty());
        assert!(!doc.remove("gitrs"));
    }

    #[test]
    fn invalid_files_are_refused() {
        assert!(
            TomlDocument::parse("[mcp_servers")
                .err()
                .unwrap()
                .contains("TOML")
        );
        assert!(TomlDocument::parse("mcp_servers = 3").is_err());
    }
}
