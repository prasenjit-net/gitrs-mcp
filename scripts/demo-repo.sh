#!/usr/bin/env bash
# Build the demo repository the examples in docs/tools.md come from, at <dir>.
# Identities and dates are fixed, so every run produces the same SHAs.
# Usage: scripts/demo-repo.sh <dir>
set -euo pipefail
dir=${1:?target dir}
if [ -n "$(ls -A "$dir" 2>/dev/null)" ]; then
  echo "$dir exists and is not empty; pass a new directory" >&2
  exit 1
fi
mkdir -p "$dir" && cd "$dir"
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
export GIT_AUTHOR_NAME="Ada Lovelace" GIT_AUTHOR_EMAIL=ada@example.com
export GIT_COMMITTER_NAME="Ada Lovelace" GIT_COMMITTER_EMAIL=ada@example.com
at() { export GIT_AUTHOR_DATE="@$1 +0000" GIT_COMMITTER_DATE="@$1 +0000"; }
git init -q -b main
mkdir src
printf '# greet\n\nA tiny greeting library.\n' > README.md
printf 'pub fn greet(name: &str) -> String {\n    format!("Hello, {name}!")\n}\n' > src/lib.rs
printf 'target/\n*.log\n' > .gitignore
printf '*.rs diff=rust\n*.png binary\n' > .gitattributes
git add . && at 1790850600 && git commit -q -m "Initial commit"
printf '\npub fn shout(name: &str) -> String {\n    greet(name).to_uppercase()\n}\n' >> src/lib.rs
git add . && at 1790937000 && git commit -q -m "Add shout" -m "Uppercase variant of greet."
at 1790937600 && git tag -a v0.1.0 -m "First release" -m "greet and shout."
git checkout -q -b feature
printf '\npub fn farewell(name: &str) -> String {\n    format!("Goodbye, {name}!")\n}\n' >> src/lib.rs
git add . && at 1791015000 && git commit -q -m "Add farewell"
git checkout -q main
printf '\n## Usage\n\n```rust\nlet hi = greet::greet("Ada");\n```\n' >> README.md
git add . && at 1791101700 && git commit -q -m "Document usage"
at 1791102000 && git notes add -m "Reviewed-by: Grace Hopper <grace@example.com>" HEAD
printf '# greet\n\nGreetings, politely or loudly.\n' > README.md.tmp && mv README.md.tmp README.md
at 1791105000 && git stash push -q -m "try a new intro"
# Working tree: unstaged edit, staged new file, untracked file.
printf '\n// TODO: localise\n' >> src/lib.rs
printf 'fn main() {\n    println!("{}", greet::greet("world"));\n}\n' > src/main.rs
git add src/main.rs
printf 'scratch notes\n' > notes.txt
