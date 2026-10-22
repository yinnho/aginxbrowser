//! calc() / min() / max() / clamp() evaluation (god-file ratchet split from mod.rs).

use super::*;

/// A calc() intermediate value: a plain number (only legal as a `*`/`/`
/// operand) or a length carrying its px and percent parts accumulated
/// independently (percent in CSS 0-100 scale).
#[derive(Debug, Clone, Copy)]
enum CalcVal {
    Number(f32),
    Len { px: f32, percent: f32 },
}

impl CalcVal {
    fn parts(self) -> (f32, f32) {
        match self {
            CalcVal::Len { px, percent } => (px, percent),
            CalcVal::Number(_) => (0.0, 0.0),
        }
    }
}

/// Evaluate a calc() expression body (the text BETWEEN the parentheses) into
/// a [`Length`]: pure px arithmetic folds to [`Length::Px`], percent-only to
/// [`Length::Percent`], and a mix to [`Length::Calc`]. em/rem fold against
/// the font context like bare declarations. The grammar is the CSS one —
/// `+`/`-` need whitespace on both sides, `*`/`/` take one number operand,
/// parentheses and nested calc() group.
pub fn eval_calc(expr: &str, fonts: &FontCtx) -> Option<Length> {
    eval_calc_with(expr, fonts, None)
}

/// [`eval_calc`] with the percentage base known (px of the containing block):
/// percentages then fold to px, so min()/max()/clamp() over mixed units can
/// be ordered.
pub fn eval_calc_with(expr: &str, fonts: &FontCtx, base: Option<f32>) -> Option<Length> {
    let mut p = CalcParser { s: expr.as_bytes(), i: 0, fonts, base };
    let v = p.expr_value()?;
    p.skip_ws();
    if p.i != p.s.len() {
        return None;
    }
    calcval_to_length(v)
}

/// Fold a fully-evaluated calc intermediate into a [`Length`].
fn calcval_to_length(v: CalcVal) -> Option<Length> {
    match v {
        // A bare number is not a length; `calc(0)` has no unit context here.
        CalcVal::Number(_) => None,
        CalcVal::Len { px, percent: 0.0 } => Some(Length::Px(px)),
        CalcVal::Len { px: 0.0, percent } => Some(Length::Percent(percent)),
        CalcVal::Len { px, percent } => Some(Length::Calc { percent, px }),
    }
}

/// Which math function a `min(`/`max(`/`clamp(` call reduces as. clamp keeps
/// the CSS form `max(MIN, min(VAL, MAX))`.
#[derive(Debug, Clone, Copy, PartialEq)]
enum MathFn {
    Min,
    Max,
    Clamp,
}

/// Evaluate a property-level value that is one math call: `calc(…)`,
/// `min(…)`, `max(…)` or `clamp(…)`. min/max take one or more
/// comma-separated sums; clamp takes exactly three. Comparing operands
/// needs one unit family — all-px or all-percent (percents share the
/// property's reference). A call mixing px with percent, or a number with
/// either, is unorderable at parse time and drops (declaration falls out,
/// same posture as calc's mixed-fold staying symbolic).
pub fn eval_math_call(v: &str, fonts: &FontCtx) -> Option<Length> {
    eval_math_call_with(v, fonts, None)
}

pub fn eval_math_call_with(v: &str, fonts: &FontCtx, base: Option<f32>) -> Option<Length> {
    let t = v.trim();
    if t.len() >= 6 && t[..5].eq_ignore_ascii_case("calc(") && t.ends_with(')') {
        return eval_calc_with(&t[5..t.len() - 1], fonts, base);
    }
    // The Values-4 functions (round/mod/rem/abs/sign/sqrt/pow/hypot/exp/log/
    // trig) reduce through the same parser as a whole expression.
    const FNS: &[&str] = &["round(", "mod(", "rem(", "abs(", "sign(", "sqrt(", "pow(", "hypot(", "exp(", "log(", "sin(", "cos(", "tan(", "asin(", "acos(", "atan2(", "atan("];
    if t.ends_with(')') && FNS.iter().any(|f| t.len() > f.len() && t[..f.len()].eq_ignore_ascii_case(f)) {
        return eval_calc_with(t, fonts, base);
    }
    for (prefix, f) in [("min(", MathFn::Min), ("max(", MathFn::Max), ("clamp(", MathFn::Clamp)] {
        let pl = prefix.len();
        if t.len() > pl && t[..pl].eq_ignore_ascii_case(prefix) && t.ends_with(')') {
            let mut p = CalcParser { s: &t.as_bytes()[pl..], i: 0, fonts, base };
            let v = p.math_args_value(f)?;
            p.skip_ws();
            if p.i != p.s.len() {
                return None;
            }
            return calcval_to_length(v);
        }
    }
    None
}

