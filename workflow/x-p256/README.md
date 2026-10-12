# x-p256

Opt-in preload recipe: in-page P-256 (BigInt / Jacobian) for X write flows.
Engine `crypto.subtle` lacks the ECDSA bits these flows need.

Shared by `x-reply` / `x-follow`. Mount with `{"recipe":"x-p256","apply_now":true}`
when attaching to an already-navigated session (preload alone only arms the
*next* document). Installs `globalThis.P256`.
