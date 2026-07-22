# Kamui Development Guide

This file is the engineering handoff and working agreement for AI agents contributing to Kamui.
Read `README.md` for user-facing documentation and `ROADMAP.md` for prioritized product phases.

## Product Direction

Kamui is a provider-agnostic LLM CLI written in Rust. It is evolving from interactive chat into a
repository-aware coding agent in the direction of Codex and Claude Code.

The near-term goal is not to build every possible AI feature. Prioritize a reliable coding workflow:

1. Safe read-only repository context.
2. A provider-agnostic tool-call protocol.
3. Permissioned file editing and command execution.
4. Efficient context management and additional providers.

Prefer small, complete capabilities over broad but incomplete systems. Challenge roadmap items whose
effort or operational risk is disproportionate to their immediate value.

## Current Product Behavior

- Every normal launch starts a new chat. Resume must be explicit with `/resume <id>` or
  `kamui -r <id>`.
- Sessions are created lazily after the first successful streamed response. Empty chats are not
  persisted or listed.
- Completed user/assistant exchanges are stored in SQLite. Partial responses from interrupted or
  failed streams are not added to history.
- The first completed exchange receives an AI-generated title. Title-generation usage is recorded
  with kind `title`, while the request count shown to users counts only primary chat requests.
- Streaming deltas are printed immediately. Usage and finish reason are shown after completion.
- `Ctrl+C` shuts down gracefully. Windows stdin uses a reader thread and Tokio channel so the async
  runtime does not block on terminal input.
- Supported chat commands are `/help`, `/new`, `/sessions`, `/resume <id>`, `/rename <id> <title>`,
  `/search <text>`, `/delete <id>`, `/stats`, and `/exit`. Plain `exit` also quits.
- After each streamed response the usage line reports time-to-first-token and total response time.
  These latency figures are displayed only, not persisted.
- Session IDs may be resolved from an unambiguous prefix. The UI normally displays the first eight
  characters.
- Resume displays the six most recent messages and reports how many earlier messages were omitted.
- Context percentage is displayed only when `KAMUI_CONTEXT_WINDOW` is configured.

## Repository Context

The process working directory is the project root.

- At startup, Kamui loads `KAMUI.md` if present, otherwise `AGENTS.md`. The selected content becomes
  a system message on every request.
- `CLAUDE.md` is an agent development guide and is intentionally not loaded by Kamui at runtime.
- A prompt can attach UTF-8 text with a relative reference such as `@src/main.rs`.
- Each file is limited to 64 KiB and all attached context is limited to 128 KiB per request.
- Absolute paths, directories, binary/non-UTF-8 files, and paths or symlinks outside the project root
  are rejected.
- Duplicate references are attached once.
- Expanded file contents are sent for that request only. The original user prompt, not expanded
  contents, is stored in session history.
- `@diff` attaches raw unstaged tracked changes using `git diff`.
- `@staged` attaches raw staged changes using `git diff --cached`.
- Untracked files are not in `@diff`; users must attach them explicitly with `@path`.
- Raw diff is deliberate. Do not silently replace it with a condensed representation because code
  review may require details omitted by summarization.

## Architecture

Important modules:

- `src/main.rs`: environment loading, CLI argument parsing, dependency construction, and startup.
- `src/chat.rs`: interactive loop, streaming display, session commands, title generation, and
  graceful shutdown.
- `src/context.rs`: project instruction discovery and safe `@file`, `@diff`, and `@staged`
  expansion.
- `src/provider/mod.rs`: provider-independent request, response, message, usage, and streaming types.
- `src/provider/openai.rs`: OpenAI-compatible Chat Completions HTTP and SSE implementation.
- `src/storage.rs`: SQLite schema, migration, sessions, messages, usage, and persistence tests.
- `.github/workflows/release.yml`: tag-triggered multi-platform release builds.
- `install.ps1` and `install.sh`: release-binary installers with SHA-256 verification.

