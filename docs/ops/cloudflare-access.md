# Cloudflare Access for jarvis.shipsystems.ai

Ops record for the Jarvis Dashboard's edge auth (spec `docs/specs/web-dashboard-v2.md`, O1; issue #2721).
Last verified: 2026-11-28.

## Where it lives

- Cloudflare account: the owner's personal account, which holds the `shipsystems.ai` zone.
- Zero Trust team domain: shown under Zero Trust → Settings. It goes into `AUGMENTAGENT_CF_ACCESS_TEAM_DOMAIN`.
  - The RDP hostname on the Legion uses a different account and team. Don't mix them up.
- Tunnel: `augmentagent-dashboard-tunnel.service` on the Legion → `http://127.0.0.1:3000`.

## Application

| Setting | Value |
|---|---|
| Name | `jarvis` (self-hosted) |
| Destination | `jarvis.shipsystems.ai` (all paths) |
| AUD tag | on the application's Overview tab; goes into `AUGMENTAGENT_CF_ACCESS_AUD` |
| Session duration | 24 hours |
| Identity provider | One-time PIN (emailed code) |
| Policies | `Nolan only` (Allow); **no Bypass or Service Auth policies** |

The `Nolan only` policy includes these emails: the owner's addresses only.

Because there's no bypass, third-party webhooks can't reach the app through this hostname. The app-side exemptions in `src/webhooks.ts` / `src/dashboard.ts` matter only for traffic that reaches `127.0.0.1:3000` some other way. (This answers spec T2.)

## MFA (Access independent MFA), enabled 2026-11-28

- Allowed methods: biometrics (platform passkey), security key, authenticator app. PIV and FIDO2 are off.
- Authentication duration: 24 hours.
- **"Apply global MFA settings by default": on.** Every Access policy requires MFA unless it explicitly overrides.
- Users enroll devices on the App Launcher account page at `https://<team-domain>`.
- **The App Launcher must have a policy (it uses `Nolan only`).** Without one, MFA enrollment dead-ends with "Please contact your administrator to enable the Access App Launcher", and enforced MFA then locks everyone out. Attach the launcher policy *before* turning on enforcement.

## App-side values (O2b, after #2708 deploys)

```
AUGMENTAGENT_CF_ACCESS_AUD=<application AUD tag>
AUGMENTAGENT_CF_ACCESS_TEAM_DOMAIN=<team>.cloudflareaccess.com
AUGMENTAGENT_CF_ACCESS_EMAILS=<the policy's emails, comma-separated>
```

The real values live only in the Legion `.env`, never in the repo.

## Rules

- Never sign in to Access from the agent-driven Chrome profile on the Legion (spec R3).
- Changing the policy's emails means updating `AUGMENTAGENT_CF_ACCESS_EMAILS` too, or the app check rejects the new address.
