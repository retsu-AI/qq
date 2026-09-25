# Providers and credentials

A **provider** is where model requests go. A **credential** is how QQ
authenticates there. QQ keeps credentials in your operating system's secret
store and never writes them into configuration.

## Built-in providers

These exist without any configuration. Store a credential once and use any
of their models.

| Provider id | Store a credential | Environment variable | Notes |
| --- | --- | --- | --- |
| `openai` | `qq auth login openai` | `OPENAI_API_KEY` | Responses API |
| `anthropic` | `qq auth login anthropic` | `ANTHROPIC_API_KEY` | Messages API; prompt caching used automatically |
| `google` | `qq auth login google` | `GEMINI_API_KEY` or `GOOGLE_API_KEY` (`GEMINI_API_KEY` wins when both are set) | key sent in the `x-goog-api-key` header, never the URL; tool schemas are reduced to Gemini's schema subset (unsupported JSON Schema keywords such as `additionalProperties`, `$ref`, and `oneOf` are dropped) so MCP tools with rich schemas still declare |
| `xai` | `qq auth login xai` or `qq auth login xai --oauth` | `XAI_API_KEY` | OAuth uses a device code and refreshes itself |
| `openai-codex` | `qq auth login openai-codex` | — | ChatGPT Plus/Pro/Business subscription; opens the browser |
| `bedrock` | AWS credential chain | `AWS_PROFILE`, `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY`, web identity, container credentials | Converse API; needs a `region`, see below |
| `bedrock-mantle` | AWS credential chain or `ApiKey` | as above | OpenAI/Anthropic wire protocols on Bedrock |

Resolution order for a built-in: an explicit `api_key:` in `providers`, then
the stored credential `PROVIDER/default`, then the environment variable.

## Storing credentials

```sh
qq auth login anthropic                  # prompts, no echo
printenv ANTHROPIC_API_KEY | qq auth login anthropic   # from a pipe
qq auth login openai --profile work      # a second key under openai/work
qq auth list                             # names, backend, kind, endpoint; never secrets
qq auth status openai/default
qq auth logout openai/default
```

`qq auth login` accepts only the built-in provider ids above. For anything
else use `qq auth set NAME`, which stores an arbitrary named secret you can
reference as `Stored("NAME")`:

```sh
qq auth set gateway/default --endpoint https://llm.example.com
```

`--endpoint` binds the secret to one host so a misconfigured `base_url` can
never send it elsewhere.

### Where credentials go

| Platform | Backend |
| --- | --- |
| Linux | Secret Service (GNOME Keyring, KWallet, KeePassXC…) |
| macOS | Keychain |
| Windows | Credential Manager; entries above its size limit go to a DPAPI-encrypted file bound to your user and machine |

If no keyring is available (a container, a headless server), pass
`--allow-file` to store a user-only plaintext file under the data
directory. Prefer environment variables in CI.

### Profiles

`PROVIDER/PROFILE`. `default` is what the built-in providers use unless you
declare otherwise:

```ron
providers: {
    "openai": OpenAi(api_key: Stored("openai/work")),
    "openai-codex": OpenAiCodex(profile: "work"),
}
```

## Built-in models

QQ ships a catalog with context windows, output limits, and pricing so cost
tracking and `--max-cost-usd` work out of the box. Any model not listed can
still be used by declaring it under the provider's `models`.

| Provider | Routes |
| --- | --- |
| `openai` | `gpt-6-sol`, `gpt-6-luna`, `gpt-6-astra`, `gpt-5.6`, `gpt-5.6-sol`, `gpt-5.6-luna`, `gpt-5.6-terra`, `gpt-5.5`, `gpt-5.4`, `gpt-5.4-mini`, `gpt-5.4-nano`, `gpt-5.3-codex-spark`, `gpt-5.2`, `gpt-5-mini`, `gpt-5-nano`, `gpt-4.1`, `gpt-4.1-mini`, `gpt-4.1-nano`, `gpt-4o`, `gpt-4o-mini` |
| `openai-codex` | the OpenAI routes your subscription includes |
| `anthropic` | `claude-opus-5`, `claude-sonnet-5`, `claude-fable-5`, `claude-opus-4-8`, `claude-opus-4-7`, `claude-opus-4-6`, `claude-opus-4-5`, `claude-sonnet-4-6`, `claude-sonnet-4-5`, `claude-haiku-4-5` |
| `google` | `gemini-3.6-flash`, `gemini-3.5-flash`, `gemini-3.5-flash-lite`, `gemini-3.1-pro-preview`, `gemini-3.1-flash-lite`, `gemini-3-flash-preview`, `gemini-2.5-pro`, `gemini-2.5-flash`, `gemini-2.5-flash-lite` |
| `xai` | `grok-4.7`, `grok-4.6`, `grok-4.5`, `grok-4.3` |
| `bedrock`, `bedrock-mantle` | the same Anthropic and OpenAI models under their Bedrock ids |

