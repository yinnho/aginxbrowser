# x-read

Two read modes over the GraphQL API, sharing one `bearer` prefix step (all
`eval`):

- `{screen_name}` → **user mode**: `UserByScreenName` — profile with counts
  and, most usefully, `relationship_perspectives` (is the logged-in account
  following / followed by / blocking / muting them).
- `{tweet_id}` → **tweet mode**: `TweetDetail` on the focal tweet — the root
  tweet plus up to 20 thread replies, and `mine_in_thread` (our own replies
  that landed in the thread; the only reliable way to verify a reply to a
  stranger actually posted — UserTweetsAndReplies lags).

## Run

```bash
# user mode
curl -sS -X POST http://127.0.0.1:8129/flow/run -H 'Content-Type: application/json' -d '{
  "name": "x-read",
  "session_id": "s_8",
  "vars": { "args_json": "{\"screen_name\": \"obscura_sh\"}" }
}'

# tweet mode
curl -sS -X POST http://127.0.0.1:8129/flow/run -H 'Content-Type: application/json' -d '{
  "name": "x-read",
  "session_id": "s_8",
  "vars": { "args_json": "{\"tweet_id\": \"2095168688643293458\"}" }
}'
```

Saved output (`read`): user mode → `{ok, mode:"user", user:{id,
screen_name, created_at, name, verified, location, bio, followers, following,
tweets, persp}}`; tweet mode → `{ok, mode:"tweet", root:{...}, replies:[...],
mine_in_thread:[...]}`.

## Schema notes (new-shape responses)

- Counts live at `relationship_counts.{followers,following}` — **not**
  `followers_count` (that's the legacy key; first version of this flow read
  it and got nulls).
- User identity: `core.{name,screen_name}`, `rest_id`; bio at
  `profile_bio.description`; location at `location.location`.
- Follow state: `relationship_perspectives.{following,followed_by,blocking,
  muting}` — authoritative, unlike the `following` echo in v1.1 friendships
  responses (see `x-follow`).
- Tweet mode seeds `localStorage.abx_uid` from the flow's `uid` var
  (default: the @aginxbrowser rest_id) to identify "mine" in the thread.
- `uid` is a flow var, not part of `args_json` — override it in `vars` when
  running the flow for a different account.
