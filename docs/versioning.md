# Versioning

gitrs-mcp follows [semantic versioning](https://semver.org). What it versions is its **MCP surface**, not a Rust API: the names, input and output schemas and annotations of its tools, the names and arguments of its prompts, the resource URI templates, and the command-line flags. Agents and clients are written against these, so they're a public API.

## What counts as breaking

A change is **breaking** when a client written against the previous version could stop working:

- removing or renaming a tool, prompt, prompt argument, resource template or flag;
- removing a property from a tool's arguments or result, or changing its type;
- making an optional argument required, or adding a required argument;
- making a read-only tool write;
- changing what a default does in a way that alters results, for example turning a dry run off by default.

A change is **compatible** when existing clients keep working:

- adding a tool, prompt, template or flag;
- adding an optional argument, or a result field (clients must ignore fields they don't know);
- refusing input that was never valid, or improving an error message.

Breaking changes need a new major version. Before 1.0, they bump the minor version (0.1 to 0.2), as Cargo does. A breaking change is called out in the changelog with how to migrate.

## How it's enforced

`api/schema.json` records the published API: every tool's schemas and annotations, every prompt's arguments and every resource template. It's generated from the server itself. The test `server::api::the_published_api_only_grows` compares the running server with the snapshot:

- **A breaking change** fails the test, with a list of what broke.
- **A compatible change** also fails the test, until the snapshot is updated with `make api`. Every API change, even additive, therefore shows up in review as a diff of `api/schema.json`.

A deliberate breaking change for a new major version updates the snapshot with `make api` too, and comes with a changelog entry.

## Protocol versions

gitrs-mcp is built on rmcp, which negotiates the MCP protocol version with each client. Clients using the `initialize` handshake (2024-11-05 through 2025-11-25) and clients using protocol 2026-07-28 are both served. Features deprecated in 2026-07-28 but still used by current clients, such as roots and `resources/subscribe`, are kept for as long as clients rely on them.

## Releases

The version is set in `Cargo.toml`, and changes are listed in [CHANGELOG.md](../CHANGELOG.md). Tagging `v<version>` builds the release binaries; see [development](development.md#releasing).
