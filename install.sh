#!/bin/sh
# gitrs-mcp installer for macOS and Linux.
#
#   curl -fsSL https://raw.githubusercontent.com/prasenjit-net/gitrs-mcp/master/install.sh | sh
#   curl -fsSL .../install.sh | sh -s -- --version v0.1.0 --no-setup
#
# What it does, in order:
#   1. works out your platform and downloads that release archive from GitHub over HTTPS;
#   2. checks the archive against the published SHA-256 checksum and stops if it differs;
#   3. puts `gitrs-mcp` in ~/.local/bin (or --dir / $GITRS_INSTALL_DIR). It never uses sudo
#      and never edits your shell profile;
#   4. runs `gitrs-mcp setup` so you can register it with your AI agents, when there is a terminal
#      to ask on.
#
# Options:
#   --version <tag>   install this release (for example v0.1.0) instead of the latest
#   --dir <path>      install into <path> (default: $GITRS_INSTALL_DIR or ~/.local/bin)
#   --no-setup        only install; don't run `gitrs-mcp setup`
#   --dry-run         show what would happen; download and write nothing
#   --uninstall       remove the installed binary (offers to unregister it first)
#   --print-target    print the release platform this machine would download, and exit
#   -h, --help        show this help
#
# Everything is inside functions and `main` runs on the last line, so a download that is cut short
# can't run half a script.

set -eu

REPO="prasenjit-net/gitrs-mcp"
BINARY="gitrs-mcp"
# Where releases are fetched from. Tests point this at a local directory with a file:// URL.
BASE_URL="${GITRS_RELEASE_BASE_URL:-https://github.com/$REPO/releases}"

say() { printf '%s\n' "$*"; }
fail() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

usage() {
    cat <<'USAGE'
gitrs-mcp installer for macOS and Linux

Usage: install.sh [options]

  --version <tag>   install this release (for example v0.1.0) instead of the latest
  --dir <path>      install into <path> (default: $GITRS_INSTALL_DIR or ~/.local/bin)
  --no-setup        only install; don't run `gitrs-mcp setup`
  --dry-run         show what would happen; download and write nothing
  --uninstall       remove the installed binary (offers to unregister it first)
  --print-target    print the release platform this machine would download, and exit
  -h, --help        show this help

More: https://github.com/prasenjit-net/gitrs-mcp
USAGE
}

# The release target triple for this machine, or a failure that says what to do instead.
detect_target() {
    os=$(uname -s)
    arch=$(uname -m)
    case "$os" in
        Darwin) suffix="apple-darwin" ;;
        Linux) suffix="unknown-linux-gnu" ;;
        *) fail "unsupported operating system '$os'. Windows has its own installer (see the README); otherwise build with: cargo install gitrs-mcp" ;;
    esac
    case "$arch" in
        arm64 | aarch64) cpu="aarch64" ;;
        x86_64 | amd64) cpu="x86_64" ;;
        *) fail "unsupported CPU '$arch'. Build from source with: cargo install gitrs-mcp" ;;
    esac
    # A shell running under Rosetta reports x86_64 on an Apple Silicon Mac; use the native build.
    if [ "$os" = "Darwin" ] && [ "$cpu" = "x86_64" ] \
        && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = "1" ]; then
        cpu="aarch64"
    fi
    # The Linux builds link glibc.
    if [ "$os" = "Linux" ] && (ldd --version 2>&1 || true) | grep -qi musl; then
        fail "this system uses musl libc (Alpine, for example); the prebuilt Linux binaries need glibc. Build from source with: cargo install gitrs-mcp"
    fi
    printf '%s-%s' "$cpu" "$suffix"
}

# fetch <url> <output file>
fetch() {
    case "$1" in
        file://*) proto='=file' ;;
        *) proto='=https' ;;
    esac
    if have curl; then
        curl --proto "$proto" --tlsv1.2 --fail --silent --show-error --location --output "$2" "$1" \
            || fail "could not download $1"
    elif have wget; then
        wget --quiet --output-document="$2" "$1" || fail "could not download $1"
    else
        fail "neither curl nor wget is installed"
    fi
}

# The SHA-256 of a file, as lowercase hex.
sha256_of() {
    if have sha256sum; then
        sha256sum "$1" | cut -d ' ' -f 1
    elif have shasum; then
        shasum -a 256 "$1" | cut -d ' ' -f 1
    else
        fail "no sha256sum or shasum to verify the download with"
    fi
}

# verify <archive> <checksum file>: stop unless the archive matches the published checksum.
verify() {
    want=$(cut -d ' ' -f 1 "$2" | tr 'A-F' 'a-f')
    got=$(sha256_of "$1")
    [ -n "$want" ] || fail "the checksum file is empty"
    [ "$want" = "$got" ] || fail "checksum mismatch for $(basename "$1"): expected $want, got $got. Nothing was installed."
}

