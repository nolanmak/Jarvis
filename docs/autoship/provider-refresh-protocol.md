# Codex 0.164.0 refresh protocol evidence

The installed executable symlink inspected on 2026-10-26 resolved to the
standalone `0.164.0-x86_64-unknown-linux-musl` release. Official upstream tag
[`rust-v0.164.0`](https://github.com/openai/codex/releases/tag/rust-v0.164.0)
resolves to commit `6aaac694803004a63c6f393d153c96d22521a6b9`. The statements below
are pinned source evidence, not a live refresh receipt or a binary reproducibility
claim. No authentication files were read and no authenticated request was made
for this inspection.

## Request and route grant

The standard native ChatGPT refresh uses `POST https://auth.openai.com/oauth/token`
with `Content-Type: application/json`. Body fields are `grant_type` equal to
`refresh_token`, public `client_id` equal to `app_EMoamEEZ73f0CkXaXp7hrann`, the
secret `refresh_token`, and `refresh_reason`. Reasons are `proactive_expiry`,
`scheduled`, `unauthorized_recovery`, or `forced`. The ChatGPT call supplies no
`resource`. This is not an authorization-code or form-encoded grant.
[Caller and JSON selection](https://github.com/openai/codex/blob/6aaac694803004a63c6f393d153c96d22521a6b9/codex-rs/login/src/auth/manager.rs#L1693),
[request encoding and fields](https://github.com/openai/codex/blob/6aaac694803004a63c6f393d153c96d22521a6b9/codex-rs/login/src/oauth/client.rs#L97),
[reason values](https://github.com/openai/codex/blob/6aaac694803004a63c6f393d153c96d22521a6b9/codex-rs/login/src/oauth/client.rs#L45).

Recorded OAuth provenance can supply issuer/client identity; explicit
`CODEX_REFRESH_TOKEN_URL_OVERRIDE` and `CODEX_APP_SERVER_LOGIN_CLIENT_ID`
settings also exist. Their existence does not authorize arbitrary gateway
routes. A standard profile should grant only the reviewed HTTPS authority,
POST method and exact `/oauth/token` path. A different issuer/client requires
separate explicit review; do not infer or broaden grants from credential input.
[Endpoint/client selection](https://github.com/openai/codex/blob/6aaac694803004a63c6f393d153c96d22521a6b9/codex-rs/login/src/auth/manager.rs#L1784),
[provenance binding](https://github.com/openai/codex/blob/6aaac694803004a63c6f393d153c96d22521a6b9/codex-rs/login/src/auth/manager.rs#L3382).

## Response and persistence

`id_token`, `access_token`, and `refresh_token` are each `Option<String>`.
Missing or JSON-null fields become absent values; persistence replaces only
supplied values and retains the corresponding existing token otherwise.
`last_refresh` is set to local UTC time. The response type does not require
`expires_in`. A controller must validate supplied fields and preserve omitted
ones; requiring all three would reject valid installed-client responses.
[Response schema](https://github.com/openai/codex/blob/6aaac694803004a63c6f393d153c96d22521a6b9/codex-rs/login/src/auth/manager.rs#L1776),
[persistence](https://github.com/openai/codex/blob/6aaac694803004a63c6f393d153c96d22521a6b9/codex-rs/login/src/auth/manager.rs#L1647).

Refresh takes a lock and performs guarded account reload. Persistence reloads
storage under a commit guard, requires ChatGPT mode and the original login
identity: login owner, account ID and ChatGPT user ID. A narrow compatibility
case tolerates missing stored login-owner metadata only with matching account,
user and unchanged nonempty refresh token. These compare expected prior state
with stored credentials. They are **not** cryptographic verification of returned
JWTs or proof that a returned identity is unchanged. Controller token-only
import must retain its own account/identity and generation checks.
[Refresh guard](https://github.com/openai/codex/blob/6aaac694803004a63c6f393d153c96d22521a6b9/codex-rs/login/src/auth/manager.rs#L3115),
[login identity](https://github.com/openai/codex/blob/6aaac694803004a63c6f393d153c96d22521a6b9/codex-rs/login/src/auth/account_grant.rs#L132),
[persistence guard](https://github.com/openai/codex/blob/6aaac694803004a63c6f393d153c96d22521a6b9/codex-rs/login/src/auth/manager.rs#L1637).

Source compatibility does not establish successful rotation, crash recovery,
serialization across conversations, or fresh-session authentication using a
rotated service token. Those remain actual acceptance checks in the
[rollout procedure](rollout.md). Do not expose raw request/response token bodies
in receipts or public logs. This evidence adds no production policy grant and
does not establish Claude or routed-provider refresh semantics.
