# config/ — runtime data next to the engine

## `verdict_rules.json`

Platform **login-front** URL needles for `session_verdict` / flow `verdict`.

- **Core (Rust)** keeps Ali TMD / punish hosts (`risk_markers`) and generic
  captcha interstitials (Cloudflare, Google sorry, Baidu antispider, …).
- **This file** lists site login shapes (taobao / xhs / zhihu / …). Edit or
  replace it to teach new sites — no rebuild required when the process can
  read the file from disk.

Load order (first file that parses wins as the *overlay*; core always runs
first so challenge/captcha cannot be shadowed by a login needle):

1. `AGINXBROWSER_VERDICT_RULES` — absolute or relative path
2. `./config/verdict_rules.json` (cwd)
3. embedded copy baked at compile time from this repo path

## P2 note

`scripts/xhs-qr-login.py` is still the QR helper; a future
`workflow/xhs-login/` should absorb it (create bare → preload recipe →
navigate login → QR handoff), same shape as `taobao-login` / `doudian-login`.
