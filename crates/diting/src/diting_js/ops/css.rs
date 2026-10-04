// CSS.supports bridge — split out of the god file (ratchet).
use super::*;

// CSS.supports bridge. The JS-side CSS object used to stub supports() to
// `false` for everything, which made feature-detecting libraries (doudian's
// SSR shell among them) take fallback render branches that diverged from the
// server HTML — React hydration then failed with #418 and discarded the SSR
// DOM (#203). Route to the same evaluator the @supports cascade uses.
#[op2(fast)]
pub(crate) fn op_css_supports(#[string] condition: &str) -> bool {
    crate::diting_css::supports_condition_applies(condition)
}
