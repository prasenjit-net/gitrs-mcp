use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{byte_budget, clip_utf8, resolve_object};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NotesArgs {
    /// Object whose notes to show (default `HEAD`). Usually a commit; tags aren't peeled, so an
    /// annotated tag's notes are those attached to the tag object, as in `git notes show`.
    pub rev: Option<String>,
    /// Notes ref to read, e.g. `refs/notes/review` or just `review` (expanded to `refs/notes/review`).
    /// Default: `core.notesRef` (normally `refs/notes/commits`) plus any `notes.displayRef` refs.
    #[serde(rename = "ref")]
    pub notes_ref: Option<String>,
    /// Maximum bytes of text returned per note (default 65536, max 1048576).
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct Note {
    /// Notes ref the note comes from, e.g. `refs/notes/commits`.
    #[serde(rename = "ref")]
    pub notes_ref: String,
    /// Full SHA of the note's blob.
    pub note_id: String,
    /// The note text. Invalid UTF-8 is replaced with U+FFFD.
    pub text: String,
    /// True if `text` was cut at `max_bytes`.
    pub truncated: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct NotesResult {
    /// Full SHA of the object the notes are attached to.
    pub id: String,
    /// One note per notes ref that annotates the object, in display order. Empty when there are none.
    pub notes: Vec<Note>,
}

/// The notes attached to an object, like `git notes show`.
pub fn notes(repo: &gix::Repository, args: NotesArgs) -> anyhow::Result<NotesResult> {
    let id = resolve_object(repo, args.rev.as_deref())?.detach();
    let mut platform = repo.notes()?;
    if let Some(name) = args.notes_ref.as_deref() {
        platform = platform.with_refs([expand_notes_ref(name)])?;
    }
    let budget = byte_budget(args.max_bytes);
    let notes = platform
        .get(id)?
        .into_iter()
        .map(|note| {
            let full = String::from_utf8_lossy(&note.blob.data);
            let (text, truncated) = clip_utf8(&full, budget);
            Note {
                notes_ref: note.reference.as_bstr().to_string(),
                note_id: note.blob.id.to_string(),
                text: text.to_owned(),
                truncated,
            }
        })
        .collect();
    Ok(NotesResult {
        id: id.to_string(),
        notes,
    })
}

/// Expand a short notes ref the way `git notes --ref` does: `review` and `notes/review` both mean
/// `refs/notes/review`.
fn expand_notes_ref(name: &str) -> String {
    if name.starts_with("refs/") {
        name.to_owned()
    } else {
        format!("refs/notes/{}", name.strip_prefix("notes/").unwrap_or(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::{empty_repo, fixture_repo, git_stdout, open, run_git};

    fn args(rev: Option<&str>, notes_ref: Option<&str>) -> NotesArgs {
        NotesArgs {
            rev: rev.map(Into::into),
            notes_ref: notes_ref.map(Into::into),
            max_bytes: None,
        }
    }

    #[test]
    fn default_notes_match_git() {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        run_git(root, &["notes", "add", "-m", "reviewed by ä", "HEAD"]);
        let repo = open(root);
        let res = notes(&repo, args(None, None)).unwrap();
        assert_eq!(res.id, git_stdout(root, &["rev-parse", "HEAD"]).trim());
        assert_eq!(res.notes.len(), 1);
        let note = &res.notes[0];
        assert_eq!(note.notes_ref, "refs/notes/commits");
        assert_eq!(note.text, git_stdout(root, &["notes", "show", "HEAD"]));
        assert_eq!(
            note.note_id,
            git_stdout(root, &["notes", "list", "HEAD"]).trim()
        );
        assert!(!note.truncated);

        assert!(
            notes(&repo, args(Some("HEAD~1"), None))
                .unwrap()
                .notes
                .is_empty()
        );
    }

    #[test]
    fn named_notes_ref_and_truncation() {
        let (dir, _) = fixture_repo();
        let root = dir.path();
        run_git(
            root,
            &["notes", "--ref", "review", "add", "-m", "looks good", "v1"],
        );
        let repo = open(root);
        assert!(
            notes(&repo, args(Some("v1"), None))
                .unwrap()
                .notes
                .is_empty()
        );
        for name in ["review", "notes/review", "refs/notes/review"] {
            let res = notes(&repo, args(Some("v1"), Some(name))).unwrap();
            assert_eq!(res.notes[0].notes_ref, "refs/notes/review", "{name}");
            assert_eq!(res.notes[0].text, "looks good\n");
        }
        let clipped = notes(
            &repo,
            NotesArgs {
                rev: Some("v1".into()),
                notes_ref: Some("review".into()),
                max_bytes: Some(5),
            },
        )
        .unwrap();
        assert_eq!(clipped.notes[0].text, "looks");
        assert!(clipped.notes[0].truncated);
        // A notes ref that doesn't exist simply has no notes.
        assert!(
            notes(&repo, args(None, Some("missing")))
                .unwrap()
                .notes
                .is_empty()
        );
    }

    #[test]
    fn errors_and_empty_repo() {
        let (_dir, repo) = fixture_repo();
        let err = format!("{:#}", notes(&repo, args(Some("nope"), None)).unwrap_err());
        assert!(err.contains("cannot resolve `nope`"), "{err}");
        let (_dir, empty) = empty_repo();
        let err = format!("{:#}", notes(&empty, args(None, None)).unwrap_err());
        assert!(err.contains("no commits yet"), "{err}");
    }

    #[test]
    fn args_use_ref_as_the_field_name() {
        let parsed: NotesArgs = serde_json::from_str(r#"{"ref":"review"}"#).unwrap();
        assert_eq!(parsed.notes_ref.as_deref(), Some("review"));
        assert!(parsed.rev.is_none());
    }
}
