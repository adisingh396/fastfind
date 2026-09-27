![fastfind](docs/banner.png)

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![Platform](https://img.shields.io/badge/platform-Windows-0078D6.svg)
![Built with Rust](https://img.shields.io/badge/built%20with-Rust-orange.svg)
![MCP](https://img.shields.io/badge/MCP-server-3fb950.svg)
[![Install in Cursor](https://img.shields.io/badge/Install-Cursor-111.svg?logo=cursor)](cursor://anysphere.cursor-deeplink/mcp/install?name=fastfind&config=eyJjb21tYW5kIjogImZhc3RmaW5kIiwgImFyZ3MiOiBbIi0tbWNwIl19)
[![Install in VS Code](https://img.shields.io/badge/Install-VS_Code-007ACC.svg?logo=visualstudiocode)](vscode:mcp/install?%7B%22name%22%3A%22fastfind%22%2C%22command%22%3A%22fastfind%22%2C%22args%22%3A%5B%22--mcp%22%5D%7D)

**fastfind is an MCP server that gives AI coding agents instant, grounded access to the filesystem.** It exposes two tools — **`search`** (find files/folders by name, type, size, date) and **`grep`** (find text *inside* files) — both answered from a whole-machine index built off the NTFS Master File Table and kept in RAM. Your agent asks once and gets exact paths and line hits back, instead of guessing and burning tokens on `ls` / `grep` / `find` loops.

---

## The problem it removes

![blind agent probing vs one grounded call](docs/probe-vs-find.gif)

A terminal agent has no map of your disk, so to find anything it loops `ls`, `find`, `grep`, `cat` — parsing errors, re-reading files, chasing paths that don't exist. Published 2025–26 analyses measured the cost:

- **60–98% of a task's tokens** go to filesystem exploration rather than the actual work.
- A grep-only retrieval step burns **~110k input tokens** and **$2–5 per complex query**; indexed retrieval does the same job in **~8.5k tokens** — a **~14× reduction**.
- **65% file precision** — roughly **1 in 3 files an agent opens is wasted**.
- **50–200 navigation loops per session**; one recorded session spent **21,536 tokens just navigating** before its first edit.

fastfind replaces all of that with one grounded call.

---

## Tools

### `search` — files & folders by attribute

Declare intent with structured filters, no path-regex:

```jsonc
{ "ext": ["pdf"], "any_of": ["invoice", "receipt"], "min_size": 50000, "whole_word": true }
```

| Field | Description |
|---|---|
| `query` | Name match; substring by default, `* ?` wildcards, or `regex:true`. Omit to match all and filter below. |
| `ext` | Extensions to include, e.g. `["pdf","docx"]`. |
| `any_of` / `all_of` / `none_of` | OR / AND / NOT terms matched against the full path. |
| `whole_word` | Match those terms only as delimited tokens (`toc`, not `protocol`). |
| `min_size` / `max_size` / `modified_after` | Size (bytes) and date (`YYYY-MM-DD`) bounds. |
| `exclude` / `no_default_ignores` | Add ignores, or turn off the built-ins. |
| `max_results` | Cap results (default 100; `0` = all). |

Each hit returns `{path, name, dir, ext, size, mtime, is_folder}`, plus a `dirs:{path:count}` rollup for instant grouping. **Built-in ignores** (`node_modules`, `.git`, caches, TinyTeX, Steam, hashed cache files) keep results clean out of the box.

### `grep` — text inside files

The filename index selects only the files in scope (`root` / `ext` / path filters), then their contents are scanned in parallel — so "where is this used?" is fast without walking the whole disk.

```jsonc
{ "pattern": "import numpy", "root": "C:\\Users\\me\\project", "ext": ["py"] }
```

| Field | Description |
|---|---|
| `pattern` | Text to find; literal by default, or `regex:true`. |
| `root` | Directory to scope the scan to (recommended). |
| `ext` / `any_of` / `all_of` / `none_of` | Restrict which files are read. |
| `ignore_case` / `whole_word` | Matching options (case-insensitive by default). |
| `max_file_size` / `max_results` | Skip huge files; cap match lines. |

Each hit returns `{path, dir, name, line_no, line}`.

> Real run — every file importing NumPy under a Documents tree: **215 matches across 208 files, from 861 candidates**, all selected via the in-RAM index.

---

## Benchmarks

![the blind-agent tax](docs/benchmarks.png)

Measured on a low-end **Intel i3-10110U (2c/4t)** over a **~1.8M-file** drive:

- Whole-drive index build: **~21 s** via the `$MFT` (once; then resident in RAM).
- `search` query: **~30 ms** (substring) / **~180 ms** (glob & regex).
- For contrast, Windows' own recursive filename search (`where /r C:\`) **did not finish within 2 minutes** on the same drive.

---

## Install

**One-click:** click the **Install in Cursor** / **Install in VS Code** badges above, or download `fastfind.mcpb` from [Releases](https://github.com/adisingh396/fastfind/releases) and double-click it into Claude Desktop.

**One command** (registers into Claude Desktop, Claude Code, Cursor, opencode and compatible harnesses, with config backups):

```console
fastfind install --apply
```

**Manual** — Claude Desktop / Claude Code / Cursor (`mcpServers`):

```jsonc
{ "mcpServers": { "fastfind": { "command": "C:\\path\\to\\fastfind.exe", "args": ["--mcp"] } } }
```

opencode (`opencode.json`):

```jsonc
{ "mcp": { "fastfind": { "type": "local", "command": ["C:\\path\\to\\fastfind.exe", "--mcp"], "enabled": true } } }
```

Also listed via [`server.json`](server.json) (official MCP registry) and [`smithery.yaml`](smithery.yaml). For the most complete index, run from an elevated shell so it can use the `$MFT` backend.

---

## How it works

```mermaid
flowchart LR
  MFT["NTFS $MFT<br/>FSCTL_ENUM_USN_DATA<br/><i>(elevated)</i>"] --> IDX
  WALK["Parallel walk<br/>FindFirstFileEx + rayon"] --> IDX
  IDX["In-RAM index<br/>path · size · mtime"] --> S["search<br/>attribute filters"]
  IDX --> G["grep<br/>index-scoped content scan"]
  S --> MCP["MCP server (stdio)"]
  G --> MCP
  MCP --> AGENTS["Claude · Cursor · opencode · VS Code"]
```

- **`$MFT` backend** reads every file record via `FSCTL_ENUM_USN_DATA` and rebuilds paths from parent references — one sequential read instead of millions of directory syscalls (needs elevation).
- **Walk backend** is a `rayon` traversal using `FindFirstFileExW` + `FIND_FIRST_EX_LARGE_FETCH`, capturing size/mtime for free — no admin, any filesystem.
- The index is built once and kept resident; `grep` reuses it to read only in-scope files.

---

## Also a CLI

```console
fastfind --ext pdf --any invoice --min-size 50000          # search
fastfind --grep "import numpy" --root C:/Users/me --ext py # grep contents
fastfind --any toc --whole-word                            # token match, not "protocol"
fastfind "*.log" --json                                    # machine-readable
```

## Build from source

Needs a [Rust toolchain](https://rustup.rs) (Windows):

```console
git clone https://github.com/adisingh396/fastfind
cd fastfind
cargo build --release   # -> target\release\fastfind.exe
```

Package the Claude Desktop extension with `npx @anthropic-ai/mcpb pack` (uses `manifest.json`).

## Roadmap

- [ ] Live index updates via the `$USN` change journal
- [ ] Prebuilt release binaries + signed `.mcpb`
- [ ] macOS / Linux backends
- [ ] Ranked `grep` output with surrounding context lines

## Contributing

Issues and PRs welcome. Run `cargo fmt` / `cargo clippy` and note what you verified.

## License

[MIT](LICENSE) © Aditya Singh

<sub>Cost figures above are ranges reported by public 2025–26 analyses of coding-agent filesystem use (ContextBench, Semble, Hypergrep, and agent session logs). Latency figures are from this project's own runs.</sub>
