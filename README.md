# kamui

Provider-agnostic, repository-aware LLM chat CLI written in Rust.

The first provider implementation uses the OpenAI Chat Completions API. The core request and
response types are independent from that API so other providers can be added without changing
the chat interface.

## Configuration

Kamui is configured with a `kamui.toml` file. The first time you run it, Kamui creates a commented
template in your OS configuration directory and exits so you can fill in your API key:

| Platform | Global config file |
| --- | --- |
| Windows | `%APPDATA%\\kamui\\kamui.toml` |
| Linux | `~/.config/kamui/kamui.toml` |
| macOS | `~/Library/Application Support/kamui/kamui.toml` |

```toml
model = "gpt-5.5"
# context_window = 128000

[provider]
base_url = "https://api.openai.com/v1"
api_key = "sk-xxxxxxxx"
```

You may also place a `kamui.toml` in a project directory to override `model`, `context_window`, and
`provider.base_url` for that project. A project file **must not** contain an `api_key` — Kamui
rejects it, so a project config is safe to commit. The API key lives only in the global file.

Any service implementing the OpenAI Chat Completions API can be used by changing the base URL,
model, and API key. Chat responses use the API's SSE streaming mode and are rendered as deltas
arrive.

### OpenAI-compatible providers

Kamui talks to any OpenAI-compatible endpoint, so you can point it at hosted aggregators or a local
model without a dedicated integration. These are OpenAI-compatible services, not native providers;
tool use and streaming depend on the server and model you pick.

**OpenRouter** (many models behind one key):

```toml
model = "openai/gpt-4o-mini"   # any OpenRouter model id, namespaced as vendor/model
[provider]
base_url = "https://openrouter.ai/api/v1"
api_key = "sk-or-v1-..."
```

**Ollama** (local models, no network key):

```toml
model = "llama3.2"                        # a model you have pulled with `ollama pull`
[provider]
base_url = "http://localhost:11434/v1"
api_key = "ollama"                        # Ollama ignores this, but a value is required
# tools = false                           # set this if the model rejects the tools field
```

Many small local models do not support tool calling and reject requests that include tools. If you
see an error like `<model> does not support tools`, add `tools = false` to that profile — Kamui then
chats without offering tools.

Only the global `kamui.toml` holds the key; switching providers is a one-file edit. You can also
keep a per-project `kamui.toml` that sets just `model` and `provider.base_url` to pin a project to a
particular provider (never the key).

### Switching models and providers at runtime

Instead of editing the file every time, define named profiles once and switch with `/model`. Share
one API key across many models by defining a `[providers.<name>]` block and pointing profiles at it
with `provider = "<name>"`:

```toml
default_profile = "sol"

[providers.jatevo]
base_url = "https://api.jatevo.ai/v1"
api_key = "sk-jvo-..."          # defined once, used by every Jatevo profile

[providers.ollama]
base_url = "http://localhost:11434/v1"
api_key = "ollama"

[profiles.sol]
provider = "jatevo"
model = "gpt-5.6-sol"

[profiles.terra]
provider = "jatevo"
model = "gpt-5.6-terra"

[profiles.codeqwen]
provider = "ollama"
model = "codeqwen:latest"
tools = false                   # this model does not support tools
```

In chat, `/model` lists the profiles and marks the active one, and `/model codeqwen` switches the
active provider and model for the next messages. Your choice is remembered across restarts, and the
banner always shows which model is active — handy for comparing the same prompt across models. A
profile can still set `base_url`/`api_key` inline instead of referencing a provider.

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
| `/model [name]` | List provider profiles, or switch to one |
| `/rename <id> <title>` | Rename a session |
| `/search <text>` | Search saved messages across all sessions |
| `/compact` | Summarize older messages to free up context |
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

Long conversations are compacted automatically: once the recent history grows large, Kamui folds the
older messages into a running summary so the session can continue without overflowing the model's
context. Run `/compact` to do it on demand. The full history is always kept in storage — only what
is sent to the model each turn is compressed.

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

Use `@clipboard` to attach the current text contents of your system clipboard — handy for pasting an
error message, a stack trace, or a snippet from elsewhere:

```text
> Why does this happen? @clipboard
```

## Tools

Kamui offers the model four tools: `list_directory` (discover what is in a folder), `read_file`
(read a file), `run_command` (run a shell command), and `patch_file` (edit or create a file). When
you ask about code, the model can explore, read, build, test, and fix on its own instead of
requiring you to attach files with `@path`:

```text
> What does the agent loop in src/chat.rs do?
> Run the tests and tell me if anything fails.
> Fix the typo in the README heading.
```

If the model calls a tool, Kamui prints a short trace of each call, runs it, feeds the result back,
and continues streaming until a final answer. The read tools reuse the same path safety as `@file`
(project-relative only, no escaping the root, 64 KiB per file) and the loop is bounded so it cannot
run away.

`run_command` never runs on its own. Kamui shows you the exact command and waits for you to approve
it (`y`/`yes`); anything else declines and tells the model so. Commands run in the project directory
with input disabled, a 30-second timeout, and a capped amount of captured output, and the model sees
the exit code alongside stdout and stderr.

`patch_file` edits one file per call and is also gated behind your approval: Kamui shows the change
as removed (`-`) and added (`+`) lines before asking. A patch replaces text that must match the file
exactly once — if it does not, the patch is rejected and the model is told to re-read the file, so a
stale edit can never overwrite unexpected content. An empty `old_text` creates a new file. Writes are
atomic, and paths cannot escape the project root.

If the [RTK](https://github.com/rtk-ai/rtk) binary is installed, simple approved commands are
automatically prefixed with `rtk` so their output is compressed before it reaches the model. RTK is
optional: commands with shell operators and systems without RTK always run directly, and the first
line of every result shows the exact command that ran.

The whole turn is saved to session history, including the tool calls and their results, so a resumed
session replays the tool interactions the model relied on.

## RTK integration direction

[RTK](https://github.com/rtk-ai/rtk) is an optional execution backend for the `run_command` tool.
Supported commands run through RTK so compact test, build, search, Git, and container output reaches
the model. Kamui still owns command permissions, timeouts, cancellation, output limits, and the
recorded command line; RTK only compresses output.

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
