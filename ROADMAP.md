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
- [ ] Optional RTK execution backend with direct-command fallback
- [ ] Patch file tool with confirmation
- [ ] Multi-file editing
- [ ] Git status, diff, and commit integration
- [ ] Tool audit trail
- [ ] Cancellation and failed-tool recovery

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
output cap; the result carries the exit code plus captured stdout and stderr. RTK routing and a
direct-fallback policy are not built yet, so commands always run directly.

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

- [ ] Structured configuration file (`kamui.toml`) for provider, model, and settings
- [ ] Native Anthropic provider
- [ ] Native Gemini provider
- [ ] Provider and model discovery
- [ ] Runtime model switching
- [ ] Temperature and model parameters
- [ ] Provider and model statistics
- [ ] Document OpenAI-compatible services (OpenRouter, Ollama, LM Studio, Groq, DeepSeek, LiteLLM)

Configuration design (decided, not yet built): a TOML config replaces `.env` and `dotenvy`. The
config file is named `kamui.toml` in both locations. A global `kamui.toml` in the OS config
directory may hold the API key. A per-project `kamui.toml` in the repository root is non-secret only
and must reject `api_key` with a clear error. Resolution precedence is project `kamui.toml`, then
global `kamui.toml`, then built-in defaults. No environment variables participate in provider or
model configuration. First run scaffolds the global config directory and
a commented `config.toml` template, then stops so the user can fill in the key. This work is
independent of the Phase 3 runtime and may be pulled forward.

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
