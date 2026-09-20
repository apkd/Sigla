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

## A few examples

| To find... | Query |
|---|---|
| A type | `type:GameManager` |
| Methods belonging to a type | `method:* in:GameManager` |
| References to a symbol | `uses:GameManager` |
| Calls made inside a method | `calls:* in:GameManager.Awake` |
| Implementations of an interface | `impl:IParser` |
| Text in part of the project | `text:"[Singleton]" path:Assets/Scripts` |

## Getting started

With a Rust toolchain installed:

```sh
cargo install --git https://github.com/apkd/sigla --locked
sigla serve
```

By default, Sigla may read anywhere your user account can access. Use `--root /path/to/projects` to restrict it to a directory, and repeat `--root` to allow several directories.

Connect your coding agent to `http://127.0.0.1:7331/mcp`. For Codex, add this to `~/.codex/config.toml`:

```toml
[mcp_servers.sigla]
url = "http://127.0.0.1:7331/mcp"
```

The agent gets one tool, `sigla.search`, with a project path and a query. Its tool description explains the query syntax, so you can simply ask the agent to find something.

## Authentication

To require a bearer token, put a random token of at least 24 characters in a private file:

```sh
sigla serve --token-file /path/to/token
```

Configure your MCP client to send `Authorization: Bearer YOUR_TOKEN`. For remote hosting, use an HTTPS reverse proxy; Sigla itself serves HTTP. Listening beyond localhost requires a token. Use `--allowed-host` for the hostname clients connect through, and `--allowed-origin` if browser access is needed.
