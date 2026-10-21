// CSS.registerProperty bridge to the cascade's @property registry.
use super::*;

/// The JS-side registerProperty already validated its arguments; this mirrors
/// the registration into the Rust registry so the cascade sees the initial
/// value and `inherits`. `initial` is "" when the call had none.
#[diting_op]
pub(crate) fn op_register_property(#[string] name: &str, #[string] syntax: &str, inherits: bool, #[string] initial: &str) {
    crate::diting_css::props::register_js(name, syntax, inherits, (!initial.is_empty()).then_some(initial));
}
