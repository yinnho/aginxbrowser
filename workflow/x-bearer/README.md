# x-bearer

Opt-in preload recipe: shared X.com bearer probe for read/write flows.

Installs `globalThis.abxProbeBearer` — refreshes `localStorage.abx_bearer`
from the live `main.*.js` when older than 12h (cache hit otherwise).

Shared by `x-reply` / `x-follow` / `x-notifs` / `x-search` / `x-read`.
Mount with `{"recipe":"x-bearer","apply_now":true}` (or in a `recipes:[…]`
list alongside `x-p256`) when attaching to an already-navigated session.
Call via a thin eval: `await abxProbeBearer()` (saved as `bearer`).