Keep the core provider-agnostic. Provider-specific payloads and parsing belong under the provider
implementation. Do not leak OpenAI response structures into chat, storage, context, or future tool
runtime APIs.

The current `Provider` trait supports non-streaming `chat` and streaming `chat_stream`. The
non-streaming path is used for title generation and is the intended path for tool-calling turns.

The provider-agnostic tool-call protocol is modeled in `provider/mod.rs` as `ToolDefinition`,
`ToolCall`, and tool-request/tool-result `Message` variants; `ChatRequest` carries `tools` and
`ChatResponse` surfaces `tool_calls`. The OpenAI adapter maps these to and from wire types entirely
within `provider/openai.rs`, so the core no longer serializes its own types into an OpenAI-shaped
payload. Native Anthropic and Gemini adapters must reuse these same neutral types. Still unbuilt: a
tool runtime and dispatch layer, the agent loop, streaming tool-call assembly, and tool-message
persistence (the `messages.role` CHECK constraint does not yet allow `'tool'`).

## Storage Decisions

- SQLite is compiled with the `bundled` feature so end users do not install SQLite separately.
- Use schema migrations through `PRAGMA user_version`; do not make destructive schema assumptions.
- The default database is in the operating system local application data directory under `kamui`.
- `KAMUI_DATA_DIR` overrides the data directory for servers and containers.
- Multi-device synchronization, if built, should exchange records through an API. Do not synchronize
  by copying a live SQLite database.
- Foreign keys and cascading session deletion must remain enabled.
- Save an exchange and its usage atomically.

## Configuration

Configuration precedence is:

1. Process environment.
2. `.env` in the current working directory.
3. Global Kamui `.env` in the OS configuration directory.

Relevant variables:

- `OPENAI_API_KEY`: required for the current provider.
- `OPENAI_BASE_URL`: defaults to `https://api.openai.com/v1`.
- `OPENAI_MODEL`: required model identifier.
- `KAMUI_CONTEXT_WINDOW`: optional integer used for context percentage reporting.
- `KAMUI_DATA_DIR`: optional storage override.

Never commit `.env`, API keys, credentials, provider responses containing secrets, or local database
files. If a key appears in logs, chat, commits, or screenshots, advise immediate rotation.

OpenRouter, Ollama, LM Studio, Groq, DeepSeek, and LiteLLM may work through an OpenAI-compatible base
URL. Describe these as OpenAI-compatible services, not native provider integrations. Native
Anthropic and Gemini support requires dedicated adapters.

## RTK Decision

