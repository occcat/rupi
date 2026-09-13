# rupi-llm

`LlmProvider` trait + 真 SSE。上层只依赖 trait。

| 路由 | 实现 |
|---|---|
| 默认 / `openai` / `openrouter` | `OpenAiCompatProvider` |
| `claude-*` / `anthropic` | `AnthropicProvider`（system 独立参数、tool_result、user 交替合并） |
| `gemini-*` / `gemini` / `vertex` | `GeminiProvider` |
| `azure` | deployment URL + `api-key` |
| `bedrock` | Anthropic Messages + Bearer（`AWS_BEARER_TOKEN_BEDROCK`） |
| 无 key | `MockProvider` |

思考档 `off|low|medium|high|xhigh|max` 映射 `reasoning_effort` / `thinkingLevel` / `thinking+budget`。429/5xx + `Retry-After` 指数退避。目录：内置 `models.json`，可被 `RUPI_MODELS` 或 `~/.rupi/models.json` 覆盖。`rupi models` / `--list-models`。
