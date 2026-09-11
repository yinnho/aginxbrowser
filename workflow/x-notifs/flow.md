# x-notifs

Pull the X notifications feed and triage it. Read-only — two `eval` steps:
`bearer` (shared refresh) → `notifs`.

Uses the **v2 URT REST endpoint** (`GET /i/api/2/notifications/{tab}.json`),
not GraphQL — notifications are the one X surface still on the old URT
family (mined from `main.js`: `fetchNotifications` → `getURT('notifications/…')`).
No txid/session-signature headers needed for reads.

## Run

```bash
curl -sS -X POST http://127.0.0.1:8129/flow/run -H 'Content-Type: application/json' -d '{
  "name": "x-notifs",
  "session_id": "s_8",
  "vars": { "args_json": "{\"tab\": \"all\", \"count\": 40, \"top\": 20}" }
}'
```

`args_json`: `{"tab": "all"|"mentions", "count": 40, "top": 20}`. Saved output
(`notifs`): `{ok, n, counts, actionable_n, items:[{kind, text, from, from_n,
tweet_id, ts}]}` where `from` resolves screen name + follower count (an array
for aggregate notifications), and `actionable_n` counts mentions/replies —
the items that might deserve a human/agent response.

## Triage policy (what to do with the output)

- **mention / reply** (`actionable_n > 0`) — someone is talking to us. Read
  the tweet (`x-read {tweet_id}` for context), draft a technical reply.
  Posting still goes through `x-reply` with user sign-off per the exposure
  rule — never auto-post from this flow.
- **follow** — informational. Red flags for impersonator/scam accounts: ≤10
  followers + display name of a known brand/person + ✪/✨-style symbols (the
  "Obscura Help Desk✪" family follows this shape exactly). Don't follow
  back; surface to the user for a block decision.
- **like / retweet** — pure social proof, no action.
- **bell_icon / system** — X's own notices ("new post notifications for
  ..."), ignore.

Empty `mentions` tab just means nobody has replied/mentioned us yet — an
outbound-only campaign produces exactly that.

## Scheduling

This flow is designed to run on a schedule (cron). Notification volume is
low; once daily is enough. Keep in mind a scheduled job needs a live logged-in
session — if `session_id` errors out, re-establish per the takeover doc
(six-cookie style B), then re-run.