struct CalcParser<'a> {
    s: &'a [u8],
    i: usize,
    fonts: &'a FontCtx,
    /// Percentage base in px (the containing block), when known.
    base: Option<f32>,
}

impl CalcParser<'_> {
    fn skip_ws(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn eat(&mut self, c: u8) -> bool {
        self.skip_ws();
        if self.i < self.s.len() && self.s[self.i] == c {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn starts_with_ignore_case(&self, prefix: &str) -> bool {
        let end = self.i + prefix.len();
        end <= self.s.len() && self.s[self.i..end].eq_ignore_ascii_case(prefix.as_bytes())
    }

    /// sum: term (( '+' | '-' ) term)* — both sides must carry units.
    fn expr_value(&mut self) -> Option<CalcVal> {
        let mut acc = self.term_value()?;
        loop {
            self.skip_ws();
            let plus = match self.s.get(self.i) {
                Some(b'+') => true,
                Some(b'-') => false,
                _ => break,
            };
            self.i += 1;
            let rhs = self.term_value()?;
            if matches!(acc, CalcVal::Number(_)) || matches!(rhs, CalcVal::Number(_)) {
                return None;
            }
            let (a, ap) = acc.parts();
            let (b, bp) = rhs.parts();
            acc = CalcVal::Len {
                px: if plus { a + b } else { a - b },
                percent: if plus { ap + bp } else { ap - bp },
            };
        }
        Some(acc)
    }

    /// product: factor (( '*' | '/' ) factor)*, one side a number.
    fn term_value(&mut self) -> Option<CalcVal> {
        let mut acc = self.factor_value()?;
        loop {
            self.skip_ws();
            let div = match self.s.get(self.i) {
                Some(b'*') => false,
                Some(b'/') => true,
                _ => break,
            };
            self.i += 1;
            let rhs = self.factor_value()?;
            let (n, val) = match (acc, rhs) {
                (CalcVal::Number(n), v) => (n, v),
                (v, CalcVal::Number(n)) => (n, v),
                _ => return None,
            };
            acc = match val {
                CalcVal::Len { px, percent } if !div => CalcVal::Len { px: px * n, percent: percent * n },
                CalcVal::Len { px, percent } if div && n != 0.0 => CalcVal::Len { px: px / n, percent: percent / n },
                _ => return None,
            };
        }
        Some(acc)
    }

    /// Parse and reduce the argument list of a math call (`i` sits just
    /// past the function name's `(`): sum (`,` sum)* `)`, one or more for
    /// min/max, exactly three for clamp.
    fn math_args_value(&mut self, f: MathFn) -> Option<CalcVal> {
        let mut args = Vec::new();
        loop {
            args.push(self.expr_value()?);
            if !self.eat(b',') {
                break;
            }
        }
        if !self.eat(b')') {
            return None;
        }
        if args.is_empty() || (f == MathFn::Clamp && args.len() != 3) {
            return None;
        }
        // One unit family only (see eval_math_call): all-px or all-percent.
        let all_px = args.iter().all(|a| matches!(a, CalcVal::Len { percent: 0.0, .. }));
        let all_pct = args.iter().all(|a| matches!(a, CalcVal::Len { px: 0.0, .. }));
        if !(all_px || all_pct) {
            return None;
        }
        let key = |a: &CalcVal| -> f32 {
            match a {
                CalcVal::Number(n) => *n,
                CalcVal::Len { px, percent } => {
                    if all_px {
                        *px
                    } else {
                        *percent
                    }
                }
            }
        };
        let pick = |lo: bool, a: &CalcVal, b: &CalcVal| {
            let (x, y) = (key(a), key(b));
            let take_b = if lo { y < x } else { y > x };
            if take_b { *b } else { *a }
        };
        let mut acc = args[0];
        match f {
            MathFn::Min | MathFn::Max => {
                let lo = f == MathFn::Min;
                for a in &args[1..] {
                    acc = pick(lo, &acc, a);
                }
            }
            MathFn::Clamp => {
                let (min, val, max) = (&args[0], &args[1], &args[2]);
                // max(MIN, min(VAL, MAX))
                let inner = pick(true, val, max);
                acc = pick(false, min, &inner);
            }
        }
        Some(acc)
    }

    fn factor_value(&mut self) -> Option<CalcVal> {
        self.skip_ws();
        if self.eat(b'(') {
            let v = self.expr_value()?;
            return self.eat(b')').then_some(v);
        }
        if self.starts_with_ignore_case("calc(") {
            self.i += 5;
            let v = self.expr_value()?;
            return self.eat(b')').then_some(v);
        }
        for (prefix, f) in [("min(", MathFn::Min), ("max(", MathFn::Max), ("clamp(", MathFn::Clamp)] {
            if self.starts_with_ignore_case(prefix) {
                self.i += prefix.len();
                return self.math_args_value(f);
            }
        }
        if let Some(v) = self.math_function() {
            return v;
        }
        // number | length token. '+/-/"/("/")/',' end a token but may START
        // one (a signed number), so only delimit when past the first byte.
        let start = self.i;
        while self.i < self.s.len() {
            let c = self.s[self.i];
            if self.i > start
                && (c.is_ascii_whitespace() || matches!(c, b'+' | b'-' | b'*' | b'/' | b'(' | b')' | b','))
            {
                break;
            }
            self.i += 1;
        }
        if self.i == start {
            return None;
        }
        let tok = std::str::from_utf8(&self.s[start..self.i]).ok()?;
        if let Ok(n) = tok.parse::<f32>() {
            return n.is_finite().then_some(CalcVal::Number(n));
        }
        match tok.to_ascii_lowercase().as_str() {
            "pi" => return Some(CalcVal::Number(std::f32::consts::PI)),
            "e" => return Some(CalcVal::Number(std::f32::consts::E)),
            _ => {}
        }
        if let Some(rad) = angle_radians(tok) {
            return Some(CalcVal::Number(rad));
        }
        let resolved = parse_css_length(tok).map(|l| resolve_len(l, self.fonts))?;
        match resolved {
            Length::Px(px) => Some(CalcVal::Len { px, percent: 0.0 }),
            Length::Percent(p) => Some(match self.base {
                Some(b) => CalcVal::Len { px: p * b / 100.0, percent: 0.0 },
                None => CalcVal::Len { px: 0.0, percent: p },
            }),
            _ => None,
        }
    }
}

/// `90deg` / `0.5turn` / `1rad` / `100grad` as radians.
fn angle_radians(tok: &str) -> Option<f32> {
    let l = tok.to_ascii_lowercase();
    for (unit, per_unit) in [
        ("deg", std::f32::consts::PI / 180.0),
        ("grad", std::f32::consts::PI / 200.0),
        ("rad", 1.0),
        ("turn", std::f32::consts::TAU),
    ] {
        if let Some(n) = l.strip_suffix(unit) {
            return n.parse::<f32>().ok().filter(|v| v.is_finite()).map(|v| v * per_unit);
        }
    }
    None
}

impl CalcParser<'_> {
    /// Comma-separated sums up to the closing `)` (cursor just past `name(`).
    fn call_args(&mut self) -> Option<Vec<CalcVal>> {
        let mut args = Vec::new();
        loop {
            args.push(self.expr_value()?);
            if !self.eat(b',') {
                break;
            }
        }
        self.eat(b')').then_some(args)
    }

    /// Combine two operands of one kind: numbers with numbers, lengths
    /// within one unit family (all-px or all-percent).
    fn pair(a: CalcVal, b: CalcVal, f: impl Fn(f32, f32) -> f32) -> Option<CalcVal> {
        match (a, b) {
            (CalcVal::Number(x), CalcVal::Number(y)) => Some(CalcVal::Number(f(x, y))),
            (CalcVal::Len { px: x, percent: 0.0 }, CalcVal::Len { px: y, percent: 0.0 }) => {
                Some(CalcVal::Len { px: f(x, y), percent: 0.0 })
            }
            (CalcVal::Len { px: 0.0, percent: x }, CalcVal::Len { px: 0.0, percent: y }) => {
                Some(CalcVal::Len { px: 0.0, percent: f(x, y) })
            }
            _ => None,
        }
    }

    fn scalar(v: CalcVal) -> Option<f32> {
        match v {
            CalcVal::Number(n) => Some(n),
            _ => None,
        }
    }

    /// The CSS Values 4 math functions beyond calc/min/max/clamp. `None`
    /// when the cursor is not at one (the caller keeps scanning); `Some(None)`
    /// when it is one but its arguments do not reduce.
    fn math_function(&mut self) -> Option<Option<CalcVal>> {
        const NAMES: &[&str] = &[
            "round", "mod", "rem", "abs", "sign", "sqrt", "pow", "hypot", "exp", "log", "sin", "cos", "tan", "asin",
            "acos", "atan2", "atan",
        ];
        let name = NAMES.iter().find(|n| {
            self.starts_with_ignore_case(n)
                && self.s.get(self.i + n.len()) == Some(&b'(')
        })?;
        self.i += name.len() + 1;
        let strategy = if *name == "round" {
            self.skip_ws();
            ["up", "down", "to-zero", "nearest"].iter().find(|st| {
                self.starts_with_ignore_case(st) && self.s.get(self.i + st.len()).is_some_and(|c| *c == b',')
            }).map(|st| {
                self.i += st.len() + 1;
                *st
            })
        } else {
            None
        };
        let Some(args) = self.call_args() else { return Some(None) };
        let a = args[0];
        let out = match (*name, args.len()) {
            ("round", 2) => {
                let strat = strategy.unwrap_or("nearest");
                Self::pair(a, args[1], |x, step| {
                    if step == 0.0 {
                        return f32::NAN;
                    }
                    let q = x / step;
                    let r = match strat {
                        "up" => q.ceil(),
                        "down" => q.floor(),
                        "to-zero" => q.trunc(),
                        _ => q.round(),
                    };
                    r * step
                })
            }
            ("mod", 2) => Self::pair(a, args[1], |x, y| if y == 0.0 { f32::NAN } else { x - y * (x / y).floor() }),
            ("rem", 2) => Self::pair(a, args[1], |x, y| if y == 0.0 { f32::NAN } else { x - y * (x / y).trunc() }),
            ("abs", 1) => match a {
                CalcVal::Number(n) => Some(CalcVal::Number(n.abs())),
                CalcVal::Len { px, percent } => Some(CalcVal::Len { px: px.abs(), percent: percent.abs() }),
            },
            ("sign", 1) => {
                let v = match a {
                    CalcVal::Number(n) => n,
                    CalcVal::Len { px, percent } => if px != 0.0 { px } else { percent },
                };
                Some(CalcVal::Number(if v == 0.0 { 0.0 } else { v.signum() }))
            }
            ("sqrt", 1) => Self::scalar(a).filter(|n| *n >= 0.0).map(|n| CalcVal::Number(n.sqrt())),
            ("exp", 1) => Self::scalar(a).map(|n| CalcVal::Number(n.exp())),
            ("log", 1) => Self::scalar(a).filter(|n| *n > 0.0).map(|n| CalcVal::Number(n.ln())),
            ("log", 2) => match (Self::scalar(a), Self::scalar(args[1])) {
                (Some(x), Some(b)) if x > 0.0 && b > 0.0 => Some(CalcVal::Number(x.ln() / b.ln())),
                _ => None,
            },
            ("pow", 2) => match (Self::scalar(a), Self::scalar(args[1])) {
                (Some(x), Some(y)) => Some(CalcVal::Number(x.powf(y))),
                _ => None,
            },
            ("hypot", n) if n >= 1 => {
                let sq = |v: CalcVal| Self::pair(v, v, |x, y| x * y);
                let mut total = sq(a);
                for v in &args[1..] {
                    let s = sq(*v);
                    total = match (total, s) {
                        (Some(t), Some(s)) => Self::pair(t, s, |x, y| x + y),
                        _ => None,
                    };
                }
                match total? {
                    CalcVal::Number(n) => Some(CalcVal::Number(n.sqrt())),
                    CalcVal::Len { px, percent } => Some(CalcVal::Len { px: px.sqrt(), percent: percent.sqrt() }),
                }
            }
            ("sin", 1) => Self::scalar(a).map(|n| CalcVal::Number(n.sin())),
            ("cos", 1) => Self::scalar(a).map(|n| CalcVal::Number(n.cos())),
            ("tan", 1) => Self::scalar(a).map(|n| CalcVal::Number(n.tan())),
            ("asin", 1) => Self::scalar(a).map(|n| CalcVal::Number(n.asin())),
            ("acos", 1) => Self::scalar(a).map(|n| CalcVal::Number(n.acos())),
            ("atan", 1) => Self::scalar(a).map(|n| CalcVal::Number(n.atan())),
            ("atan2", 2) => Self::pair(a, args[1], |y, x| y.atan2(x)).map(|v| match v {
                CalcVal::Len { px, percent } => CalcVal::Number(if px != 0.0 { px } else { percent }),
                n => n,
            }),
            _ => None,
        };
        Some(out.filter(|v| match v {
            CalcVal::Number(n) => n.is_finite(),
            CalcVal::Len { px, percent } => px.is_finite() && percent.is_finite(),
        }))
    }
}

/// Units `parse_css_length` does not spell out: the absolute family
/// (cm/mm/q/in/pt/pc fold to px at 96dpi) and the viewport variants
/// (small/large/dynamic are all the one static viewport here; vi/vb follow
/// the horizontal writing mode).
pub(super) fn parse_extra_unit(v: &str) -> Option<CssLength> {
    let l = v.to_ascii_lowercase();
    for (unit, mk) in [
        ("dvw", CssLength::Vw as fn(f32) -> CssLength),
        ("svw", CssLength::Vw),
        ("lvw", CssLength::Vw),
        ("vi", CssLength::Vw),
        ("dvh", CssLength::Vh),
        ("svh", CssLength::Vh),
        ("lvh", CssLength::Vh),
        ("vb", CssLength::Vh),
        ("vmin", CssLength::Vmin),
        ("vmax", CssLength::Vmax),
    ] {
        if let Some(n) = l.strip_suffix(unit).and_then(|n| n.css_f32()) {
            return Some(mk(n));
        }
    }
    // Absolute units after the viewport ones: "vmin" ends in "in".
    for (unit, px) in [("cm", 96.0 / 2.54), ("mm", 96.0 / 25.4), ("q", 96.0 / 101.6), ("in", 96.0), ("pt", 96.0 / 72.0), ("pc", 16.0)] {
        if let Some(n) = l.strip_suffix(unit).and_then(|n| n.css_f32()) {
            return Some(CssLength::Px(n * px));
        }
    }
    None
}

/// Rewrite every `<number><unit>` literal whose unit is a plain length
/// (em/rem/vw/vh/…) to px, leaving `%` symbolic: the layout engine later
/// evaluates the expression with the containing-block width as the percent
/// base and no font context.
fn px_normalize(expr: &str, fonts: &FontCtx) -> String {
    let b = expr.as_bytes();
    let mut out = String::with_capacity(expr.len());
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let prev_ident = i > 0 && (b[i - 1].is_ascii_alphabetic() || b[i - 1] == b'-' || b[i - 1] == b'_');
        if (c.is_ascii_digit() || (c == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit))) && !prev_ident {
            let mut j = i;
            while j < b.len() && (b[j].is_ascii_digit() || b[j] == b'.') {
                j += 1;
            }
            let num_end = j;
            while j < b.len() && (b[j].is_ascii_alphabetic() || b[j] == b'%') {
                j += 1;
            }
            let tok = &expr[i..j];
            if j > num_end {
                if let Some(Length::Px(px)) = parse_css_length(tok).map(|l| resolve_len(l, fonts)) {
                    out.push_str(&format!("{px}px"));
                    i = j;
                    continue;
                }
            }
            out.push_str(tok);
            i = j;
            continue;
        }
        out.push(c as char);
        i += 1;
    }
    out
}

