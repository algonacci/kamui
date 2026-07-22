# Kamui Roadmap

Kamui is evolving from a provider-agnostic chat CLI into a repository-aware coding agent. The
roadmap prioritizes a safe, useful read-only workflow before file mutation and command execution.

## Phase 1: Chat Foundation

- [x] Interactive streaming chat
- [x] Provider-agnostic core
- [x] OpenAI-compatible provider
- [x] SQLite session storage
- [x] Session list, resume, and delete
- [x] Auto title generation
- [x] Token and context usage statistics
- [x] Cross-platform installers and release workflow

## Phase 2: Repository-Aware Chat

- [x] Project instructions (`KAMUI.md` or `AGENTS.md`)
- [x] Read-only `@file` context
- [x] Git diff context (`@diff` and `@staged`)
- [x] Session rename
- [x] Conversation search
- [x] Latency and time-to-first-token tracking

Descoped from Phase 2 (not planned): custom global instructions and Markdown export. Do not
re-add without a concrete user request.

## Phase 3: Coding Agent Runtime

- [x] Provider-agnostic tool-call protocol
- [x] Tool runtime, dispatch, and streaming agent loop
- [x] Read file tool
- [x] List directory tool
- [x] Safe terminal command runner with permission, timeout, and output limits
- [x] Preserve raw output on failures and command exit codes
- [x] Optional RTK execution backend with direct-command fallback
- [x] Patch file tool with confirmation
- [x] Multi-file editing
- [x] Git status, diff, and commit integration
- [x] Tool audit trail
- [x] Cancellation and failed-tool recovery

Phase 3 is complete. Kamui can explore (`list_directory`), read (`read_file`), execute
(`run_command` with approval and optional RTK compression), and edit (`patch_file` with preview and
approval), with whole turns persisted so resumed sessions replay tool interactions. A turn's
recorded usage folds every agent-loop round together: output tokens accumulate across rounds while
the input count tracks the final round, so context reporting stays correct without double-counting.

How the last four items are satisfied:

- Multi-file editing: the agent loop runs any number of `patch_file` calls within a single turn,
  each independently previewed and approved, so a change spanning several files is one conversation
  turn. A single atomic multi-file transaction is deliberately not built; each file is its own
  reviewable, atomic write.
- Git status, diff, and commit integration: Git is available through `run_command` (with approval
  and RTK compression) for status, diff, and commit, and read-only history context is available
  through `@diff` and `@staged`. No Git-specific tool is needed on top of these.
- Tool audit trail: every tool request and result is persisted as part of the turn (see the tool
  message persistence above) and replayed on resume, which is the durable record of what ran.
- Cancellation and failed-tool recovery: `Ctrl+C` during a turn interrupts it and returns to the
  prompt, killing any running command, without saving the partial turn; at the idle prompt it exits.
  Tool failures (bad arguments, a patch that does not match, a declined command, an unknown tool)
  are returned to the model as text so it can recover on the next round rather than aborting.

Progress: the provider-agnostic tool-call types (`ToolDefinition`, `ToolCall`, tool-request and
tool-result messages), their OpenAI serialization, and both non-streaming and streaming (index-keyed
delta reassembly) parsing have landed with tests. The core no longer serializes its own message
types into an OpenAI-shaped payload; wire mapping lives in the provider. A `ToolRegistry` dispatches
calls, read-only `read_file` and `list_directory` tools reuse the shared `@file` path-safety checks,
and the chat loop runs a streaming agent loop (bounded by a per-turn round limit) that executes
requested tools and feeds results back until the model returns a plain answer. Tool failures are
returned to the model as text so it can recover.

The `run_command` tool executes shell commands in the project directory. Kamui owns the permission
policy: any tool that reports `requires_confirmation` (only `run_command` so far) is shown to the
user and must be approved with `y`/`yes` before it runs; declining feeds a refusal back to the
model. Commands run with stdin disabled, a 30-second timeout that kills the process, and a 16 KiB
output cap; the result carries the exit code plus captured stdout and stderr.

RTK routing is in place: the `rtk` binary is detected once per process, and simple commands are
prefixed with `rtk` so compressed output reaches model context. Commands containing shell operators,
commands already prefixed with `rtk`, and every command on systems without RTK run directly. The
first line of each result records the exact command line that was executed.

The `patch_file` tool edits one file per call by exact-match replacement: `old_text` must match the
file exactly once, or the patch is rejected with guidance so the model re-reads and retries; an
empty `old_text` creates a new file that must not already exist. Every patch shows a +/- line
preview and requires the same `y`/`yes` approval as commands. Writes go through a temporary file
and rename so an interrupted write cannot leave a half-written file, and paths resolve through the
same containment checks as reading (a new file's parent directory must already exist inside the
project). Multi-file editing beyond one file per call remains future work.

Tool messages are now persisted. A `user_version = 3` migration rebuilds the `messages` table to
allow the `'tool'` role and store `tool_calls` and `tool_call_id`, and a whole turn (prompt, tool
requests, tool results, final answer) is saved atomically, so resumed sessions replay the tool
interactions. Still approximate: recorded usage is the final round's, not the sum across a turn's
rounds. Per-turn usage accounting, the terminal runner, mutation tools, and a durable audit trail
remain future work.

RTK is an execution optimization, not the command permission layer. When installed, Kamui should
route supported terminal commands through the external `rtk` binary to reduce tool output before it
enters model context. Unsupported commands and systems without RTK must continue to work directly.
Raw `git diff` remains available for code review because a condensed diff can omit required detail.

## Phase 4: Context Management

- [ ] Directory context with ignore rules
- [ ] Clipboard context
- [ ] Conversation summarization
- [ ] Context compression
- [ ] Project indexing
- [ ] Semantic search
- [ ] Image input
- [ ] PDF input

## Phase 5: Providers and Models

- [x] Structured configuration file (`kamui.toml`) for provider, model, and settings
- [ ] Native Anthropic provider
- [ ] Native Gemini provider
- [ ] Provider and model discovery
- [ ] Runtime model switching
- [ ] Temperature and model parameters
- [ ] Provider and model statistics
- [ ] Document OpenAI-compatible services (OpenRouter, Ollama, LM Studio, Groq, DeepSeek, LiteLLM)

Configuration is built (`src/config.rs`): a TOML `kamui.toml` replaces `.env` and `dotenvy`. A
global `kamui.toml` in the OS config directory may hold the API key; a per-project `kamui.toml` in
the working directory is non-secret only and is rejected if it sets `api_key`. Resolution precedence
is project, then global, then built-in defaults, with no environment variables in provider or model
configuration. First run scaffolds the global config directory with a commented template and exits
so the user can fill in the key. `KAMUI_DATA_DIR` still overrides only the database location.

## Phase 6: Terminal Experience

- [ ] Markdown rendering
- [ ] Syntax highlighting
- [ ] Thinking indicator
- [ ] Rich terminal UI
- [ ] Session pinning
- [ ] Prompt templates
- [ ] System prompt profiles
- [ ] Daily and monthly usage reports
- [ ] Optional cost tracking
- [ ] Benchmark mode

## Later: Extensibility and Remote Work

- [ ] MCP client
- [ ] Plugin API and manager
- [ ] Background jobs and job queue
- [ ] Scheduled tasks
- [ ] Worker nodes and remote execution
- [ ] Local memory and RAG
- [ ] Multi-agent workflows

## Not Planned Soon

- [ ] GUI and dashboard
- [ ] Mobile app
- [ ] Voice mode
- [ ] MCP server
- [ ] Infrastructure-specific plugins (Docker, Kubernetes, PostgreSQL)
