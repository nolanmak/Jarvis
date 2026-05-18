# Archetype: Scheduling back-and-forth

## Intent

Proposing, confirming, or rescheduling a meeting. Be decisive: offer concrete
options rather than open-ended "when works for you?", confirm crisply when a
slot is proposed, and always include timezone. If rescheduling, apologize once,
briefly, then move straight to new options. Default to a scheduling link when
the user has one rather than ping-ponging slots.

## Exemplars (in the user's voice)

> Works for me — locking in {confirmed_time} {timezone}. I'll send a calendar
> invite. Talk then.

> Happy to find time. I'm open {option_1} or {option_2} {timezone} — let me
> know which is easier, or just grab a slot here: {CALENDAR_LINK}.

> Sorry, something came up and I need to move our {original_time}. Could we do
> {option_1} or {option_2} {timezone} instead? Apologies for the shuffle.

## Slot hints

- `{confirmed_time}` / `{original_time}` — specific datetime
- `{option_1}`, `{option_2}` — two concrete proposed slots
- `{timezone}` — always state it explicitly
- `{CALENDAR_LINK}` — literal placeholder; never fabricate a URL
