# NewsletterBuddy bridge (research and drafts only)

The Discord query agent can use `augmentagent newsletter` only when Jarvis has an explicit `allowed_user_id` and the incoming author matches it. The command allowlist excludes `configure`, audience approval and delivery. The token is retrieved by the CLI from the host's OS credential store; it is not forwarded into the model's environment or placed on the command line.

On the Jarvis host, set `NEWSLETTERBUDDY_URL` to the NewsletterBuddy HTTPS origin (loopback HTTP is allowed for same-host development).

For unattended operation, store the service token in a private regular file at `~/.config/augmentagent/newsletterbuddy.token` (`$XDG_CONFIG_HOME/augmentagent/newsletterbuddy.token` when configured). The file must be owned by the service user, have mode 0600 and contain a single nonempty ASCII token, optionally followed by one newline, no more than 4096 bytes. Symlinks and unsafe files are rejected. `NEWSLETTERBUDDY_TOKEN_FILE` selects an explicit path for operator CLI use. Precedence is explicit file, existing default file, then the OS credential store. An invalid selected file fails closed; it never falls back. File credentials survive process restart and reboot without an interactive unlock.

Existing OS credential configuration remains available through `augmentagent newsletter configure`, reading the token on stdin. This integration preserves the existing platform keyring backend. Never put a token in CLI arguments, model-facing environment, fixtures or Git.

The owner can ask Jarvis in Discord to create a newsletter desk, save a prompt/topic brief with optional repeated `--feed-url` RSS/Atom sources, start and inspect research, list citable evidence, label candidates useful/not useful with reason codes, inspect effective feedback/ranking, explicitly reset learned source preferences while retaining labels, create/list/read/edit/pause/resume/soft-delete daily research or draft schedules, fire a due occurrence, and generate/read a draft. The bridge derives `Idempotency-Key` from the trusted Discord channel/message ID plus operation and target, so retrying the same event does not create a second research run, feedback event, ranking reset, schedule edit or draft. The CLI accepts no caller-supplied actor ID and NewsletterBuddy derives ownership from its bearer token. A remote HTTP URL is rejected; use HTTPS. The bridge does not approve or send an edition.

Owner-created `/loop` tasks can also call these same commands. The loop runner passes the stored loop owner to the authorization gate and forwards its stable occurrence ID as the idempotency seed; loops owned by anyone else do not receive the tools. Create separate research and draft schedules in NewsletterBuddy. A daily Jarvis `/loop` invokes `schedule-run` with the corresponding schedule ID; NewsletterBuddy checks the local due time, coalesces missed days, and durably records the occurrence. Do not also configure a standalone scheduler for the same brief. A draft occurrence with insufficient evidence is recorded as skipped, not an empty edition. Jarvis owns the default clock; NewsletterBuddy owns the schedule definition and durable occurrence execution.

This is an implementation slice, not a staging sign-off. A Linux Jarvis ↔ NewsletterBuddy round trip with synthetic evidence and configured services is required before closing NewsletterBuddy NB-07.

## Draft with the Jarvis reasoner

`draft-submit --newsletter-id ID --brief-revision N --proposal-json JSON` saves a proposal written by the current Jarvis reasoner, without a separate NewsletterBuddy model account. The JSON contains `subject`, `intro`, and `items`; each item has `headline`, `summary`, and `evidenceIds` taken from the evidence response. The service validates references, checks research readiness and stores an immutable cited revision. Replaying the same trusted event is idempotent; changed content under the same request is rejected. `generate` remains available when the service has its own draft model configured. Neither command approves or sends.

## Approve and release a send (owner-operated)

Sending an edition is a two-step, owner-only path that uses a **distinct editorial credential** (`from_editorial_config`): the research bearer token cannot reach these endpoints. Configure it as a private mode-0600 file at `~/.config/augmentagent/newsletterbuddy-editorial.token` (override with `NEWSLETTERBUDDY_EDITORIAL_TOKEN_FILE`) or in the OS credential store under the `editorial` account; NewsletterBuddy must have the matching `NEWSLETTER_EDITORIAL_TOKEN_SHA256` configured.

- `approve --newsletter-id ID --draft-revision N --channel email|sms` binds the latest immutable draft hash, channel and audience snapshot and creates **held** (unsent) delivery rows. It returns the approval `id`.
- `release --newsletter-id ID --approval-id AID` moves those held rows to the queue; the delivery worker then sends them for real. Returns `{ "queued": <count> }`.
- `deliveries --newsletter-id ID --approval-id AID` reads redacted per-recipient statuses (no addresses).

`approve` and `release` are **not** exposed to the Discord reasoner — they are run by the owner from a terminal, because an approval binds an audience and a release sends real email, and the reasoner processes untrusted research content. Only the read-only `deliveries` status query is available to the reasoner. Both mutating commands derive their `Idempotency-Key` from the trusted request ID plus operation and target, so a replay of the same event does not double-approve or double-release. Set `NEWSLETTERBUDDY_REQUEST_ID` (shaped `<digits>:<digits>`, e.g. `0:$(date +%s%N)`) when running them manually.
