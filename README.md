# kamui

Provider-agnostic LLM chat CLI written in Rust.

The first provider implementation uses the OpenAI Chat Completions API. The core request and
response types are independent from that API so other providers can be added without changing
the chat interface.

## Configuration

Create a `.env` file based on `.env.example`:

```env
OPENAI_API_KEY=sk-xxxxxxxx
OPENAI_BASE_URL=https://api.openai.com/v1
OPENAI_MODEL=gpt-5.5
```

Any service implementing the OpenAI Chat Completions API can be used by changing the base URL,
model, and API key.

## Run

```sh
cargo run
```
