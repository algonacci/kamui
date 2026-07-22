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
- [ ] Git diff context
- [ ] Custom global instructions
- [ ] Session rename
- [ ] Session export to Markdown
- [ ] Conversation search
- [ ] Latency and time-to-first-token tracking

## Phase 3: Coding Agent Runtime

- [ ] Provider-agnostic tool-call protocol
- [ ] Read file tool
- [ ] Patch file tool with confirmation
- [ ] Multi-file editing
- [ ] Terminal command tool with permissions and timeout
- [ ] Git status, diff, and commit integration
- [ ] Tool audit trail
- [ ] Cancellation and failed-tool recovery

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

- [ ] Native Anthropic provider
- [ ] Native Gemini provider
- [ ] Provider and model discovery
- [ ] Runtime model switching
- [ ] Temperature and model parameters
- [ ] Provider and model statistics
- [ ] Document OpenAI-compatible services (OpenRouter, Ollama, LM Studio, Groq, DeepSeek, LiteLLM)

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
