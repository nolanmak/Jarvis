# Deft Protocol — Deftform Public REST API + Webhooks (C&C surface)

> **Status: SPEC FROM PUBLIC VENDOR DOCS — PARTIALLY VALIDATED, REST OF
> `REQUIRES LIVE VALIDATION`.**
>
> "Deft" in issue #116 ("hookup agent to our deft forms for c&c … explore
> their api or reverse engineer their internal api") resolves to
> **Deftform** (`https://deftform.com`) — an AI-assisted form builder with a
> *documented, official* public REST API (`https://deftform.com/api/v1/`) and
> a first-class outbound **webhook** on form submission. **No
> reverse-engineering is required** — this is the rare channel where the
> sanctioned path exists, so the #40-style ban-risk gate that governed
> WhatsApp does **not** apply here (see §7 Decision).
>
> Endpoints, auth, rate limits, and the webhook payload below are taken from
> the vendor help center (`help.deftform.com/api/*`,
> `help.deftform.com/nice-to-know/using-webhooks`) captured **2026-05-18**.
> Anything that the public docs do not pin to an exact JSON shape is flagged
> `REQUIRES LIVE VALIDATION` and must be confirmed against a real workspace +
> access token before the channel runs in non-dry-run mode. The Rust scaffold
> in `crates/augmentagent-channel-deft/` implements against this doc and is
> **inert until `AUGMENTAGENT_DEFT_ENABLED` is set** (see §6).
>
> Re-harvest + update this doc if Deftform versions `/api/v1` → `/api/v2` or
> changes the webhook envelope (symptom: 404 on documented paths, or webhook
> bodies that no longer carry the `data[]`/`uuid` shape in §4).

