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
