# genai × BB3 LiteLLM — throwaway prototype

Disposable proof for the Saddle ticket "Validate a multi-provider inference crate against BB3's
LiteLLM". Not v1 code; nothing here is meant to be merged.

It drives [`genai`](https://crates.io/crates/genai) `=0.7.1` against `https://litellm.bb3.internal/v1/`
on both API families Saddle supports, with one binary:

```
genai-proto <messages|responses> <model> <stream|tools|cancel> [--effort low|medium|high]
```

- `stream` — streams a short answer to stdout (reasoning summaries dimmed).
- `tools` — declares `read_file`, lets the model call it, answers with a canned `Cargo.toml`,
  and loops until the model gives a final answer. The assistant turn is replayed from genai's
  `captured_content`, which carries thinking signatures / encrypted reasoning.
- `cancel` — drops the stream after 40 chunks, then sends a follow-up on the same client.

Env: `LITELLM_API_KEY` (required), `LITELLM_BASE_URL`, `CA_FILE` (extra PEM trust anchor),
`NO_FALLBACK=1` (sends LiteLLM's `disable_fallbacks`), `RAW_FRAMES=1` (dump SSE frames).

```
cargo build && LITELLM_API_KEY=sk-... ./run_matrix.sh   # logs/ holds each run's transcript
```

## Key wiring

All routing is pinned by a `ServiceTargetResolver`: base URL, the key, and the adapter
(`AdapterKind::Anthropic` for Messages, `AdapterKind::OpenAIResp` for Responses) come from
config, and the model name (`anthropic/…`, `chatgpt/…`, `melious/…`) is passed through
untouched. genai's name-based adapter inference must not be used: every BB3 alias has a
LiteLLM route prefix.

genai sends `x-api-key: <key>` + `anthropic-version` on Messages and
`Authorization: Bearer <key>` on Responses; LiteLLM accepts the same virtual key either way.

## Results (2026-10-25, LiteLLM v1.104.2, fallbacks disabled)

See `logs/`. Every row below streamed, and every `tools` row completed a full round-trip.

| Route | Model | stream | tools | reasoning round-trip | cancel |
|---|---|---|---|---|---|
| Messages | anthropic/claude-haiku-4-5-20251001 | ✅ | ✅ | – | ✅ |
| Messages | anthropic/claude-sonnet-4-6 (`effort=medium`) | ✅ | ✅ | ✅ thinking + signature replayed | – |
| Messages | anthropic/claude-opus-4-8 (`effort=low`) | ✅ | – | – | ✅ |
| Responses | chatgpt/gpt-5.6-luna | ✅ | ✅ | ✅ encrypted reasoning replayed (`store:false`) | ✅ |
| Messages | melious/glm-5.3 | ✅ | ✅ | – | – |
| Responses | melious/glm-5.3 | ✅ | ✅ | – | – |
| Messages (translated) | chatgpt/gpt-5.6-luna | ✅ | ✅ | ⚠️ no reasoning surfaced | – |
| Responses (translated) | anthropic/claude-haiku-4-5-20251001 | ✅ | ✅ | – | – |
| Responses (translated) | anthropic/claude-sonnet-4-6 | ❌ 429 | ❌ | – | – |

## Quirks found

1. **anthropic/* Sonnet/Opus refuse the Responses route (429 `rate_limit_error`, message
   `"Error"`)**. BB3's `anthropic_subscription` pre-call hook adds the Claude Code identity
   only for `/chat/completions` and `/v1/messages` call types; Responses calls arrive
   unidentified, which the subscription refuses for everything but Haiku. Use Messages for
   `anthropic/*`.
2. **`chatgpt/*` silently falls back to the paid API on malformed input.** The Codex backend
   rejects a string `input` (`"Input must be a list"`), and BB3's router fallback then retries
   on `openai/<same>` with no error — only the `x-litellm-attempted-fallbacks` header tells.
   genai always sends a list, so it stays on the subscription. `disable_fallbacks: true`
   (via `ChatOptions::with_extra_body`) exposes such errors during development.
3. **Responses SSE from LiteLLM has no `event:` lines**, only `data:` frames carrying `type`.
   Messages SSE has `event:` lines and pads `data:` JSON with trailing spaces. genai handles
   both.
4. **Cross-family translation drops reasoning**: `chatgpt/*` over Messages answers and calls
   tools, but no thinking/summary is streamed even with an effort set.
5. Tool-call ids: `toolu_…` on anthropic, `call_<32 hex>` on chatgpt, `call_<24 hex>` on
   melious, preserved through the round-trip on both routes and across translation.
6. genai's `ToolCallChunk` carries the *accumulated* argument string so far, not a delta.
7. genai sets `store:false` and asks for `reasoning.encrypted_content` on Responses, so
   multi-turn reasoning is stateless and replayable from Saddle's own transcript.
8. Cancellation = dropping the `ChatStream`. The client stays usable immediately. Whether
   LiteLLM aborts the upstream generation on client disconnect was not verified.

## TLS

genai's default `rustls` feature uses `rustls-platform-verifier`: system trust store on
Linux, Keychain on macOS. With the BB3 root CA trusted there, no extra config is needed.
For a host that does not trust it, `CA_FILE=ca/bb3_root_ca.crt` (from emilbroman/bb3) works:
it builds a `reqwest::Client` with `tls_certs_merge` and hands it over through
`Client::builder().with_reqwest(...)`. Verified in a Linux sandbox by pointing `SSL_CERT_FILE` at
a bundle without the BB3 CA: it fails without `CA_FILE` and succeeds with it. macOS Keychain
behaviour still needs the user to check.
