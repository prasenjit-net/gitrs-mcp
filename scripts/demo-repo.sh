#!/usr/bin/env bash
# Build the demo workspace the examples in docs/tools.md come from, under <dir>:
#   <dir>/demo         the demo repository (serve this one)
#   <dir>/demo-review  a linked worktree of demo, on `feature`, locked, in the middle of merging `polish`
#   <dir>/assets       a small repository that demo uses as the submodule `vendor/assets`
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

git init -q -b main assets
printf 'body { font-family: serif; }\n' > assets/style.css
git -C assets add . && at 1790841600 && git -C assets commit -q -m "Add stylesheet"

git init -q -b main demo
cd demo
# The identity write tools record as committer and tagger.
git config user.name "Ada Lovelace"
git config user.email ada@example.com
mkdir src
printf '# greet\n\nA tiny greeting library.\n' > README.md
printf 'pub fn greet(name: &str) -> String {\n    format!("Hello, {name}!")\n}\n' > src/lib.rs
printf 'target/\n*.log\n' > .gitignore
printf '*.rs diff=rust\n*.png binary\n' > .gitattributes
git -c protocol.file.allow=always submodule add -q ../assets vendor/assets
git add . && at 1790845200 && git commit -q -m "Initial commit"
printf '\npub fn shout(name: &str) -> String {\n    greet(name).to_uppercase()\n}\n' >> src/lib.rs
git add . && at 1790937000 && git commit -q -m "Add shout" -m "Uppercase variant of greet."
at 1790937600 && git tag -a v0.1.0 -m "First release" -m "greet and shout."
git checkout -q -b feature
printf 'pub fn farewell(name: &str) -> String {\n    format!("Goodbye, {name}.")\n}\n' > src/farewell.rs
git add . && at 1791036000 && git commit -q -m "Add farewell"
git checkout -q -b polish v0.1.0
printf 'pub fn farewell(name: &str) -> String {\n    format!("See you, {name}!")\n}\n' > src/farewell.rs
git add . && at 1791050000 && git commit -q -m "Add a friendlier farewell"
git checkout -q main
printf '\n## Usage\n\n```rust\nlet hi = greet::greet("Ada");\n```\n' >> README.md
git add . && at 1791101700 && git commit -q -m "Document usage" -m "Fixes #12"
at 1791102000 && git notes add -m "Reviewed-by: Grace Hopper <grace@example.com>" HEAD
printf '# greet\n\nGreetings, politely or loudly.\n' > README.md.tmp && mv README.md.tmp README.md
at 1791105000 && git stash push -q -m "try a new intro"
# A remote whose URL embeds a password, to show config_get redacting it. Nothing is ever fetched.
git remote add origin https://ada:hunter2@example.com/greet.git
git worktree add -q ../demo-review feature
git worktree lock --reason "reviewing feature" ../demo-review
# Both branches add src/farewell.rs, so this merge stops with a conflict for `conflicts` to show.
at 1791106000 && git -C ../demo-review merge -q polish > /dev/null || true
# Working tree: unstaged edit, staged new file, untracked file.
sed 's/Hello, {name}!/Hello, {name}!!/' src/lib.rs > src/lib.rs.new && mv src/lib.rs.new src/lib.rs
printf 'fn main() {\n    println!("{}", greet::greet("world"));\n}\n' > src/main.rs
git add src/main.rs
printf 'scratch notes\n' > notes.txt
