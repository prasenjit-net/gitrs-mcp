//! The part of an agent's config file setup edits: one server entry. JSON and TOML files both
//! implement [`Document`], so the add/remove/compare logic is written once.

/// What gets registered: the command that starts the server and its arguments. Never secrets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Registered {
    pub command: String,
    pub args: Vec<String>,
}

impl Registered {
    /// How it reads on a terminal.
    pub fn summary(&self) -> String {
        std::iter::once(self.command.as_str())
            .chain(self.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// What a config file holds under our server name.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Existing {
    Missing,
    Entry(Registered),
    /// Something that isn't a plain command entry (a URL server, say). Shown as written.
    Other(String),
}

/// A parsed config file, edited in place so everything else in it is kept.
pub(crate) trait Document {
    fn get(&self, name: &str) -> Existing;
    fn set(&mut self, name: &str, server: &Registered);
    /// Remove the entry, and any container left empty by it. False when there was none.
    fn remove(&mut self, name: &str) -> bool;
    /// True when nothing is left in the file.
    fn is_empty(&self) -> bool;
    fn render(&self) -> String;
}
