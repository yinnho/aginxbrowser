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
///
/// A theme is (preset, mode): the preset picks the palette family (classic,
/// signal-flow, blueprint, editorial — archify's visual_preset dropdown),
/// the mode picks light or dark within it.
pub struct Theme {
    pub preset: &'static str,
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

    /// The presets render_markdown accepts, for error text.
    pub fn presets() -> &'static [&'static str] {
        &["classic", "signal-flow", "blueprint", "editorial"]
    }

    /// Look a (preset, mode) pair up. This is the full render_markdown
    /// surface: 4 presets × 2 modes = 8 live themes.
    pub fn resolve(preset: &str, mode: &str) -> Option<&'static Theme> {
        match (preset, mode) {
            ("classic", "light") => Some(&LIGHT),
            ("classic", "dark") => Some(&DARK),
            ("signal-flow", "light") => Some(&SIGNAL_FLOW_LIGHT),
            ("signal-flow", "dark") => Some(&SIGNAL_FLOW_DARK),
            ("blueprint", "light") => Some(&BLUEPRINT_LIGHT),
            ("blueprint", "dark") => Some(&BLUEPRINT_DARK),
            ("editorial", "light") => Some(&EDITORIAL_LIGHT),
            ("editorial", "dark") => Some(&EDITORIAL_DARK),
            _ => None,
        }
    }
}

