# xhs-qrcode

Opt-in preload recipe. Source: `recipe.js` (qrcode-generator 1.4.4, same bytes as
the former `workflow/xhs-login/qrcode.min.js` + a `globalThis.qrcode` bind).

Used by `xhs-login` so the SVG QR step does not inline ~20KB of library into
`flow.json`. Mount via `{"recipe":"xhs-qrcode"}` / `recipes:[…]` before navigate
(or with `apply_now` on an already-open document).
