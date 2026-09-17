# NewsletterBuddy bridge (draft-only)

The Discord query agent can use `augmentagent newsletter` only when Jarvis has an explicit `allowed_user_id` and the incoming author matches it. The command allowlist excludes `configure`, audience approval and delivery. The token is retrieved by the CLI from the host's OS credential store; it is not forwarded into the model's environment or placed on the command line.

On the Jarvis host, set `NEWSLETTERBUDDY_URL` to the NewsletterBuddy HTTPS origin (loopback HTTP is allowed for same-host development). Store the token manually through stdin:

```sh
printf '%s' "$NEWSLETTERBUDDY_TOKEN" | augmentagent newsletter configure
```

Do not put the token in a CLI argument or Jarvis's model-facing environment. Unset the shell variable after configuration. On Linux, this build enables the persistent kernel keyring/Secret Service backend; the service account must have a functioning keyring session. A missing credential returns a setup error rather than silently using a mock store.

The owner can ask Jarvis in Discord to create a newsletter desk, save a prompt/topic brief, start and inspect research, list citable evidence, and generate/read a draft. The bridge derives `Idempotency-Key` from the trusted Discord channel/message ID plus operation and target, so retrying the same event does not create a second research run or draft. The CLI accepts no caller-supplied actor ID and NewsletterBuddy derives ownership from its bearer token. A remote HTTP URL is rejected; use HTTPS. The bridge does not approve or send an edition.

This is an implementation slice, not a staging sign-off. A Linux Jarvis ↔ NewsletterBuddy round trip with synthetic evidence and configured services is required before closing NewsletterBuddy NB-07.