/// The historical palette (批2-批5 output, value for value).
pub static LIGHT: Theme = Theme {
    preset: "classic",
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
    preset: "classic",
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

// ---------------------------------------------------------------------------
// Visual presets. archify ships these as runtime CSS-variable blocks; ours
// bake the same palette into constant tables (values from their template
// blocks, rgba entries composited over the page background offline because
// diting's SVG paint path needs concrete colors). classic is LIGHT/DARK
// above; these three cover the remaining presets × both modes.
// ---------------------------------------------------------------------------

/// signal-flow: cool signal blues with teal emphasis.
pub static SIGNAL_FLOW_LIGHT: Theme = Theme {
    preset: "signal-flow",
    name: "light",
    page_bg: "#f4f9fc",
    page_fg: "#102638",
    code_bg: "#fdfefe",
    border: "#bfd5e2",
    quote_fg: "#587287",
    ink: "#102638",
    ink_muted: "#587287",
    ink_soft: "#7b97aa",
    guide: "#8aa2b4",
    edge_label: "#0d9488",
    danger: "#c53a59",
    skip: "#c65f27",
    panel: "#edf5fa",
    panel_alt: "#ffffff",
    panel_danger: "#f2e7ee",
    frame: "#a9c5d5",
    frontend: NodeColors {
        fill: "#dff3f8",
        stroke: "#0789a1",
        text: "#587287",
    },
    backend: NodeColors {
        fill: "#def0ef",
        stroke: "#087f69",
        text: "#587287",
    },
    database: NodeColors {
        fill: "#e9e8fb",
        stroke: "#7254c7",
        text: "#587287",
    },
    cloud: NodeColors {
        fill: "#f2efe8",
        stroke: "#b9670b",
        text: "#587287",
    },
    security: NodeColors {
        fill: "#f2e7ee",
        stroke: "#c53a59",
        text: "#587287",
    },
    messagebus: NodeColors {
        fill: "#f3ece9",
        stroke: "#c65f27",
        text: "#587287",
    },
    neutral: NodeColors {
        fill: "#e6ecf1",
        stroke: "#607a8c",
        text: "#587287",
    },
    plain: NodeColors {
        fill: "#ffffff",
        stroke: "#8aa2b4",
        text: "#587287",
    },
};

/// signal-flow dark: deep-space navy with luminous signal inks.
pub static SIGNAL_FLOW_DARK: Theme = Theme {
    preset: "signal-flow",
    name: "dark",
    page_bg: "#030711",
    page_fg: "#f5fbff",
    code_bg: "#050c1a",
    border: "#1d3350",
    quote_fg: "#9eb0c7",
    ink: "#f5fbff",
    ink_muted: "#9eb0c7",
    ink_soft: "#7890ad",
    guide: "#52667f",
    edge_label: "#2dd4bf",
    danger: "#fda4af",
    skip: "#fdba74",
    panel: "#060f1d",
    panel_alt: "#07101e",
    panel_danger: "#220e1b",
    frame: "#2c4564",
    frontend: NodeColors {
        fill: "#03202c",
        stroke: "#67e8f9",
        text: "#9eb0c7",
    },
    backend: NodeColors {
        fill: "#052021",
        stroke: "#5eead4",
        text: "#9eb0c7",
    },
    database: NodeColors {
        fill: "#191536",
        stroke: "#c4b5fd",
        text: "#9eb0c7",
    },
    cloud: NodeColors {
        fill: "#221b10",
        stroke: "#fcd34d",
        text: "#9eb0c7",
    },
    security: NodeColors {
        fill: "#220e1b",
        stroke: "#fda4af",
        text: "#9eb0c7",
    },
    messagebus: NodeColors {
        fill: "#231512",
        stroke: "#fdba74",
        text: "#9eb0c7",
    },
    neutral: NodeColors {
        fill: "#131a26",
        stroke: "#a5b4c7",
        text: "#9eb0c7",
    },
    plain: NodeColors {
        fill: "#07101e",
        stroke: "#52667f",
        text: "#9eb0c7",
    },
};

/// blueprint: drafting-table cyan paper with structural ink.
pub static BLUEPRINT_LIGHT: Theme = Theme {
    preset: "blueprint",
    name: "light",
    page_bg: "#edf7fa",
    page_fg: "#123344",
    code_bg: "#f8fdff",
    border: "#78aabd",
    quote_fg: "#4e7486",
    ink: "#123344",
    ink_muted: "#4e7486",
    ink_soft: "#6d93a5",
    guide: "#86a6b4",
    edge_label: "#087f69",
    danger: "#b32f50",
    skip: "#b65120",
    panel: "#e2f1f6",
    panel_alt: "#f9fdff",
    panel_danger: "#eae9ee",
    frame: "#83b2c4",
    frontend: NodeColors {
        fill: "#dbeff4",
        stroke: "#087f9c",
        text: "#4e7486",
    },
    backend: NodeColors {
        fill: "#daedee",
        stroke: "#08755f",
        text: "#4e7486",
    },
    database: NodeColors {
        fill: "#e2e9f4",
        stroke: "#6757a8",
        text: "#4e7486",
    },
    cloud: NodeColors {
        fill: "#e9ede7",
        stroke: "#a86609",
        text: "#4e7486",
    },
    security: NodeColors {
        fill: "#eae9ee",
        stroke: "#b32f50",
        text: "#4e7486",
    },
    messagebus: NodeColors {
        fill: "#eaebea",
        stroke: "#b65120",
        text: "#4e7486",
    },
    neutral: NodeColors {
        fill: "#e0ecf0",
        stroke: "#506f7e",
        text: "#4e7486",
    },
    plain: NodeColors {
        fill: "#f9fdff",
        stroke: "#86a6b4",
        text: "#4e7486",
    },
};

/// blueprint dark: night drafting board with bright line-work.
pub static BLUEPRINT_DARK: Theme = Theme {
    preset: "blueprint",
    name: "dark",
    page_bg: "#06131f",
    page_fg: "#e3f6ff",
    code_bg: "#071a2a",
    border: "#27627f",
    quote_fg: "#91b8ca",
    ink: "#e3f6ff",
    ink_muted: "#91b8ca",
    ink_soft: "#78a3b7",
    guide: "#557e92",
    edge_label: "#64dfc1",
    danger: "#ff8da1",
    skip: "#ffad66",
    panel: "#071c2c",
    panel_alt: "#0a2031",
    panel_danger: "#1f1a28",
    frame: "#34799a",
    frontend: NodeColors {
        fill: "#092636",
        stroke: "#66d9ef",
        text: "#91b8ca",
    },
    backend: NodeColors {
        fill: "#09252b",
        stroke: "#69dfbd",
        text: "#91b8ca",
    },
    database: NodeColors {
        fill: "#161f36",
        stroke: "#b4a8ff",
        text: "#91b8ca",
    },
    cloud: NodeColors {
        fill: "#1f2321",
        stroke: "#ffd166",
        text: "#91b8ca",
    },
    security: NodeColors {
        fill: "#1f1a28",
        stroke: "#ff8da1",
        text: "#91b8ca",
    },
    messagebus: NodeColors {
        fill: "#201f21",
        stroke: "#ffad66",
        text: "#91b8ca",
    },
    neutral: NodeColors {
        fill: "#142532",
        stroke: "#a7cad9",
        text: "#91b8ca",
    },
    plain: NodeColors {
        fill: "#0a2031",
        stroke: "#557e92",
        text: "#91b8ca",
    },
};

/// editorial: warm paper with muted typographic ink.
pub static EDITORIAL_LIGHT: Theme = Theme {
    preset: "editorial",
    name: "light",
    page_bg: "#f2eee5",
    page_fg: "#242018",
    code_bg: "#fbf8f1",
    border: "#c4b9a6",
    quote_fg: "#6f6658",
    ink: "#242018",
    ink_muted: "#6f6658",
    ink_soft: "#8a806f",
    guide: "#a09788",
    edge_label: "#bb4c23",
    danger: "#9e463f",
    skip: "#ad4b25",
    panel: "#ede7dc",
    panel_alt: "#fbf8f1",
    panel_danger: "#eaded5",
    frame: "#b8aa94",
    frontend: NodeColors {
        fill: "#dfe3dc",
        stroke: "#287e84",
        text: "#6f6658",
    },
    backend: NodeColors {
        fill: "#e0e3d7",
        stroke: "#397b53",
        text: "#6f6658",
    },
    database: NodeColors {
        fill: "#e6dfdc",
        stroke: "#765d86",
        text: "#6f6658",
    },
    cloud: NodeColors {
        fill: "#eae1d1",
        stroke: "#9a671f",
        text: "#6f6658",
    },
    security: NodeColors {
        fill: "#eaded5",
        stroke: "#9e463f",
        text: "#6f6658",
    },
    messagebus: NodeColors {
        fill: "#eddfd3",
        stroke: "#ad4b25",
        text: "#6f6658",
    },
    neutral: NodeColors {
        fill: "#e5e1d7",
        stroke: "#746b5e",
        text: "#6f6658",
    },
    plain: NodeColors {
        fill: "#fbf8f1",
        stroke: "#a09788",
        text: "#6f6658",
    },
};

/// editorial dark: inked paper at night, warm low-light inks.
pub static EDITORIAL_DARK: Theme = Theme {
    preset: "editorial",
    name: "dark",
    page_bg: "#181611",
    page_fg: "#f4eddf",
    code_bg: "#231f18",
    border: "#625a4a",
    quote_fg: "#b9ae9b",
    ink: "#f4eddf",
    ink_muted: "#b9ae9b",
    ink_soft: "#948978",
    guide: "#776e60",
    edge_label: "#dd6b3d",
    danger: "#df9085",
    skip: "#df946f",
    panel: "#252119",
    panel_alt: "#231f18",
    panel_danger: "#2f1e19",
    frame: "#726957",
    frontend: NodeColors {
        fill: "#1b2825",
        stroke: "#7fc6c7",
        text: "#b9ae9b",
    },
    backend: NodeColors {
        fill: "#1e281d",
        stroke: "#8fc29e",
        text: "#b9ae9b",
    },
    database: NodeColors {
        fill: "#282226",
        stroke: "#c0a4d0",
        text: "#b9ae9b",
    },
    cloud: NodeColors {
        fill: "#2f2616",
        stroke: "#d8ad68",
        text: "#b9ae9b",
    },
    security: NodeColors {
        fill: "#2f1e19",
        stroke: "#df9085",
        text: "#b9ae9b",
    },
    messagebus: NodeColors {
        fill: "#301f15",
        stroke: "#df946f",
        text: "#b9ae9b",
    },
    neutral: NodeColors {
        fill: "#2a271f",
        stroke: "#b8ad99",
        text: "#b9ae9b",
    },
    plain: NodeColors {
        fill: "#231f18",
        stroke: "#776e60",
        text: "#b9ae9b",
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

    #[test]
    fn resolve_covers_every_preset_mode_combo() {
        for preset in Theme::presets() {
            for mode in Theme::names() {
                let t = Theme::resolve(preset, mode)
                    .unwrap_or_else(|| panic!("{preset}/{mode} must resolve"));
                assert_eq!(t.preset, *preset);
                assert_eq!(t.name, *mode);
            }
        }
        // Bad names reject; a cross product of known halves still rejects.
        assert!(Theme::resolve("sepia", "light").is_none());
        assert!(Theme::resolve("signal-flow", "sepia").is_none());
        assert!(Theme::resolve("", "").is_none());
    }

    #[test]
    fn presets_lead_with_the_default() {
        // classic is the default render_markdown preset and must be first.
        assert_eq!(Theme::presets()[0], "classic");
        assert_eq!(Theme::presets().len(), 4);
        assert_eq!(LIGHT.preset, "classic");
    }

    #[test]
    fn presets_dont_alias_the_classic_palette() {
        // A preset table that copied classic's values would silently render
        // identical bytes — each preset must move the page and at least one
        // kind slot in both modes.
        for other in [
            &SIGNAL_FLOW_LIGHT,
            &BLUEPRINT_LIGHT,
            &EDITORIAL_LIGHT,
        ] {
            assert_ne!(LIGHT.page_bg, other.page_bg, "{} light must move the page", other.preset);
            assert_ne!(LIGHT.ink, other.ink, "{} light must move the ink", other.preset);
            assert_ne!(
                LIGHT.frontend.stroke,
                other.frontend.stroke,
                "{} light must move kind slots",
                other.preset
            );
        }
        for other in [&SIGNAL_FLOW_DARK, &BLUEPRINT_DARK, &EDITORIAL_DARK] {
            assert_ne!(DARK.page_bg, other.page_bg, "{} dark must move the page", other.preset);
            assert_ne!(DARK.ink, other.ink, "{} dark must move the ink", other.preset);
        }
    }
}
