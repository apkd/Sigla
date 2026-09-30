```

       .|'''.|   ||          '||
       ||..  '  ...    ... .  ||   ....
        ''|||.   ||   || ||   ||  '' .||    Low-latency code search MCP server
      .     '||  ||    |''    ||  .|' ||    for Rust, C# and Unity projects
      |'....|'  .||.  '||||. .||. '|..'|'
                     .|....'
```

[![MIT License](https://img.shields.io/github/license/apkd/sigla?style=flat&label=License&logo=listmonk&labelColor=2C3439&color=fff)](https://github.com/apkd/sigla/blob/master/LICENSE)
[![CI workflow status](https://img.shields.io/github/actions/workflow/status/apkd/sigla/ci.yml?logo=githubactions&logoColor=white&label=Tests&labelColor=2C3439)](https://github.com/apkd/sigla/actions/workflows/ci.yml)
[![GitHub commit activity](https://img.shields.io/github/commit-activity/m/apkd/sigla?label=Commits&labelColor=2C3439&color=EBFF65&logo=git)](https://github.com/apkd/sigla/commits/master)
[![GitHub last commit](https://img.shields.io/github/last-commit/apkd/sigla?labelColor=2C3439&color=f97&logoColor=f96&logo=tinder&label=Committed)](https://github.com/apkd/sigla/commit/HEAD~1)

Sigla helps coding agents find their way around C# and Rust projects. It finds declarations, follows references, and returns the relevant code with a source location, ready to read.

- Find a type or method, see where it's used, or look for implementations of an interface.
- Get short, readable results with source excerpts and locations.
- Look up APIs from referenced .NET assemblies, even when their source isn't available.
- Share one server across several agents and projects, with indexing and refresh handled automatically.

Sigla runs on Linux and works with Cargo workspaces and Unity-generated C# projects. It reads your project files directly, so there's no Unity package to install or editor to keep running.

Local Unity discovery uses `~/Unity/Hub/Editor` by default. If the editor belongs to another account or lives elsewhere, pass `--unity-editors /path/to/Unity/Hub/Editor`. The Sigla service account must be able to read that directory and its editor files.

## A few examples

| To find... | Query |
|---|---|
| A type | `type:GameManager` |
| Methods belonging to a type | `method:* in:GameManager` |
| References to a symbol | `uses:GameManager` |
| Calls made inside a method | `calls:* in:GameManager.Awake` |
| Implementations of an interface | `impl:IParser` |
| Text in part of the project | `text:"[Singleton]" path:Assets/Scripts` |
| Source files by name | `file:*Parser*.cs` |
| Source paths under a directory | `file:src/**/*.rs` |

## Getting started

With a Rust toolchain, `pkg-config`, and libarchive development files installed (`libarchive-dev` on Debian/Ubuntu):

```sh
cargo install --git https://github.com/apkd/sigla --locked
sigla serve
```

By default, Sigla may read anywhere your user account can access. Use `--root /path/to/projects` to restrict it to a directory, and repeat `--root` to allow several directories.

Remote mode uses `--allow-repo 'https://github.com/owner/*'` for anonymous public access. Add `--allow-repo-private 'https://github.com/owner/private-repo'` to permit authenticated access to a repository. These rules also apply to Git package dependencies. Recoverable discovery and dependency problems go to server logs; query responses contain only results or request failures.

Selected Git LFS inputs are downloaded into a verified cache. Unavailable objects are skipped and retried during later preparation; an unreadable plugin does not discard the Unity project graph.

Local .NET discovery and restore run through `bubblewrap` with the checkout read-only. Standard SDK outputs, generated sources, and package caches go into Sigla's cache. Existing NuGet user configuration remains readable. Custom targets that require checkout writes produce a discovery diagnostic; Sigla falls back to readable sources. Remote .NET discovery uses its existing cached filesystem overlay.

Connect your coding agent to `http://127.0.0.1:7331/mcp`. For Codex, add this to `~/.codex/config.toml`:

```toml
[mcp_servers.sigla]
url = "http://127.0.0.1:7331/mcp"
```

The agent gets three tools. Each takes `project`, a local project path or, in remote mode, a repository URL with an optional `#branch`, `#tag`, or full `#commit` ID. Tags take precedence over branches with the same name, as in Git revision lookup. Use `#refs/heads/name` or `#refs/tags/name` to select one explicitly. Lightweight and annotated tags must point to commits. Branches and tags refresh periodically; commit IDs stay pinned. Fetching a historical commit requires the server to allow fetching that object. Abbreviated commit IDs and revision expressions such as `main~1` are not supported.

| Tool | Other arguments | Result |
|---|---|---|
| `search` | `query` | Symbols, references, text, or `file:GLOB` paths |
| `browse` | Optional `path` | An adaptive, indented directory tree |
| `view` | `path`, optional `mode` | File contents; `minified` by default, or `exact` |

## Authentication

To require a bearer token, put a random token of at least 24 characters in a private file:

```sh
sigla serve --token-file /path/to/token
```

Configure your MCP client to send `Authorization: Bearer YOUR_TOKEN`. For remote hosting, use an HTTPS reverse proxy; Sigla itself serves HTTP. Listening beyond localhost requires a token. Use `--allowed-host` for the hostname clients connect through, and `--allowed-origin` if browser access is needed.
