//! The theme layer — 批6 精美层's constant table. Every color the shell and
//! the five family adapters emit lives here; the adapters keep their
//! vocabulary (kinds, variants) and typography (sizes, dash patterns), the
//! theme owns the palette.
//!
//! Fork from archify's mechanism (deliberate): their viewer re-themes at
//! runtime via data-theme + CSS custom properties. Our artifact is a static
//! deterministic single file rendered by diting, whose SVG paint path reads
//! presentation attributes rather than CSS — so themes bake their values in
//! at generation time. `data-theme` rides along as provenance for a future
//! viewer runtime. LIGHT is the historical palette, value for value.

/// One node kind's triple: body fill, stroke, and label ink. Sequence rides
/// fill+stroke; the grid families also use the (darker) text ink.
pub struct NodeColors {
    pub fill: &'static str,
    pub stroke: &'static str,
    pub text: &'static str,
}

/// A complete palette. Same input + same theme = same bytes; the theme name
/// is part of the render input, so the receipt records it.
pub struct Theme {
    pub name: &'static str,
    // Prose shell
    pub page_bg: &'static str,
    pub page_fg: &'static str,
    pub code_bg: &'static str,
    pub border: &'static str,
    pub quote_fg: &'static str,
    // Diagram ink
    /// Strong text: titles, node labels, emphasis edge labels, band axes.
    pub ink: &'static str,
    /// Sublabels, secondary marks, return edges.
    pub ink_muted: &'static str,
    /// Default edge stroke and quiet caption text.
    pub ink_soft: &'static str,
    /// Lifelines, band divider dashes, boundary default strokes.
    pub guide: &'static str,
    /// Sequence's default message-label fill (an accent, unlike the others).
    pub edge_label: &'static str,
    /// Security/exception/danger strokes and labels.
    pub danger: &'static str,
    /// The dashed "skip"/async variant.
    pub skip: &'static str,
    /// Lane, stage, and boundary frame fills.
    pub panel: &'static str,
    /// Node body base and edge-label backgrounds.
    pub panel_alt: &'static str,
    /// Security boundary/group fills.
    pub panel_danger: &'static str,
    /// Default frame strokes (lanes, stages).
    pub frame: &'static str,
    // Node kinds
    pub frontend: NodeColors,
    pub backend: NodeColors,
    pub database: NodeColors,
    pub cloud: NodeColors,
    pub security: NodeColors,
    pub messagebus: NodeColors,
    pub neutral: NodeColors,
    /// The unlabeled default: plain body, quiet stroke (lifecycle's fallback
    /// state, sequence's unknown kind).
    pub plain: NodeColors,
}

impl Theme {
    /// Resolve a kind (or lifecycle state mapped onto a kind slot by the
    /// adapter) to its colors. Unknown kinds take the neutral slot.
    pub fn node(&self, kind: &str) -> &NodeColors {
        match kind {
            "frontend" => &self.frontend,
            "backend" => &self.backend,
            "database" => &self.database,
            "cloud" => &self.cloud,
            "security" => &self.security,
            "messagebus" => &self.messagebus,
            "plain" => &self.plain,
            _ => &self.neutral,
        }
    }

    /// Look a theme up by its render_markdown name.
    pub fn by_name(name: &str) -> Option<&'static Theme> {
        match name {
            "light" => Some(&LIGHT),
            "dark" => Some(&DARK),
            _ => None,
        }
    }

    /// The names render_markdown accepts, for error text.
    pub fn names() -> &'static [&'static str] {
        &["light", "dark"]
    }
}

/// The historical palette (批2-批5 output, value for value).
pub static LIGHT: Theme = Theme {
    name: "light",
    page_bg: "#ffffff",
    page_fg: "#18181b",
    code_bg: "#f4f4f5",
    border: "#e4e4e7",
    quote_fg: "#52525b",
    ink: "#18181b",
    ink_muted: "#71717a",
    ink_soft: "#52525b",
    guide: "#a1a1aa",
    edge_label: "#2563eb",
    danger: "#dc2626",
    skip: "#9333ea",
    panel: "#fafafa",
    panel_alt: "#ffffff",
    panel_danger: "#fef2f2",
    frame: "#d4d4d8",
    frontend: NodeColors {
        fill: "#dbeafe",
        stroke: "#2563eb",
        text: "#1e40af",
    },
    backend: NodeColors {
        fill: "#dcfce7",
        stroke: "#16a34a",
        text: "#166534",
    },
    database: NodeColors {
        fill: "#fef3c7",
        stroke: "#d97706",
        text: "#92400e",
    },
    cloud: NodeColors {
        fill: "#e0e7ff",
        stroke: "#4f46e5",
        text: "#3730a3",
    },
    security: NodeColors {
        fill: "#fee2e2",
        stroke: "#dc2626",
        text: "#991b1b",
    },
    messagebus: NodeColors {
        fill: "#f3e8ff",
        stroke: "#9333ea",
        text: "#6b21a8",
    },
    neutral: NodeColors {
        fill: "#f4f4f5",
        stroke: "#71717a",
        text: "#3f3f46",
    },
    plain: NodeColors {
        fill: "#ffffff",
        stroke: "#71717a",
        text: "#3f3f46",
    },
};

