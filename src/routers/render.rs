//! POST /render_markdown — markdown in, deterministic self-contained HTML
//! artifact out (the document layer, adapted from archify, MIT). The
//! parameters mirror what the removed MCP tool exposed; artifact bytes and
//! the receipt are unchanged.

use axum::extract::Json;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};

use crate::{spawn_blocking, AppError};

#[derive(Debug, Deserialize)]
pub(crate) struct RenderMarkdownRequest {
    /// The markdown document. Prose plus typed archify fenced blocks
    /// (sequence/workflow/architecture/dataflow/lifecycle) that render to
    /// inline SVG.
    pub markdown: String,
    /// `light` (default) or `dark`.
    #[serde(default)]
    pub theme: Option<String>,
    /// Palette family: `classic` (default), `signal-flow`, `blueprint`,
    /// `editorial` — orthogonal to theme.
    #[serde(default)]
    pub preset: Option<String>,
    /// Composition audit profile: `standard` (default) or `showcase`.
    /// Grades the receipt only; the artifact bytes are identical.
    #[serde(default)]
    pub quality: Option<String>,
    /// Bake the declarative entrance choreography into the artifact
    /// (CSS keyframes, zero scripts). Default false.
    #[serde(default)]
    pub motion: Option<bool>,
}

#[derive(Serialize)]
struct RenderMarkdownResponse {
    html: String,
    receipt: serde_json::Value,
}

pub(crate) async fn render_markdown_handler(
    Json(req): Json<RenderMarkdownRequest>,
) -> Result<impl IntoResponse, AppError> {
    use crate::docgen::theme::Theme;

    // (preset, mode) → theme, with the classic/light defaults filling
    // one-sided requests.
    let theme: Result<&'static Theme, String> = match (&req.preset, &req.theme) {
        (None, None) => Ok(&crate::docgen::theme::LIGHT),
        (None, Some(mode)) => Theme::by_name(mode).ok_or_else(|| {
            format!(
                "unknown theme \"{mode}\" — expected one of: {}",
                Theme::names().join(", ")
            )
        }),
        (Some(preset), mode) => Theme::resolve(preset, mode.as_deref().unwrap_or("light"))
            .ok_or_else(|| {
                format!(
                    "unknown preset \"{preset}\" or theme \"{}\" — presets: {}; themes: {}",
                    mode.as_deref().unwrap_or("light"),
                    Theme::presets().join(", "),
                    Theme::names().join(", ")
                )
            }),
    };
    let quality = req.quality.as_deref().map_or(
        Ok(crate::docgen::checks::Quality::Standard),
        |name| {
            crate::docgen::checks::Quality::by_name(name).ok_or_else(|| {
                format!("unknown quality \"{name}\" — expected one of: standard, showcase")
            })
        },
    );
    let (theme, quality) = match (theme, quality) {
        (Ok(t), Ok(q)) => (t, q),
        (Err(e), _) | (_, Err(e)) => return Err(AppError::BadRequest(e)),
    };
    let motion = req.motion.unwrap_or(false);
    let outcome = spawn_blocking(move || {
        crate::docgen::render_with_quality(&req.markdown, theme, quality, motion)
    })
    .await
    .map_err(|e| AppError::Internal(format!("render task failed: {e}")))?;
    Ok((
        StatusCode::OK,
        Json(RenderMarkdownResponse {
            html: outcome.html,
            receipt: outcome.receipt,
        }),
    ))
}
