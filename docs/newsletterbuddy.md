# NewsletterBuddy bridge (research and drafts only)

The Discord query agent can use `augmentagent newsletter` only when Jarvis has an explicit `allowed_user_id` and the incoming author matches it. The command allowlist excludes `configure`, audience approval and delivery. The token is retrieved by the CLI from the host's OS credential store; it is not forwarded into the model's environment or placed on the command line.

On the Jarvis host, set `NEWSLETTERBUDDY_URL` to the NewsletterBuddy HTTPS origin (loopback HTTP is allowed for same-host development). Store the token manually through stdin:

```sh
printf '%s' "$NEWSLETTERBUDDY_TOKEN" | augmentagent newsletter configure
```

Do not put the token in a CLI argument or Jarvis's model-facing environment. Unset the shell variable after configuration. On Linux, this build enables kernel keyutils without a DBus dependency. Kernel keyrings do not survive reboot, so the service operator must re-provision the token before Jarvis resumes newsletter work after a host reboot. A missing credential returns a setup error rather than silently using a mock store.

The owner can ask Jarvis in Discord to create a newsletter desk, save a prompt/topic brief, start and inspect research, list citable evidence, label candidates useful/not useful with reason codes, inspect effective feedback/ranking, explicitly reset learned source preferences while retaining labels, create/list/read/edit/pause/resume/soft-delete daily research or draft schedules, fire a due occurrence, and generate/read a draft. The bridge derives `Idempotency-Key` from the trusted Discord channel/message ID plus operation and target, so retrying the same event does not create a second research run, feedback event, ranking reset, schedule edit or draft. The CLI accepts no caller-supplied actor ID and NewsletterBuddy derives ownership from its bearer token. A remote HTTP URL is rejected; use HTTPS. The bridge does not approve or send an edition.

Owner-created `/loop` tasks can also call these same commands. The loop runner passes the stored loop owner to the authorization gate and forwards its stable occurrence ID as the idempotency seed; loops owned by anyone else do not receive the tools. Create separate research and draft schedules in NewsletterBuddy. A daily Jarvis `/loop` invokes `schedule-run` with the corresponding schedule ID; NewsletterBuddy checks the local due time, coalesces missed days, and durably records the occurrence. Do not also configure a standalone scheduler for the same brief. A draft occurrence with insufficient evidence is recorded as skipped, not an empty edition. Jarvis owns the default clock; NewsletterBuddy owns the schedule definition and durable occurrence execution.

This is an implementation slice, not a staging sign-off. A Linux Jarvis ↔ NewsletterBuddy round trip with synthetic evidence and configured services is required before closing NewsletterBuddy NB-07.
