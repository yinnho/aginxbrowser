# xhs-sign — document-start signing + logout fuse

Opt-in preload recipe (`{"recipe":"xhs-sign"}` / alias `xhs`).
Source of truth: `recipe.js` (compiled into the binary via `include_str!`
from `src/session/preload_recipes.rs` — edit here, rebuild).

Flows that mount it: `xhs-post`, `xhs-login`, (future) `xhs-merchant-publish`.
Core does **not** auto-mount by host.