/// The dark palette: zinc-950 page, lifted inks, and the kind fills moved to
/// each hue's 950 shade with a 400-level stroke and 200-level label ink.
pub static DARK: Theme = Theme {
    name: "dark",
    page_bg: "#09090b",
    page_fg: "#e4e4e7",
    code_bg: "#18181b",
    border: "#27272a",
    quote_fg: "#a1a1aa",
    ink: "#f4f4f5",
    ink_muted: "#a1a1aa",
    ink_soft: "#d4d4d8",
    guide: "#52525b",
    edge_label: "#60a5fa",
    danger: "#f87171",
    skip: "#c084fc",
    panel: "#18181b",
    panel_alt: "#27272a",
    panel_danger: "#450a0a",
    frame: "#3f3f46",
    frontend: NodeColors {
        fill: "#172554",
        stroke: "#60a5fa",
        text: "#bfdbfe",
    },
    backend: NodeColors {
        fill: "#052e16",
        stroke: "#4ade80",
        text: "#bbf7d0",
    },
    database: NodeColors {
        fill: "#422006",
        stroke: "#fbbf24",
        text: "#fde68a",
    },
    cloud: NodeColors {
        fill: "#1e1b4b",
        stroke: "#818cf8",
        text: "#c7d2fe",
    },
    security: NodeColors {
        fill: "#450a0a",
        stroke: "#f87171",
        text: "#fecaca",
    },
    messagebus: NodeColors {
        fill: "#3b0764",
        stroke: "#c084fc",
        text: "#e9d5ff",
    },
    neutral: NodeColors {
        fill: "#27272a",
        stroke: "#a1a1aa",
        text: "#d4d4d8",
    },
    plain: NodeColors {
        fill: "#27272a",
        stroke: "#71717a",
        text: "#e4e4e7",
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn light_pins_the_historical_palette() {
        // Spot-pin the slots the adapters previously hardcoded: if any of
        // these moves, every frozen gate hash moves with it — consciously.
        assert_eq!(LIGHT.ink, "#18181b");
        assert_eq!(LIGHT.ink_muted, "#71717a");
        assert_eq!(LIGHT.ink_soft, "#52525b");
        assert_eq!(LIGHT.guide, "#a1a1aa");
        assert_eq!(LIGHT.edge_label, "#2563eb");
        assert_eq!(LIGHT.danger, "#dc2626");
        assert_eq!(LIGHT.skip, "#9333ea");
        assert_eq!(LIGHT.panel, "#fafafa");
        assert_eq!(LIGHT.panel_alt, "#ffffff");
        assert_eq!(LIGHT.panel_danger, "#fef2f2");
        assert_eq!(LIGHT.frame, "#d4d4d8");
        assert_eq!(LIGHT.frontend.fill, "#dbeafe");
        assert_eq!(LIGHT.backend.stroke, "#16a34a");
        assert_eq!(LIGHT.database.text, "#92400e");
        assert_eq!(LIGHT.neutral.fill, "#f4f4f5");
        assert_eq!(LIGHT.plain.fill, "#ffffff");
    }

    #[test]
    fn by_name_resolves_and_rejects() {
        assert_eq!(Theme::by_name("light").unwrap().name, "light");
        assert_eq!(Theme::by_name("dark").unwrap().name, "dark");
        assert!(Theme::by_name("sepia").is_none());
        // The advertised name list is exactly what resolves.
        for name in Theme::names() {
            assert!(Theme::by_name(name).is_some());
        }
    }

    #[test]
    fn dark_remaps_every_slot() {
        // A dark theme that forgot a slot would leak a light-only value into
        // a dark page — every palette slot must differ between the themes.
        assert_ne!(LIGHT.ink, DARK.ink);
        assert_ne!(LIGHT.page_bg, DARK.page_bg);
        assert_ne!(LIGHT.frontend.fill, DARK.frontend.fill);
        assert_ne!(LIGHT.neutral.stroke, DARK.neutral.stroke);
        assert_ne!(LIGHT.plain.fill, DARK.plain.fill);
    }
}
