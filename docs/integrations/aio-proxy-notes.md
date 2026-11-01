# aio-proxy integration notes

## Source and license

- Reference checkout: `D:\refs\aio-proxy`.
- License: MIT (`D:\refs\aio-proxy\LICENSE`). No source was copied.
- Relevant contract: `packages/plugin-sdk/src/oauth.ts` (`CredentialPort`, `OAuthAdapter`); runtime shape: `packages/plugin-sdk/src/runtime.ts` (`OAuthRuntimeResult`); concrete account-specific implementations: `packages/plugins/openai-chatgpt/src/plugin/plugin.ts` and `src/runtime/runtime.ts`, plus GitHub Copilot's plugin/runtime.

## What transfers to NexusLink

OAuth subscriptions should be modeled as an adapter-backed provider kind, not as an ordinary static API key:

1. An adapter owns account options, credential validation, login/consent, token exchange, refresh, and provider-specific runtime construction.
2. Credentials need typed fields such as access token, refresh token, expiry, account identity, and revision. The host needs a credential port with serialized refresh and compare-and-swap semantics so simultaneous requests do not rotate the same refresh token twice.
3. Runtime construction owns upstream URL/header/body adaptation. The ChatGPT adapter, for example, obtains a current credential, replaces authorization with a bearer access token, adds account/session headers, rewrites the provider endpoint, and adapts request bodies. This is not generic OpenAI-compatible API-key traffic.
4. Catalog discovery belongs to the adapter and should be explicit, bounded, and refreshable; subscription model availability cannot be assumed from a static API catalog.
5. Authentication failure and refresh failure must be surfaced distinctly. Never silently convert an OAuth account to an API-key provider or expose refresh credentials in the UI/logs.

## NexusLink gap and placement

- Current provider config is a static `base_url` + `api_key`/`api_keys` + OpenAI-compatible chat path (`src/config.rs`, `Provider`; `src/proxy.rs`, `send_upstream`). `Resolved` carries a raw `api_key`; it has no credential handle, provider auth strategy, refresh lifecycle, or request adapter.
- `src/gateway.rs` writes Claude Desktop configuration and does not perform provider routing. The request path is `src/proxy.rs`; model/provider config is `src/config.rs`. Future OAuth work belongs in a new provider-runtime/auth module used by `proxy.rs`, with UI/config lifecycle work kept separate from the Desktop config writer.
- `NUEKeyStore` is for NexusLink clients, not upstream OAuth credentials. Do not reuse it for ChatGPT/Claude/Copilot subscription secrets.
- Before enabling account-backed providers, define secure credential persistence (platform keychain/DPAPI or a vetted encrypted store; Unix permissions alone are only a baseline), explicit user consent, refresh concurrency, revocation/logout, and per-provider terms/endpoint policy.

## Minimal staged implementation proposal

1. Add a provider auth enum with `api_key` default so existing JSON remains compatible; add an opaque account/credential reference for OAuth entries, never refresh/access tokens inside model mappings.
2. Define a small adapter contract: login/authorize, validate+refresh credential, discover models, and execute/adapt a request. Keep provider-specific endpoints and headers in adapters.
3. Add secure credential storage and refresh single-flight/CAS before adding any subscription provider. Fail closed on absent, expired-unrefreshable, or revoked credentials.
4. Implement one provider adapter with mock-based tests for login result validation, refresh concurrency, request adaptation, catalog failure, logout, and secret redaction. Add further providers only after their supported auth/API terms are verified.

This is a separate feature slice, not part of the current routing patch. Implementing only a bearer-token field would appear to support subscriptions while failing as soon as that short-lived token expires.
