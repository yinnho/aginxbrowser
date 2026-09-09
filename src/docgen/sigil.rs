//! The SIGIL system — 16×16 semantic icons stamped into node corners.
//!
//! Vocabulary from archify (MIT): a small shape table keyed by semantic kind
//! (frontend/backend/... plus lifecycle's start/active/waiting/success/
//! failure), a tone table mapping each shape onto a palette slot, and one
//! emission: a scaled group (11/16) at the node corner, stroked in the kind's
//! slot color at the reference's 0.76 opacity.
//!
//! Deviations from the reference geometry, all forced by the diting v1 SVG
//! path parser and recorded here so the diff stays auditable:
//! - cloud: the three `a` arc segments are pre-lifted to cubic beziers
//!   (endpoint→center parameterization, ≤90° splits) — diting draws `A` as
//!   an endpoint chord, which would flatten the cloud into a hexagon;
//! - database: both `s` smooth-cubic segments are expanded to explicit `c`
//!   (reflection of the previous control point, which precedes each) —
//!   parse_path has no `S` arm and silently stops on one;
//! - `class="sigil-fill"` becomes a ` data-fill` marker here, expanded to
//!   the tone color with `stroke="none"`;
//! - the reference CSS sets stroke-width 1.35 on elements inside the
//!   scale(0.6875) group; a real browser scales that by the CTM to ~0.93px.
//!   diting does not scale stroke width by group transforms, so the
//!   effective 0.93 is baked directly.

use super::theme::Theme;

/// The kind → shape lookup: the 13 kinds with geometry, everything else
/// (including unknown kinds) takes the neutral square.
fn shape_name(kind: &str) -> &'static str {
    const SHAPES: [&str; 13] = [
        "frontend", "backend", "database", "cloud", "security", "messagebus", "external",
        "start", "active", "waiting", "success", "failure", "neutral",
    ];
    SHAPES.iter().find(|&&s| s == kind).copied().unwrap_or("neutral")
}

/// Shape → tone slot (reference SIGIL_TONE): lifecycle's state types borrow
/// their phase color, neutral and unknown ride the external slot.
fn tone(shape: &str) -> &'static str {
    match shape {
        "frontend" | "start" => "frontend",
        "backend" | "active" => "backend",
        "database" | "success" => "database",
        "cloud" | "waiting" => "cloud",
        "security" | "failure" => "security",
        "messagebus" => "messagebus",
        _ => "external",
    }
}

/// The 16×16 geometry per shape. ` data-fill` marks filled sub-elements.
fn geometry(shape: &str) -> &'static str {
    match shape {
        "frontend" => {
            "<rect x=\"2\" y=\"3\" width=\"12\" height=\"10\" rx=\"2\"/>\
             <path d=\"M2 6.5h12\"/>\
             <circle cx=\"4.1\" cy=\"4.8\" r=\".7\" data-fill/>\
             <circle cx=\"6.3\" cy=\"4.8\" r=\".7\" data-fill/>"
        }
        "backend" => "<path d=\"M6 3 3 8l3 5M10 3l3 5-3 5\"/>",
        "database" => {
            "<ellipse cx=\"8\" cy=\"4\" rx=\"5\" ry=\"2\"/>\
             <path d=\"M3 4v8c0 1.1 2.2 2 5 2c2.8 0 5-.9 5-2V4M3 8c0 1.1 2.2 2 5 2c2.8 0 5-.9 5-2\"/>"
        }
        "cloud" => {
            "<path d=\"M4.3 12.5h7.3C12.93 12.56 14.04 11.53 14.1 10.2C14.16 8.87 13.13 7.76 \
             11.8 7.7C11.56 5.98 10.23 4.62 8.52 4.32C6.81 4.03 5.11 4.86 4.3 6.4C2.83 6.67 \
             1.75 7.95 1.75 9.45C1.75 10.95 2.83 12.23 4.3 12.5Z\"/>"
        }
        "security" => {
            "<path d=\"M8 2.2 13 4v3.5c0 3.1-1.8 5.4-5 6.5-3.2-1.1-5-3.4-5-6.5V4Z\"/>\
             <path d=\"m5.8 8 1.5 1.5 3-3\"/>"
        }
        "messagebus" => {
            "<path d=\"M2.5 4.5h11M2.5 8h11M2.5 11.5h11\"/>\
             <circle cx=\"5\" cy=\"4.5\" r=\"1\" data-fill/>\
             <circle cx=\"10.5\" cy=\"8\" r=\"1\" data-fill/>\
             <circle cx=\"7\" cy=\"11.5\" r=\"1\" data-fill/>"
        }
        "external" => {
            "<rect x=\"2.5\" y=\"5\" width=\"8.5\" height=\"8\" rx=\"1.5\"/>\
             <path d=\"M8 2.5h5.5V8M13.5 2.5 7.5 8.5\"/>"
        }
        "start" => {
            "<circle cx=\"8\" cy=\"8\" r=\"5\"/>\
             <path d=\"m7 5.4 3.6 2.6L7 10.6Z\" data-fill/>"
        }
        "active" => "<path d=\"M2 8h3l1.5-3.5L9 12l1.6-4H14\"/>",
        "waiting" => {
            "<path d=\"M4 2.5h8M4 13.5h8M5 3c0 2.8 2 3.2 3 5-1 1.8-3 2.2-3 \
             5M11 3c0 2.8-2 3.2-3 5 1 1.8 3 2.2 3 5\"/>"
        }
        "success" => {
            "<circle cx=\"8\" cy=\"8\" r=\"5.3\"/>\
             <path d=\"m5.2 8 1.8 1.8 3.8-4\"/>"
        }
        "failure" => {
            "<circle cx=\"8\" cy=\"8\" r=\"5.3\"/>\
             <path d=\"m5.7 5.7 4.6 4.6m0-4.6-4.6 4.6\"/>"
        }
        _ => {
            "<rect x=\"3\" y=\"3\" width=\"10\" height=\"10\" rx=\"2\"/>\
             <circle cx=\"8\" cy=\"8\" r=\"1.2\" data-fill/>"
        }
    }
}