/// A math call over `%` and absolute lengths that the parse-time evaluator
/// cannot order (`min(100%, 600px)`, `clamp(300px, 50%, 800px)`,
/// `round(…, 5%)`): a placeholder length that makes layout schedule the
/// repair pass, plus the px-normalised expression to evaluate there.
pub(super) fn pct_math(v: &str, fonts: &FontCtx) -> Option<(Length, std::sync::Arc<str>)> {
    let t = v.trim();
    if !t.contains('%') {
        return None;
    }
    let l = t.to_ascii_lowercase();
    if !["calc(", "min(", "max(", "clamp(", "round(", "mod(", "rem(", "abs("].iter().any(|p| l.starts_with(p)) {
        return None;
    }
    let norm = px_normalize(t, fonts);
    // Valid iff it evaluates against some base; the real base arrives at layout.
    eval_math_call_with(&norm, fonts, Some(1000.0))?;
    Some((Length::Calc { percent: 100.0, px: 0.0 }, norm.into()))
}

/// Evaluate a [`pct_math`] expression with the containing-block width (px)
/// as the percent base.
pub fn eval_width_expr(expr: &str, base: f32) -> Option<f32> {
    match eval_math_call_with(expr, &FontCtx::default(), Some(base))? {
        Length::Px(px) => Some(px),
        _ => None,
    }
}
