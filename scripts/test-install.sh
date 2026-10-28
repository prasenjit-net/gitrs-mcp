#!/bin/sh
# Tests install.sh against a local fixture release (file:// URLs), so no network is used.
#
#   scripts/test-install.sh target/debug/gitrs-mcp
#
# The fixture holds the given binary in the same archive layout the release workflow publishes.
set -eu

BIN=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
HERE=$(cd "$(dirname "$0")/.." && pwd)
INSTALL="$HERE/install.sh"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

pass=0
check() { # check <description> <command...>
    desc=$1
    shift
    if "$@"; then
        pass=$((pass + 1))
    else
        printf 'FAIL: %s\n' "$desc" >&2
        exit 1
    fi
}
contains() { grep -q -- "$1" "$2"; }
lacks() { ! grep -q -- "$1" "$2"; }
absent() { [ ! -e "$1" ]; }

TARGET=$(sh "$INSTALL" --print-target)
ASSET="gitrs-mcp-$TARGET.tar.gz"

# A release directory as GitHub serves it: latest/download/ and download/<tag>/.
RELEASE="$WORK/releases"
mkdir -p "$WORK/pack/gitrs-mcp-$TARGET" "$RELEASE/latest/download" "$RELEASE/download/v9.9.9"
cp "$BIN" "$WORK/pack/gitrs-mcp-$TARGET/gitrs-mcp"
tar -czf "$RELEASE/latest/download/$ASSET" -C "$WORK/pack" "gitrs-mcp-$TARGET"
checksum() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1"; else shasum -a 256 "$1"; fi
}
(cd "$RELEASE/latest/download" && checksum "$ASSET" >"$ASSET.sha256")
cp "$RELEASE/latest/download/"* "$RELEASE/download/v9.9.9/"
export GITRS_RELEASE_BASE_URL="file://$RELEASE"

run() { # run <log> <install.sh args...>: runs the installer with no terminal input
    log=$1
    shift
    set +e
    sh "$INSTALL" "$@" >"$log" 2>&1 </dev/null
    status=$?
    set -e
    return $status
}
fails() { ! run "$@"; }

# A successful install puts a working binary in the directory and warns about PATH.
check "installs" run "$WORK/a.log" --dir "$WORK/bin" --no-setup
check "binary runs" "$WORK/bin/gitrs-mcp" --version
check "reports the install" contains "Installed gitrs-mcp" "$WORK/a.log"
check "warns when the directory is not on PATH" contains "is not on your PATH" "$WORK/a.log"
check "leaves no temporary files" [ "$(ls -A "$WORK/bin")" = "gitrs-mcp" ]

# On PATH there is no warning, and installing again over a binary in use works.
PATH="$WORK/bin:$PATH" run "$WORK/b.log" --dir "$WORK/bin" --no-setup
check "no warning when on PATH" lacks "is not on your PATH" "$WORK/b.log"
check "reinstall keeps one binary" [ "$(ls -A "$WORK/bin")" = "gitrs-mcp" ]

# The version flag picks download/<tag>, with or without the v.
check "explicit version" run "$WORK/c.log" --dir "$WORK/v1" --version v9.9.9 --no-setup
check "version without v" run "$WORK/c2.log" --dir "$WORK/v2" --version 9.9.9 --no-setup
check "unknown version fails" fails "$WORK/c3.log" --dir "$WORK/v3" --version 1.2.3 --no-setup
check "and installs nothing" absent "$WORK/v3/gitrs-mcp"

# A checksum that does not match stops the install before anything is written.
printf '%s  %s\n' "0000000000000000000000000000000000000000000000000000000000000000" "$ASSET" \
    >"$RELEASE/latest/download/$ASSET.sha256"
check "checksum mismatch fails" fails "$WORK/d.log" --dir "$WORK/bad" --no-setup
check "says why" contains "checksum mismatch" "$WORK/d.log"
check "installs nothing" absent "$WORK/bad/gitrs-mcp"
(cd "$RELEASE/download/v9.9.9" && cp "$ASSET.sha256" "$RELEASE/latest/download/$ASSET.sha256")

# Dry run writes and downloads nothing.
check "dry run" run "$WORK/e.log" --dir "$WORK/dry" --dry-run
check "dry run describes" contains "Dry run: would install" "$WORK/e.log"
check "dry run writes nothing" absent "$WORK/dry"

# An unsupported CPU stops with a pointer to building from source.
mkdir "$WORK/fake"
REAL_UNAME=$(command -v uname)
# The single quotes are deliberate: $1 and $@ belong to the generated script.
# shellcheck disable=SC2016
printf '#!/bin/sh\ncase "$1" in -m) echo mips;; *) %s "$@";; esac\n' "$REAL_UNAME" >"$WORK/fake/uname"
chmod +x "$WORK/fake/uname"
check "unsupported cpu fails" env PATH="$WORK/fake:$PATH" sh -c "! sh '$INSTALL' --dir '$WORK/mips' >'$WORK/f.log' 2>&1"
check "unsupported cpu is explained" contains "cargo install gitrs-mcp" "$WORK/f.log"

# Without a terminal, setup is not run and the user is told how.
if ! ( : </dev/tty ) 2>/dev/null; then
    check "no terminal, no setup" run "$WORK/g.log" --dir "$WORK/notty"
    check "tells how to run setup" contains "gitrs-mcp setup" "$WORK/g.log"
fi

# Uninstall removes the binary; bad options fail.
check "uninstall dry run" run "$WORK/h.log" --dir "$WORK/bin" --uninstall --dry-run
check "dry run keeps the binary" [ -f "$WORK/bin/gitrs-mcp" ]
if ! ( : </dev/tty ) 2>/dev/null; then
    check "uninstall" run "$WORK/i.log" --dir "$WORK/bin" --uninstall
    check "removes the binary" absent "$WORK/bin/gitrs-mcp"
    check "uninstall of nothing fails" fails "$WORK/j.log" --dir "$WORK/bin" --uninstall
fi
check "unknown option fails" fails "$WORK/k.log" --bogus
check "help works" run "$WORK/l.log" --help
check "help shows usage" contains "Usage: install.sh" "$WORK/l.log"

printf 'install.sh: %s checks passed\n' "$pass"