/// The tone color with the reference's 0.76 opacity baked as an alpha byte
/// (194 = 0xc2) — diting paints source-over, so a concrete 8-digit hex is
/// both renderable there and faithful to the reference's rgba look.
fn tone_color(theme: &'static Theme, shape: &str) -> String {
    format!("{}c2", theme.node(tone(shape)).stroke)
}

/// Emit the sigil for `kind` at node-box corner (`x`, `y`), both in tenths
/// of a pixel. The group scales the 16-unit geometry to 11 px.
pub fn sigil(kind: &str, x: i32, y: i32, theme: &'static Theme) -> String {
    let shape = shape_name(kind);
    let color = tone_color(theme, shape);
    let fill = format!(" fill=\"{color}\" stroke=\"none\"");
    let body = geometry(shape).replace(" data-fill", &fill);
    format!(
        "<g data-sigil=\"{shape}\" transform=\"translate({} {}) scale(0.6875)\" \
         fill=\"none\" stroke=\"{color}\" stroke-width=\"0.93\" \
         stroke-linecap=\"round\" stroke-linejoin=\"round\">{body}</g>",
        super::tx(x),
        super::tx(y)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shape_emits_a_scaled_group() {
        for kind in [
            "frontend",
            "backend",
            "database",
            "cloud",
            "security",
            "messagebus",
            "external",
            "start",
            "active",
            "waiting",
            "success",
            "failure",
            "neutral",
        ] {
            let out = sigil(kind, 60, 60, &super::super::theme::LIGHT);
            assert!(
                out.starts_with(&format!("<g data-sigil=\"{kind}\"")),
                "{kind}: wrong stamp"
            );
            assert!(out.contains("scale(0.6875)"), "{kind}: no scale");
            assert!(out.contains("translate(6 6)"), "{kind}: wrong origin");
            assert!(!out.contains(" data-fill"), "{kind}: marker leaked");
        }
    }

    #[test]
    fn unknown_kind_falls_back_to_the_neutral_shape() {
        let out = sigil("quantum", 60, 60, &super::super::theme::LIGHT);
        assert!(out.starts_with("<g data-sigil=\"neutral\""));
    }

    #[test]
    fn tones_borrow_their_phase_slot() {
        // start rides the frontend slot, failure the security slot — with the
        // 0.76 alpha baked as the trailing byte.
        let light = &super::super::theme::LIGHT;
        assert!(sigil("start", 0, 0, light).contains(&format!("stroke=\"{}c2\"", light.frontend.stroke)));
        assert!(sigil("failure", 0, 0, light).contains(&format!("stroke=\"{}c2\"", light.security.stroke)));
        // neutral itself rides external, which is the neutral slot here.
        assert!(sigil("neutral", 0, 0, light).contains(&format!("stroke=\"{}c2\"", light.neutral.stroke)));
    }

    #[test]
    fn normalized_geometry_avoids_unsupported_commands() {
        // cloud's arcs and database's smooth cubics are the two shapes
        // normalized offline; neither `A`/`a` nor `S`/`s` may survive.
        for kind in ["cloud", "database"] {
            let body = geometry(kind);
            for bad in ["a2", "A3", "s5", "S"] {
                assert!(!body.contains(bad), "{kind}: leaked {bad:?}");
            }
        }
    }
}