This doc has seven sections, matching the spike scope (#116):

1. What "Deft" is, and the official-vs-RE determination
2. Auth
3. Read endpoints (poll path)
4. Webhook (push path) — submission payload
5. Trigger semantics: poll vs push, idempotency/dedup, C&C command mapping
6. The Rust scaffold + gating + TODO map
7. Decision: recommended integration path + risks + go/no-go

---

## 1. What "Deft" is — official API vs RE determination

| Question | Finding | Source |
|---|---|---|
| Which "Deft"? | **Deftform** form builder (`deftform.com`). The user's "our deft forms" = a Deftform workspace's forms. **REQUIRES LIVE VALIDATION** that this is the exact product the user means (issue explicitly says "to be confirmed by the user"). | issue #116 repro note |
| Documented public API? | **Yes.** `https://deftform.com/api/v1/` with Bearer-token auth, documented endpoints, documented error envelope + rate limits. | `help.deftform.com/api/endpoints`, `/api/authentication`, `/api/introduction` |
| Documented push (webhook)? | **Yes.** Per-workspace webhook endpoints, attachable per form, fired on each submission. | `help.deftform.com/nice-to-know/using-webhooks` |
| RE of an internal API needed? | **No.** Both poll (REST) and push (webhook) are first-class sanctioned features. The issue's "or reverse engineer their internal api" branch is **not exercised** — there is nothing to RE. | — |

Because the sanctioned path exists, the WhatsApp #40 ban-risk gate model
(global kill-switch + allowlist *because the transport is unofficial*) is
**not** the governing concern here. We still ship behind an env gate (§6) —
but for *blast-radius / least-privilege* reasons (a C&C surface that can drive
the agent must be explicitly armed), **not** because the transport risks an
account ban.

---

## 2. Auth — VALIDATED (scheme), token lifecycle `REQUIRES LIVE VALIDATION`

Bearer token in the standard `Authorization` header. The token is generated
by the workspace owner at `https://deftform.com/settings/api`.

```
Authorization: Bearer <DEFTFORM_ACCESS_TOKEN>
Accept:        application/json
Content-Type:  application/json
```

Base URL: `https://deftform.com/api/v1/`

- The token is **workspace-scoped** (one workspace's forms + responses).
  Multi-workspace ⇒ multiple tokens. **REQUIRES LIVE VALIDATION**: whether a
  token can be scoped read-only vs. read-write (the docs expose write
  endpoints — `POST /forms/{id}/response`, `POST /forms/{id}/settings` — so a
  leaked C&C token is *write-capable*; treat it as a high-value secret).
- Token expiry / rotation policy: **not documented → REQUIRES LIVE
  VALIDATION.** Treat as long-lived; surface a clean "rotate the Deftform
  token" error on `401`.

Storage in this project: keyring slot `augmentagent/deft/<workspace_id>`
via `augmentagent-auth` (Linux Secret Service), mirroring the GitHub PAT
model. One token per connected workspace.

---

## 3. Read endpoints — poll path

All `GET`, all Bearer-auth, base `https://deftform.com/api/v1/`.

### `GET /workspace`
Workspace metadata. Use: validate a freshly-pasted token at
`augmentagent deft login` time (analogue of GitHub `whoami`).

### `GET /forms`
All forms in the workspace + their fields. Use: discover form IDs
(`OUm6T9`-shaped) and let the user pick which form(s) are the C&C surface.

### `GET /forms/{id}/fields`
Fields of one form: each field has a stable `uuid`, a human `label`, and an
optional user-defined `custom_key`. **The `custom_key` is the contract anchor**
for C&C (see §5) — labels can be reworded by whoever edits the form, `uuid`
is opaque, but `custom_key` is operator-chosen and form-unique.

### `GET /responses/{formId}`
**The poll primitive.** All submissions for a form.

- `{formId}` is the form-detail-page ID (lower-right), same value as `id`
  in `GET /forms`.
- Response envelope (vendor-documented shape): `{ "success": true, "data": …
  }`. The exact `data` array element shape for a *submission* is **not
  pinned in the public docs → REQUIRES LIVE VALIDATION.** Expected, by
  analogy with the webhook payload (§4) and the `POST .../response` body:
  per-submission `id` + `uuid` + a `data[]` of `{label, response, uuid,
  custom_key}`. The scaffold deserializes defensively (`#[serde(default)]`,
  unknown fields ignored) exactly like the GitHub `Notification` type.
- Pagination: **not documented → REQUIRES LIVE VALIDATION.** Plan: assume an
  unpaginated newest-first list in v0; add pagination once the live shape is
  known. Dedup is by submission `uuid` (see §5), so re-fetching the full list
  each tick is correct-but-wasteful and acceptable at our cadence.

### `GET /response/{UUID}/pdf`
PDF summary of one submission. Not needed for C&C; documented for
completeness (could attach to an approval card later).

### Write endpoints (documented, **not used by the C&C read path**)
- `POST /forms/{id}/response` — inject a synthetic submission. *Does not*
  fire admin/respondent emails. Useful only for an end-to-end self-test
  harness; behind the same gate, never auto-invoked.
- `POST /forms/{id}/settings` — mutate form config (`name`, `is_closed`, …).
  Out of scope for C&C; noted because the token can do it (least-privilege
  argument in §7).

---

## 4. Webhook — push path (preferred trigger)

Configured in workspace settings → create an **endpoint** (a URL we host) →
attach it to the chosen form(s). One form may have multiple endpoints; one
endpoint may be reused across forms.

- **Method:** `POST` (vendor docs say "sends a webhook on submit"; method not
  literally spelled out → treat as POST, **REQUIRES LIVE VALIDATION**).
- **Body shape (vendor-documented):** a `data` array; each element:

```json
{
  "submission_id": "<form-unique submission id>",
  "uuid": "<globally-unique submission uuid>",
  "data": [
    {
      "label": "Command",
      "response": "approve",
      "uuid": "6403fc2b-6d52-4231-b63f-db6ea9f651dd",
      "custom_key": "agent_command"
    },
    {
      "label": "Argument",
      "response": "make it shorter and warmer",
      "uuid": "9a1f...-...",
      "custom_key": "agent_arg"
    }
  ]
}
```

  The `label`/`response`/`uuid`/`custom_key` quartet is vendor-confirmed; the
  exact placement of `submission_id`/`uuid` *meta* keys and any wrapper
  envelope is **REQUIRES LIVE VALIDATION** (docs say "additional metadata
  including submission ID and UUID" without the literal frame).
- **Signing / verification:** **not documented → REQUIRES LIVE VALIDATION.**
  Unlike Calendly (`Calendly-Webhook-Signature: t=…,v1=hmac`) the Deftform
  docs describe **no HMAC/signature**. Mitigation baked into the scaffold:
  the webhook receiver requires a **shared-secret path token**
  (`/deft/webhook/<AUGMENTAGENT_DEFT_WEBHOOK_SECRET>`) and an exact-match
  `formId` allowlist, since the body itself is unauthenticated. Treat any
  request not carrying the secret path as hostile.
- **Retries:** not documented → REQUIRES LIVE VALIDATION. Assume *no* retry
  (so the receiver must be reply-fast and persist before ack); the poll path
  (§3) is the safety net for missed pushes.
- **Test affordance:** workspace UI "Send test" + webhook.site — usable to
  capture the real envelope and clear the `REQUIRES LIVE VALIDATION` flags
  here without a real respondent.

---

## 5. Trigger semantics

### Poll vs push
- **Push (webhook): preferred.** Lowest latency for a C&C surface; the agent
  reacts the moment a form is submitted. Receiver is an axum route on the
  existing dashboard process (mirrors how Calendly/Linear webhooks land).
- **Poll (`GET /responses/{formId}`): the safety net + the v0 path.** Because
  the webhook is unsigned and retry behavior is unknown, the poll path is
  authoritative for *correctness*; the webhook is a *latency optimization*.
  The scaffold's `Trigger` impl is the poll path (works with zero inbound
  networking, matches the spike's "runnable v0 that reads one submission"
  deliverable). Cadence: default 2 min, well inside the 60 req/min /
  1440 req/day budget (§ rate limits).

### Idempotency / dedup
Every submission carries a globally-unique `uuid`. Dedup key =
`deft:<uuid>`, written as `Email.message_id` (same pattern as
`gh:<thread_id>`). `Store::is_message_processed` gates re-processing, so the
poll path re-fetching the full list each tick is safe, and a webhook +
poll double-delivery of the same submission collapses to one action.

### Rate limits — VALIDATED
`60 requests/minute` and `1,440 requests/day` per token. Headers:
`x-ratelimit-limit`, `x-ratelimit-remaining`. On `429` back off; on
`401/403` surface "rotate Deftform token". Our 2-min poll = ~720 req/day for
one form, comfortably under the daily cap; multi-form deployments must widen
the interval (the scaffold's config exposes `poll_interval`).

### C&C command mapping
A form submission becomes an agent command-and-control instruction. The
mapping mirrors the WhatsApp control surface
(`augmentagent-channel-whatsapp::control::parse_control_command`):

| Deftform field (`custom_key`) | Maps to |
|---|---|
| `agent_command` = `approve`/`ok`/`send`/`yes` | `ApprovalActionHandler::approve` (acts on the most recent pending draft) |
| `agent_command` = `decline`/`skip`/`reject` | `ApprovalActionHandler::skip` |
| `agent_command` = `revise` + `agent_arg` | `ApprovalActionHandler::revise(arg)` |
| `agent_command` = anything else (or a free-text `agent_query` field) | `QueryHandler::answer` (wiki/email/web question, same as Discord query mode) |

The submission is normalized into the shared `Email` shape
(`platform="deft"`, `kind="dm"`, `account_entity_id="deft:<workspace_id>"`)
so it flows through the **exact** triage→draft→approval pipeline every other
channel uses — no bespoke downstream code. **Which `custom_key`s the operator
actually wires on their form is REQUIRES LIVE VALIDATION** (it's the user's
form; the scaffold's mapping is the documented default + is config-overridable
via `command_field_key` / `arg_field_key` / `query_field_key`).

---

## 6. Rust scaffold + gating

Crate: `crates/augmentagent-channel-deft/` — layout mirrors
`augmentagent-channel-github`: `lib.rs` / `auth.rs` / `api.rs` /
`channel.rs` / `types.rs`.

- `types.rs` — `DeftSubmission` (wire shape, defensively decoded),
  `command_from_submission` (the §5 mapping, fully unit-tested offline),
  `into_email` conversion.
- `auth.rs` — `DeftAuth { workspace_id, token, base_url }`, keyring slot
  `augmentagent/deft/<workspace_id>`. Mirrors `GithubAuth`.
- `api.rs` — `DeftApi` trait + `DeftClient` (reqwest). Endpoints from §3.
  Real HTTP is **compiled but gated**: every network method first checks
  `deft_enabled()` and returns `DeftError::Disabled` if the env gate is
  unset, so the crate is provably inert in prod until armed.
- `channel.rs` — `DeftChannel` implementing the `Trigger` contract
  (`next_work_items`) over the poll path; webhook receiver is a documented
  TODO (needs the live envelope from §4).
- Workspace `Cargo.toml`: member added. CLI: dependency added; **no `serve`
  spawn** until the live-validation TODOs clear — exactly like the GitHub
  channel "gates on a PAT ⇒ never spawned in prod" pattern, here it gates on
  `AUGMENTAGENT_DEFT_ENABLED` + a persisted token.

### TODO map (tied to the `REQUIRES LIVE VALIDATION` flags above)

| TODO | Blocks | Doc ref |
|---|---|---|
| Confirm "Deft" == Deftform with the user | everything | §1 |
| Capture real `GET /responses/{formId}` JSON (via live token or webhook.site) and pin `DeftSubmission` | un-gating poll path | §3 |
| Capture real webhook envelope + confirm POST + meta frame | webhook receiver impl | §4 |
| Confirm token rotation/expiry + read-only scoping | secret-handling hardening | §2 |
| Confirm operator's actual `custom_key`s on the form | command mapping defaults | §5 |
| Decide pagination strategy once list shape known | large-form correctness | §3 |

---

## 7. Decision — recommended path, risks, go/no-go

### Recommended path: **Official REST API, poll-first + webhook-optimized.**

Deftform ships a sanctioned public API *and* a webhook. There is **no reason
to reverse-engineer an internal API** and we explicitly do not. v0 = the poll
path (`Trigger` over `GET /responses/{formId}`), because it needs zero inbound
networking and satisfies the spike's "runnable v0 that reads one submission"
deliverable; the webhook is a fast-path bolt-on once its live envelope is
captured.

### Risks

| Risk | Severity | Mitigation |
|---|---|---|
| Webhook body is **unsigned** (no HMAC documented) | Med | Secret-in-path receiver + strict `formId` allowlist + poll path is authoritative; webhook only *accelerates* an action the poll would also produce. |
| A C&C surface drives the agent (approve/revise/query) — spoofed submission could trigger actions | **High** | (a) `AUGMENTAGENT_DEFT_ENABLED` arming gate; (b) commands route through the **existing approval pipeline** (an `approve` acts on a *human-queued* pending draft, it does not invent outbound); (c) `QueryHandler` answers are read-only. No new ungated outbound capability is created. |
| Deftform token is **write-capable** (can inject responses / mutate form settings) | Med | Treated as a high-value secret in the keyring; least-privilege noted; request a read-scoped token if/when Deftform offers one (REQUIRES LIVE VALIDATION). |
| Public docs don't pin submission/webhook JSON exactly | Med | Defensive serde + every unverified shape flagged + TODO map; channel cannot un-gate until shapes are captured live. |
| Account-ban / ToS risk of RE | **None** | We use the sanctioned API only. The #40 WhatsApp ban-risk model does not apply. |
| Rate limit (60/min, 1440/day) | Low | 2-min poll ≈ 720/day for one form; config-tunable interval; honor `x-ratelimit-*` + 429. |

### Go / No-Go

**GO — conditional.** Land the doc + gated scaffold now (this PR). Flip the
`AUGMENTAGENT_DEFT_ENABLED` gate and wire the `serve` spawn **only after**:

1. The user confirms "Deft" == Deftform and supplies a workspace token.
2. A real `GET /responses/{formId}` (and ideally one webhook "Send test")
   payload is captured to clear the §3/§4 `REQUIRES LIVE VALIDATION` flags
   and pin `DeftSubmission`.
3. The operator's actual command `custom_key`s are confirmed (§5).

No-Go conditions: user's "Deft" turns out to be a different product (re-scope
the spike), or Deftform removes the public API tier the user's plan needs.
