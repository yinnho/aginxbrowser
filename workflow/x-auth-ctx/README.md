# x-auth-ctx

Opt-in preload recipe: shared X.com txid context + web session bind helpers
for write flows (`x-reply` / `x-follow`).

Installs on `globalThis`:

| Name | Role |
|------|------|
| `abxEnsureAuthCtx` | async — ensure `abx_txid_ctx` (72h) + `abx_bind` (~3h); returns JSON string |
| `abxB64keep` | bytes → base64 |
| `abxDerEncode` | ECDSA `(r,s)` → DER `Uint8Array` |
| `abxAuthHeaders` | bearer + ct0 session headers (`content-type: application/json`) |
| `abxTxidGen` | `(method, path, ctx, animationKey)` → `x-client-transaction-id` |

Requires `globalThis.P256` (`x-p256`) already installed. Mount with
`{"recipes":["x-p256","x-bearer","x-auth-ctx"],"apply_now":true}` when
attaching to a live session. Thin eval: `await abxEnsureAuthCtx()`.