# True when there is a terminal to ask questions on (stdin is the script itself under `curl | sh`).
have_terminal() {
    [ -t 0 ] || ( : </dev/tty ) 2>/dev/null
}

# Run `<binary> setup [args]` with its questions on the terminal.
run_with_terminal() {
    if [ -t 0 ]; then
        "$@"
    else
        "$@" </dev/tty
    fi
}

on_path() {
    case ":$PATH:" in
        *":$1:"*) return 0 ;;
        *) return 1 ;;
    esac
}

install() {
    version="$1"
    dir="$2"
    no_setup="$3"
    dry_run="$4"

    target=$(detect_target)
    asset="$BINARY-$target.tar.gz"
    if [ -z "$version" ]; then
        url="$BASE_URL/latest/download"
    else
        url="$BASE_URL/download/$version"
    fi

    say "Installing $BINARY (${version:-latest}) for $target into $dir"
    if [ "$dry_run" = 1 ]; then
        say "Dry run: would download $url/$asset"
        say "Dry run: would verify it against $url/$asset.sha256"
        say "Dry run: would install $dir/$BINARY"
        [ "$no_setup" = 1 ] || say "Dry run: would then run '$BINARY setup'"
        return 0
    fi

    have tar || fail "tar is required"
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT INT TERM

    fetch "$url/$asset" "$tmp/$asset"
    fetch "$url/$asset.sha256" "$tmp/$asset.sha256"
    verify "$tmp/$asset" "$tmp/$asset.sha256"

    mkdir "$tmp/unpacked"
    tar -xzf "$tmp/$asset" -C "$tmp/unpacked" || fail "could not unpack $asset"
    binary=$(find "$tmp/unpacked" -type f -name "$BINARY" | head -n 1)
    [ -n "$binary" ] || fail "$asset does not contain $BINARY"

    mkdir -p "$dir" || fail "cannot create $dir. Choose another directory with --dir"
    # Copy beside the destination, then rename over it: replacing a running binary is safe.
    if ! { cp "$binary" "$dir/.$BINARY.new" && chmod 755 "$dir/.$BINARY.new" \
        && mv -f "$dir/.$BINARY.new" "$dir/$BINARY"; }; then
        fail "cannot write to $dir. Choose another directory with --dir"
    fi
    say "Installed $("$dir/$BINARY" --version) to $dir/$BINARY"

    if ! on_path "$dir"; then
        say ""
        say "$dir is not on your PATH. Add it by putting this in your shell profile:"
        say "    export PATH=\"$dir:\$PATH\""
    fi

    if [ "$no_setup" = 1 ]; then
        return 0
    fi
    say ""
    if have_terminal; then
        run_with_terminal "$dir/$BINARY" setup
    else
        say "To register it with your AI agents, run: $dir/$BINARY setup"
    fi
}

uninstall() {
    dir="$1"
    dry_run="$2"
    [ -f "$dir/$BINARY" ] || fail "$dir/$BINARY is not installed"
    if [ "$dry_run" = 1 ]; then
        say "Dry run: would offer '$dir/$BINARY setup --remove', then delete $dir/$BINARY"
        return 0
    fi
    # Registrations would be left pointing at nothing, so offer to remove them while the binary exists.
    if have_terminal; then
        run_with_terminal "$dir/$BINARY" setup --remove
    else
        say "Registrations with AI agents are not removed. First run: $dir/$BINARY setup --remove --all --yes"
    fi
    rm -f "$dir/$BINARY"
    say "Removed $dir/$BINARY"
}

main() {
    version=""
    dir="${GITRS_INSTALL_DIR:-$HOME/.local/bin}"
    no_setup=0
    dry_run=0
    action=install
    while [ $# -gt 0 ]; do
        case "$1" in
            --version)
                [ $# -ge 2 ] || fail "--version needs a value, for example v0.1.0"
                case "$2" in
                    v*) version="$2" ;;
                    *) version="v$2" ;;
                esac
                shift 2
                ;;
            --dir)
                [ $# -ge 2 ] || fail "--dir needs a directory"
                dir="$2"
                shift 2
                ;;
            --no-setup) no_setup=1; shift ;;
            --dry-run) dry_run=1; shift ;;
            --uninstall) action=uninstall; shift ;;
            --print-target) detect_target; printf '\n'; return 0 ;;
            -h | --help) usage; return 0 ;;
            *) fail "unknown option '$1' (try --help)" ;;
        esac
    done
    [ -n "${HOME:-}" ] || [ -n "${GITRS_INSTALL_DIR:-}" ] || fail "HOME is not set; pass --dir"
    case "$action" in
        install) install "$version" "$dir" "$no_setup" "$dry_run" ;;
        uninstall) uninstall "$dir" "$dry_run" ;;
    esac
}

main "$@"
