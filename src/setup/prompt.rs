//! The questions setup asks on a terminal. Input and output are injected so tests can script them.

use std::io::{BufRead, Write};

use anyhow::Context;

pub(crate) struct Terminal<'a> {
    pub input: &'a mut dyn BufRead,
    pub out: &'a mut dyn Write,
}

/// How many times a malformed answer is asked again before giving up.
const ATTEMPTS: usize = 3;

impl Terminal<'_> {
    pub fn say(&mut self, text: &str) -> anyhow::Result<()> {
        writeln!(self.out, "{text}").context("cannot write to the terminal")
    }

    fn ask(&mut self, question: &str) -> anyhow::Result<String> {
        write!(self.out, "{question} ").context("cannot write to the terminal")?;
        self.out.flush().context("cannot write to the terminal")?;
        let mut line = String::new();
        let read = self
            .input
            .read_line(&mut line)
            .context("cannot read your answer")?;
        anyhow::ensure!(read > 0, "no answer: the input ended");
        Ok(line.trim().to_lowercase())
    }

    /// A yes/no question; an empty answer means `default`.
    pub fn confirm(&mut self, question: &str, default: bool) -> anyhow::Result<bool> {
        let hint = if default { "[Y/n]" } else { "[y/N]" };
        for _ in 0..ATTEMPTS {
            match self.ask(&format!("{question} {hint}"))?.as_str() {
                "" => return Ok(default),
                "y" | "yes" => return Ok(true),
                "n" | "no" => return Ok(false),
                _ => self.say("Please answer y or n.")?,
            }
        }
        anyhow::bail!("no valid answer to: {question}")
    }

    /// A numbered checklist. The answer is `all` (or empty), `none`, or numbers such as `1,3`.
    /// Returns the chosen indexes, in list order.
    pub fn choose(&mut self, question: &str, items: &[String]) -> anyhow::Result<Vec<usize>> {
        for (i, item) in items.iter().enumerate() {
            self.say(&format!("  {}) {item}", i + 1))?;
        }
        for _ in 0..ATTEMPTS {
            let answer = self.ask(&format!("{question} [all]"))?;
            if let Some(chosen) = parse_choice(&answer, items.len()) {
                return Ok(chosen);
            }
            self.say(&format!(
                "Enter all, none, or numbers from 1 to {}, such as 1,3.",
                items.len()
            ))?;
        }
        anyhow::bail!("no valid answer to: {question}")
    }
}

fn parse_choice(answer: &str, count: usize) -> Option<Vec<usize>> {
    match answer {
        "" | "all" | "a" => return Some((0..count).collect()),
        "none" | "n" | "0" => return Some(Vec::new()),
        _ => {}
    }
    let mut chosen = answer
        .split([',', ' '])
        .filter(|part| !part.is_empty())
        .map(|part| {
            part.parse::<usize>()
                .ok()
                .filter(|n| (1..=count).contains(n))
                .map(|n| n - 1)
        })
        .collect::<Option<Vec<_>>>()?;
    chosen.sort_unstable();
    chosen.dedup();
    Some(chosen)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run<T>(
        answers: &str,
        f: impl FnOnce(&mut Terminal) -> anyhow::Result<T>,
    ) -> (anyhow::Result<T>, String) {
        let mut input = answers.as_bytes();
        let mut out = Vec::new();
        let result = f(&mut Terminal {
            input: &mut input,
            out: &mut out,
        });
        (result, String::from_utf8(out).unwrap())
    }

    #[test]
    fn confirm_uses_the_default_and_rejects_nonsense() {
        assert!(run("\n", |t| t.confirm("Go?", true)).0.unwrap());
        assert!(!run("\n", |t| t.confirm("Go?", false)).0.unwrap());
        assert!(run("Yes\n", |t| t.confirm("Go?", false)).0.unwrap());
        assert!(!run("no\n", |t| t.confirm("Go?", true)).0.unwrap());
        let (result, shown) = run("maybe\nsure\nperhaps\n", |t| t.confirm("Go?", true));
        assert!(result.is_err() && shown.contains("Please answer y or n."));
        assert!(
            run("", |t| t.confirm("Go?", true))
                .0
                .unwrap_err()
                .to_string()
                .contains("input ended")
        );
    }

    #[test]
    fn choose_parses_lists_all_and_none() {
        let items = ["a".to_owned(), "b".to_owned(), "c".to_owned()];
        assert_eq!(
            run("\n", |t| t.choose("Pick", &items)).0.unwrap(),
            [0, 1, 2]
        );
        assert_eq!(
            run("none\n", |t| t.choose("Pick", &items)).0.unwrap(),
            Vec::<usize>::new()
        );
        assert_eq!(
            run("3, 1,3\n", |t| t.choose("Pick", &items)).0.unwrap(),
            [0, 2]
        );
        let (result, shown) = run("9\nx\n2\n", |t| t.choose("Pick", &items));
        assert_eq!(result.unwrap(), [1]);
        assert!(shown.contains("1) a") && shown.contains("numbers from 1 to 3"));
        assert!(run("9\n9\n9\n", |t| t.choose("Pick", &items)).0.is_err());
    }
}