[RTK](https://github.com/rtk-ai/rtk) is planned as an optional external execution backend in Phase 3.
It is a Rust application, but it currently exposes a binary target rather than a stable public Rust
library API. Do not add it as a Cargo dependency or copy its source into Kamui.

The intended execution flow is:

```text
model requests a command
  -> Kamui validates permission and policy
  -> Kamui applies timeout, cancellation, and output limits
  -> supported command runs through the external `rtk` binary when available
  -> otherwise command runs directly
  -> Kamui records the command, exit status, and result
  -> compact output enters model context
```

RTK responsibilities:

- Filter, group, deduplicate, and compress command output.
- Preserve useful failures and command exit status.
- Reduce model context used by tests, builds, searches, Git, containers, and other supported tools.

Kamui responsibilities that RTK does not replace:

- Tool-call protocol.
- User permission and confirmation policy.
- Path and command safety.
- Timeout and cancellation.
- Output size limits.
- Audit trail and recovery behavior.
- Direct-command fallback.

Do not require RTK for normal chat or repository context. Detect it at runtime. A later `kamui doctor`
command may report its availability and version, and installers may offer it as an optional install.

## Priorities

The source of truth is `ROADMAP.md`. Current priority order is:

1. Phase 2 is complete. Session rename, conversation search, and latency/time-to-first-token
   tracking shipped. Custom global instructions and Markdown export were descoped; do not start them
   without a concrete user request.
2. Design Phase 3 provider-agnostic tool calls before implementing mutation or shell execution.
3. Build a safe terminal runner, then add optional RTK routing and direct fallback.
4. Add editing only with confirmation, containment, audit, and failure recovery.
5. Add broader context management and native providers after the runtime foundation is stable.

Avoid starting these early because their true scope is large:

- Write/patch/multi-file editing without an explicit tool runtime.
- Arbitrary terminal execution without permissions and timeout.
- Project indexing and semantic search.
- Context compression.
- MCP, plugin systems, remote workers, background jobs, or multi-agent execution.
- GUI, mobile, and voice clients.

## Coding Principles

- Prefer the smallest correct change.
- Keep behavior cross-platform across Windows, Linux, and macOS.
- Treat filesystem boundaries, symlinks, command invocation, and subprocess output as hostile input.
- Preserve existing user data and shipped behavior when changing storage or sessions.
- Avoid backward-compatibility layers unless persisted data, released behavior, or external consumers
  require them.
- Add concise comments only where behavior is not self-explanatory.
- Keep user-facing command names consistent. Resume uses `/resume` and `-r`; do not introduce
  ambiguous aliases without a concrete need.
- Do not persist expanded repository context unless a future design explicitly requires it.
- Do not count title-generation calls as primary chat requests.
- Do not save a partially streamed exchange as if it completed successfully.

## Verification

Before considering Rust changes complete, run:

```sh
rtk cargo fmt --all
rtk cargo test
rtk cargo clippy --all-targets --all-features -- -D warnings
rtk git diff --check
```

If RTK is unavailable, run the same commands without the `rtk` prefix. Also run `cargo check` or a
release build when changing dependencies, platform behavior, installers, or release packaging.

Current tests cover persistence, cascade deletion, session summaries, hidden empty sessions, SSE
parsing, project instruction precedence, file-reference expansion, duplicate references, unchanged
plain prompts, and staged Git diff expansion. Add focused tests for new parsing, storage, safety, and
cross-platform path behavior.

## Git and Releases

- Do not commit or push unless the user explicitly requests it.
- Do not rewrite or move a published tag. If release code changes after a tag, create a new patch
  version and tag.
- Release tags matching `v*` trigger five build targets: Windows x64, Linux x64, Linux ARM64, macOS
  Intel, and macOS Apple Silicon.
- GitHub Release assets are required before public installers work because installers download from
  `releases/latest/download` and verify checksums.
- Tag `v0.1.0` points to the initial persistent streaming chat release commit. Its workflow run was
  blocked before jobs started because the GitHub account was locked due to a billing issue. No empty
  GitHub Release was intentionally published. Re-run or create the next patch release only after
  GitHub Actions billing is operational.

## Known Limitations

- The current provider uses the Chat Completions API, not a native Responses API or native tool-call
  loop.
- Paths containing spaces cannot currently be represented by the whitespace-based `@file` parser.
- Project instructions are loaded only from the launch directory, not recursively from ancestors or
  nested directories.
- `@diff` excludes untracked files and `@diff`/`@staged` require Git on `PATH`.
- Context limits are byte-based rather than tokenizer-aware.
- Cost analytics are intentionally deferred because pricing metadata and multi-provider semantics are
  not yet defined.
- Unix installer behavior has not been exercised locally from the Windows development environment.

## Definition of Done

A feature is complete when:

- Its behavior is provider-neutral unless explicitly provider-specific.
- Failure modes are clear and do not corrupt sessions or files.
- Relevant unit tests exist.
- Formatting, tests, strict Clippy, and diff checks pass.
- User-facing behavior is documented in `README.md`.
- Product priority or completion state is reflected in `ROADMAP.md`.
- No secret, local `.env`, database, build artifact, or unrelated worktree change is included.
