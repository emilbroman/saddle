#!/usr/bin/env bash
# Runs every scenario against BB3's LiteLLM with router fallbacks disabled and writes logs/.
# Requires LITELLM_API_KEY. Build first: cargo build.
set -u
cd "$(dirname "$0")"
export NO_FALLBACK=1
B=${CARGO_TARGET_DIR:-target}/debug/genai-proto
mkdir -p logs
run() {
  local name="$1"; shift
  timeout 120 "$B" "$@" > "logs/$name.log" 2>&1
  printf '%-26s exit=%s  %s\n' "$name" "$?" "$(grep -E '^OK|^Status' "logs/$name.log" | head -1 | cut -c1-120)"
}
# Messages route
run m-haiku-stream         messages  anthropic/claude-haiku-4-5-20251001 stream
run m-haiku-tools          messages  anthropic/claude-haiku-4-5-20251001 tools
run m-haiku-cancel         messages  anthropic/claude-haiku-4-5-20251001 cancel
run m-sonnet-think-tools   messages  anthropic/claude-sonnet-4-6 tools --effort medium
run m-opus-cancel          messages  anthropic/claude-opus-4-8 cancel --effort low
# Responses route
run r-luna-stream          responses chatgpt/gpt-5.6-luna stream
run r-luna-tools           responses chatgpt/gpt-5.6-luna tools
run r-luna-cancel          responses chatgpt/gpt-5.6-luna cancel
run r-luna-think-tools     responses chatgpt/gpt-5.6-luna tools --effort medium
# Open-weight route on both families
run m-glm-tools            messages  melious/glm-5.3 tools
run r-glm-tools            responses melious/glm-5.3 tools
# Cross-family translation by LiteLLM
run x-m-luna-tools         messages  chatgpt/gpt-5.6-luna tools
run x-m-luna-think-tools   messages  chatgpt/gpt-5.6-luna tools --effort medium
run x-r-haiku-tools        responses anthropic/claude-haiku-4-5-20251001 tools
run x-r-sonnet-stream      responses anthropic/claude-sonnet-4-6 stream