The TUI's `/models` lists authenticated providers and includes live discovery.
For Codex, a successful response replaces implicit bundled entries: hidden or
retired models no longer linger in the picker. Explicit `models` declarations
and the currently selected route remain visible; neither grants account access.
Failed discovery falls back to the bundled catalog. Results are cached for five
minutes. QQ sends Codex client version `0.156.1` for discovery (the upstream
release that adds GPT-6 Sol/Luna); upgrading a separately installed Codex CLI
does not change QQ's discovery version.

GPT-6 Sol/Luna support `none`, `low`, `medium`, `high`, `xhigh`, and `max`.
GPT-6 Astra supports `low` through `max`, but not `none` or `minimal`.
Direct Claude Sonnet 5 / Opus 4.7 and 4.8 support `low`, `medium`, `high`,
`xhigh`, and `max`; Opus/Sonnet 4.6 omit `xhigh`, and Opus 4.5 stops at `high`.
The picker shows only the selected model's advertised choices. Unknown models
show only Default. Default retains the existing configuration/profile inheritance
semantics; it is not the explicit `none` level. Anthropic uses
`output_config.effort`, never an OpenAI reasoning field.

Protocol 28 and session-store schema 36 introduce `max`; older binaries cannot
open upgraded stores. Back up the store before upgrading if rollback is needed.

Anthropic discovery now follows bounded pagination before replacing implicit
bundled model entries, preserving configured routes and failure fallback.
Their pricing is currently unknown in the bundled catalog; cost-budget behavior
therefore remains conservative. Codex uses the conservative bundled 272K
context limit, distinct from the direct API's 1.05M limit.

Opus 5.5 is not yet advertised as supported: its always-on thinking requires
signed thinking-block replay during tool rounds, which QQ does not yet retain.


## Amazon Bedrock

```ron
(
    version: 1,
    model: "bedrock/anthropic.claude-sonnet-5",
    providers: {
        "bedrock": AmazonBedrock(region: "us-east-1", auth: Aws(DefaultChain)),
    },
)
```

`auth` is `Aws(DefaultChain)`, `Aws(Profile("NAME"))`, or a Bedrock API key
`ApiKey(Env("BEDROCK_API_KEY"))`. Profiles that use `credential_process` are
rejected: QQ cannot guarantee the subprocess terminates on a timeout.

Bedrock Mantle exposes the same models over the OpenAI Responses, OpenAI
Chat Completions, or Anthropic Messages protocols:

```ron
"bedrock-mantle": AmazonBedrockMantle(region: "us-east-1", api: AnthropicMessages, auth: Aws(DefaultChain)),
```

Both need the default build; `--no-default-features` builds refuse Bedrock
with an error naming the missing `provider-bedrock` feature.

## Gateways and self-hosted models

Anything that speaks one of the four wire protocols works as a `Custom`
provider:

```ron
providers: {
    "local": Custom(
        connection: (
            base_url: "http://127.0.0.1:11434/v1",      // loopback may use http
            api: OpenAiChatCompletions,
            auth: NoAuth,
        ),
        models: { "qwen3:32b": (name: "Qwen 3 32B", context_window: 32768) },
    ),
    "gateway": Custom(
        connection: (
            base_url: "https://llm.example.com/v1",
            api: AnthropicMessages,
            auth: Bearer(Env("GATEWAY_TOKEN")),
        ),
    ),
    "litellm": LiteLlm(
        connection: (base_url: "https://litellm.example.com/v1", api: OpenAiChatCompletions, auth: ApiKey(Env("LITELLM_API_KEY"))),
    ),
}
```

Non-loopback `http://` is rejected unless `policy.require_https: false`.
Model routes are `local/qwen3:32b`; declare each model you want to use.

## Checking that a credential works

```sh
qq auth status anthropic/default          # stored? which backend? bound endpoint?
qq ask --model anthropic/claude-sonnet-5 "Reply with pong"
```

If the second command fails, [Troubleshooting](troubleshooting.md#credentials)
lists each message and its fix.
