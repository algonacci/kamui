# kamui

Provider-agnostic, repository-aware LLM chat CLI written in Rust.

The first provider implementation uses the OpenAI Chat Completions API. The core request and
response types are independent from that API so other providers can be added without changing
the chat interface.

## Configuration

For development, create a `.env` file in the repository based on `.env.example`. An installed
binary can be run from any directory by placing the same file in the OS configuration directory:

| Platform | Configuration file |
| --- | --- |
| Windows | `%APPDATA%\\kamui\\.env` |
| Linux | `~/.config/kamui/.env` |
| macOS | `~/Library/Application Support/kamui/.env` |

Process environment variables take precedence, followed by a local `.env`, then the global file.

```env
OPENAI_API_KEY=sk-xxxxxxxx
OPENAI_BASE_URL=https://api.openai.com/v1
OPENAI_MODEL=gpt-5.5
KAMUI_CONTEXT_WINDOW=128000
```

Any service implementing the OpenAI Chat Completions API can be used by changing the base URL,
model, and API key. Chat responses use the API's SSE streaming mode and are rendered as deltas
arrive.

## Install

Windows PowerShell:

```powershell
irm https://raw.githubusercontent.com/algonacci/kamui/main/install.ps1 | iex
```

Linux and macOS:

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/algonacci/kamui/main/install.sh | sh
```

Then open a new terminal and run:

```sh
kamui
```

Check the installed version with `kamui --version` and list command-line options with
`kamui --help`.

For development, install the current checkout into Cargo's binary directory:

```sh
cargo install --path .
```

This compiles once and installs `kamui` in `~/.cargo/bin`. It does not compile again each time the
command runs. Use `cargo install --path . --force` after local source changes.

## Development

```sh
cargo run
```

Kamui stores sessions, messages, and token usage in a local SQLite database. Each launch starts a
new chat, but it is only saved as a session after the first successful response. Use `/sessions`
and `/resume <id>` to continue an earlier conversation. Use `/help` inside the chat to list all
session commands.

After the first response, the active provider generates a short session title. `/sessions` shows
that title together with the last-updated timestamp, message count, and total token usage. Tokens
used to generate the title are included in session usage analytics.

Resume a saved session directly when starting Kamui:

```sh
kamui -r <session-id>
```

### Session commands

| Command | Description |
| --- | --- |
| `/new` | Start a new session |
| `/sessions` | List saved sessions |
| `/resume <id>` | Resume a session |
| `/rename <id> <title>` | Rename a session |
| `/search <text>` | Search saved messages across all sessions |
| `/delete <id>` | Delete a session |
| `/stats` | Show current session usage |
| `/help` | List available commands |
| `/exit` | Save and quit |

`/rename` accepts a session ID prefix followed by the new title; if the renamed session is the
active one, its in-memory title updates immediately. `/search` matches message text
case-insensitively (literal `%` and `_` are not treated as wildcards) and prints each hit as its
session ID, timestamp, title, and a snippet centered on the match.

After each streamed response, Kamui reports time-to-first-token (`TTFT`) and total response time
(`Time`) alongside token usage and the finish reason.

## Repository context

Kamui uses the directory where it was launched as the project root. If that directory contains
`KAMUI.md` or `AGENTS.md`, Kamui sends the first file found in that order as project instructions
with every chat request.

Reference a UTF-8 text file relative to the project root with `@path`:

```text
> Explain the error handling in @src/main.rs
```

Referenced files are attached only to that request and are not copied into session history. Each
file is limited to 64 KiB and all attached files together are limited to 128 KiB. Absolute paths,
directories, binary files, and paths or symlinks outside the project are rejected.

Use `@diff` for unstaged tracked changes or `@staged` for changes in the Git index:

```text
> Review @diff for bugs
> Write a commit summary for @staged
```

Git context is read-only and can be combined with file references. Untracked files are not included
in `@diff`; attach them explicitly with `@path`.

## Tools

Kamui offers the model three tools: `list_directory` (discover what is in a folder), `read_file`
(read a file), and `run_command` (run a shell command). When you ask about code, the model can
explore, read, build, and test on its own instead of requiring you to attach files with `@path`:

```text
> What does the agent loop in src/chat.rs do?
> Run the tests and tell me if anything fails.
```

If the model calls a tool, Kamui prints a short trace of each call, runs it, feeds the result back,
and continues streaming until a final answer. The read tools reuse the same path safety as `@file`
(project-relative only, no escaping the root, 64 KiB per file) and the loop is bounded so it cannot
run away.

`run_command` never runs on its own. Kamui shows you the exact command and waits for you to approve
it (`y`/`yes`); anything else declines and tells the model so. Commands run in the project directory
with input disabled, a 30-second timeout, and a capped amount of captured output, and the model sees
the exit code alongside stdout and stderr. File editing is not implemented yet.

The whole turn is saved to session history, including the tool calls and their results, so a resumed
session replays the tool interactions the model relied on.

## RTK integration direction

[RTK](https://github.com/rtk-ai/rtk) will be an optional execution backend when Kamui gains terminal
tools. Supported commands can run through RTK so compact test, build, search, Git, and container
output reaches the model. Kamui will still own command permissions, timeouts, cancellation, output
limits, and audit records.

RTK is not currently used by chat or `@diff`, and users do not need to install it yet. Keeping raw
diff context avoids dropping details needed for code review. The future integration will detect the
external `rtk` binary and fall back to direct execution when it is unavailable or a command is not
supported.

## Data storage

The database uses the standard local application data directory for Windows, macOS, and Linux.
Set `KAMUI_DATA_DIR` to override it, particularly for servers and containers:

```env
KAMUI_DATA_DIR=/var/lib/kamui
```

SQLite is bundled into the Kamui binary, so users do not need to install it separately. For a
container deployment, mount `KAMUI_DATA_DIR` as a persistent volume. Each device has its own local
database; future multi-device synchronization should exchange records through an API rather than
copying the database file.
