#!/usr/bin/env python3
"""Time gitrs-mcp tool calls against the equivalent git commands on one repository.

Usage: scripts/bench.py <gitrs-mcp binary> <repo> [runs]

The server is started once; each case is called `runs` times (default 5) over stdio and timed from
request to response, so the times include JSON encoding but not process start-up. The first call of
a case shows a cold object cache, the best of the rest a warm one. Each git command runs `runs` times
as a fresh process, as an agent shelling out to git would. Cases whose file or author doesn't exist in
the repository are skipped, so it works on any repository, though git.git is the reference.
"""
import json
import subprocess
import sys
import time

BLAME_FILE = "diff.c"
HISTORY_FILE = "Makefile"
AUTHOR = "Junio C Hamano"

CASES = [
    # name, tool, arguments, git command
    ("log, 500 commits", "log", {"limit": 500}, ["log", "-n", "500", "--format=%H %s"]),
    ("log, 20 after skipping 20000", "log", {"limit": 20, "skip": 20000},
     ["log", "-n", "20", "--skip=20000", "--format=%H %s"]),
    ("search_commits by author, 50", "search_commits", {"author": AUTHOR, "limit": 50},
     ["log", "-n", "50", f"--author={AUTHOR}", "--format=%H %s"]),
    (f"file_history {HISTORY_FILE}, 100", "file_history", {"path": HISTORY_FILE, "limit": 100},
     ["log", "-n", "100", "--format=%H %s", "--", HISTORY_FILE]),
    (f"blame {BLAME_FILE}", "blame", {"path": BLAME_FILE, "limit": 2000},
     ["blame", "--porcelain", "--", BLAME_FILE]),
    ("grep 'static int', 100 matches", "grep", {"pattern": "static int", "limit": 100},
     ["grep", "-n", "--column", "-I", "-m", "100", "static int"]),
]


class Server:
    def __init__(self, binary, repo):
        self.proc = subprocess.Popen(
            [binary, "--repo", repo], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, text=True, bufsize=1)
        self.next_id = 1
        self.request("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                    "clientInfo": {"name": "bench", "version": "0"}})
        self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def send(self, message):
        self.proc.stdin.write(json.dumps(message) + "\n")
        self.proc.stdin.flush()

    def request(self, method, params):
        request_id = self.next_id
        self.next_id += 1
        self.send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        while True:
            message = json.loads(self.proc.stdout.readline())
            if message.get("id") == request_id:
                return message

    def call(self, tool, arguments):
        started = time.perf_counter()
        response = self.request("tools/call", {"name": tool, "arguments": arguments})
        elapsed = time.perf_counter() - started
        result = response.get("result", {})
        if "error" in response or result.get("isError"):
            raise RuntimeError(json.dumps(response)[:300])
        return elapsed

    def close(self):
        self.proc.stdin.close()
        self.proc.wait()


def git_time(repo, command):
    started = time.perf_counter()
    subprocess.run(["git", "-C", repo, *command], check=True, stdout=subprocess.DEVNULL)
    return time.perf_counter() - started


def applies(repo, tool, arguments):
    """Whether the case's file or author exists in this repository."""
    if "path" in arguments:
        found = subprocess.run(["git", "-C", repo, "cat-file", "-e", f"HEAD:{arguments['path']}"],
                               capture_output=True)
        return found.returncode == 0
    if tool == "log" and "skip" in arguments:
        count = subprocess.run(["git", "-C", repo, "rev-list", "--count", "HEAD"],
                               capture_output=True, text=True, check=True).stdout
        return int(count) > arguments["skip"]
    return True


def ms(seconds):
    return f"{seconds * 1000:,.0f} ms"


def main():
    binary, repo = sys.argv[1], sys.argv[2]
    runs = int(sys.argv[3]) if len(sys.argv) > 3 else 5
    commits = subprocess.run(["git", "-C", repo, "rev-list", "--count", "HEAD"],
                             capture_output=True, text=True, check=True).stdout.strip()
    git_version = subprocess.run(["git", "--version"], capture_output=True, text=True).stdout.strip()
    print(f"{repo}: {commits} commits; {git_version}; {runs} runs each\n")
    print("| Case | gitrs-mcp first call | gitrs-mcp best | git best |")
    print("|---|---:|---:|---:|")
    server = Server(binary, repo)
    try:
        for name, tool, arguments, command in CASES:
            if not applies(repo, tool, arguments):
                print(f"| {name} | skipped | | |")
                continue
            ours = [server.call(tool, arguments) for _ in range(runs)]
            theirs = [git_time(repo, command) for _ in range(runs)]
            warm = min(ours[1:]) if runs > 1 else ours[0]
            print(f"| {name} | {ms(ours[0])} | {ms(warm)} | {ms(min(theirs))} |")
            sys.stdout.flush()
    finally:
        server.close()


if __name__ == "__main__":
    main()
