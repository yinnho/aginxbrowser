//! Viewport overrides, emulated media, and the viewport band-capture family
//! (frame pump + CSS-time control). Split from page/mod.rs (ARCHITECTURE.md P2 batch 3); behavior unchanged.
use super::*;

impl Page {
    /// Pin the session's viewport: scripts see innerWidth/innerHeight/
    /// visualViewport move, matchMedia answers coarse-pointer/hover-none
    /// when `mobile`, and the layout ICB (element rects, @media cascade)
    /// follows via the same set_viewport op the persona publishes through.
    pub fn set_viewport_override(&mut self, w: f32, h: f32, mobile: bool, dpr: Option<f64>) {
        self.viewport_override = Some((w, h, mobile));
        self.dpr_override = dpr.filter(|d| *d > 0.0);
        self.apply_viewport_override();
    }

    /// The pinned viewport, if any — read back by session_clone so the
    /// derived session reproduces the same device emulation.
    pub fn viewport_override(&self) -> Option<(f32, f32, bool)> {
        self.viewport_override
    }

    /// Set the emulated media environment (CDP `Emulation.setEmulatedMedia`).
    /// `None` keeps a param unchanged (Chrome semantics: present replaces,
    /// absent untouched); both `None` = clear everything back to defaults.
    pub fn set_emulated_media(
        &mut self,
        features: Option<Vec<(String, String)>>,
        media: Option<Option<String>>,
    ) {
        let cur = self.emulated_media.take().unwrap_or(EmulatedMedia {
            features: Vec::new(),
            media: None,
        });
        self.emulated_media = Some(EmulatedMedia {
            features: features.unwrap_or(cur.features),
            media: match media {
                Some(m) => Some(m),
                None => cur.media,
            },
        });
        self.apply_emulated_media();
    }

    /// Replay the emulated media into the current realm: the bootstrap
    /// recomputes its matchMedia truth tables (firing change events on
    /// crossing MQLs) and pushes the same pairs to the Rust layout state
    /// via the `set_media_env` op, so @media arms re-parse in agreement.
    pub(super) fn apply_emulated_media(&mut self) {
        let Some(env) = &self.emulated_media else { return; };
        let features = serde_json::to_string(&env.features).unwrap_or_else(|_| "[]".into());
        let media = match &env.media {
            Some(Some(m)) if m.eq_ignore_ascii_case("print") => "print",
            _ => "screen",
        };
        if let Some(js) = &mut self.js {
            let _ = js.execute_script(
                "<emulated-media>",
                &format!("__diting_setMediaFeatures({features:?}, {media:?})"),
            );
        }
    }

    /// Browser.setContentsSize path: resize the window *contents* (the
    /// viewport) without disturbing the emulation knobs — a pinned mobile
    /// flag or dpr survives, only the dimensions move (Chrome's resize is a
    /// contents-size change, not a device-metrics re-pin). With nothing
    /// pinned, this is a plain desktop viewport pin.
    pub fn resize_contents(&mut self, w: f32, h: f32) {
        let mobile = self.viewport_override.map(|(_, _, m)| m).unwrap_or(false);
        self.set_viewport_override(w, h, mobile, self.dpr_override);
    }

    /// The pinned device-pixel ratio, if any — read back so scoped viewport
    /// changes (Page.printToPDF pins paper size for the render) restore the
    /// session's emulation exactly, dpr included.
    pub fn dpr_override(&self) -> Option<f64> {
        self.dpr_override
    }

    /// The viewport every frame-producing surface agrees on: the pinned
    /// emulation override when valid, else the live realm's persona
    /// viewport, else the CDP default 1280x720 (what getLayoutMetrics used
    /// to hard-code — the AginxOS report's P1 mismatch).
    #[cfg(feature = "screenshot")]
    pub fn effective_viewport(&self) -> (f32, f32) {
        if let Some((w, h, _)) = self.viewport_override {
            if w.is_finite() && w > 0.0 && h.is_finite() && h > 0.0 {
                return (w, h);
            }
        }
        if let Some(js) = &self.js {
            return js.with_state(|st| st.viewport);
        }
        (1280.0, 720.0)
    }

    /// One viewport-band frame from the live realm's cached layout (band
    /// paint; no outerHTML re-parse). Returns the frame plus the img URLs
    /// the caller should fetch (async, page client) and store via
    /// [`Self::store_band_image`] before calling again.
    #[cfg(feature = "screenshot")]
    pub fn viewport_band_frame(
        &self,
        scroll_x: f32,
        scroll_y: f32,
        viewport: (f32, f32),
    ) -> Option<(crate::diting_js::ops::BandFrame, Vec<String>)> {
        let js = self.js.as_ref()?;
        js.with_state(|st| crate::diting_js::ops::band_frame(st, scroll_x, scroll_y, viewport))
    }

    /// Same band frame but also collecting the PDF text layer (vector-text
    /// batch): `frame.text_ops` carries the band-local glyph ops and the
    /// raster pass skips those items.
    #[cfg(feature = "screenshot")]
    pub fn viewport_band_frame_with_text(
        &self,
        scroll_x: f32,
        scroll_y: f32,
        viewport: (f32, f32),
    ) -> Option<(crate::diting_js::ops::BandFrame, Vec<String>)> {
        let js = self.js.as_ref()?;
        js.with_state(|st| {
            crate::diting_js::ops::band_frame_with_text(st, scroll_x, scroll_y, viewport)
        })
    }

    /// Page-cut band frame (print/slides pumps, #60): `(x, y)` is a
    /// document-space band origin, not a scroll offset — root
    /// `overflow: hidden` must not collapse the paintable extent, or every
    /// page of such a deck renders the first viewport's frame. Scroll
    /// semantics (the collapse, so `window.scrollY` and the scroll pump
    /// agree) stay on [`Self::viewport_band_frame`].
    #[cfg(feature = "screenshot")]
    pub fn viewport_band_cut(
        &self,
        x: f32,
        y: f32,
        viewport: (f32, f32),
    ) -> Option<(crate::diting_js::ops::BandFrame, Vec<String>)> {
        let js = self.js.as_ref()?;
        js.with_state(|st| crate::diting_js::ops::band_frame_cut(st, x, y, viewport))
    }

    /// [`Self::viewport_band_cut`] with the PDF text layer collected.
    #[cfg(feature = "screenshot")]
    pub fn viewport_band_cut_with_text(
        &self,
        x: f32,
        y: f32,
        viewport: (f32, f32),
    ) -> Option<(crate::diting_js::ops::BandFrame, Vec<String>)> {
        let js = self.js.as_ref()?;
        js.with_state(|st| {
            crate::diting_js::ops::band_frame_cut_with_text(st, x, y, viewport)
        })
    }

    /// The document's text-ink extent (CSS px) from the same cached layout
    /// run band paint rides — the true content height of a bare-text body,
    /// whose only element boxes (html/body) stretch to the viewport.
    #[cfg(feature = "screenshot")]
    pub fn text_ink_extent(&self) -> Option<(f32, f32)> {
        let js = self.js.as_ref()?;
        js.with_state(crate::diting_js::ops::text_ink_extent)
    }

    /// Store a fetched image body for band paint (FIFO-capped; drops the
    /// layout cache since intrinsic sizes can reflow placeholder boxes).
    #[cfg(feature = "screenshot")]
    pub fn store_band_image(&self, url: String, bytes: Vec<u8>) {
        if let Some(js) = &self.js {
            js.with_state_mut(|st| crate::diting_js::ops::store_image_bytes(st, url, bytes));
        }
    }

    /// Fetch the img bodies band paint is missing, through the page's own
    /// identity: the stealth stack when armed (its TLS/UA fingerprint and
    /// cookie jar are what fingerprint-gated image CDNs — the bilibili-412
    /// family — let through), else the plain subresource path. Both carry
    /// the document as Referer (strict-origin-when-cross-origin, the plain
    /// client's subresource policy — Referer-checking CDNs reject a bare
    /// request). Shared by every band-paint pump (CDP capture, screencast,
    /// video, print/PDF); same per-URL policy everywhere: SSRF gate, ≤2 MiB
    /// per body, 3 s per request, 200-only. Failures just leave the
    /// placeholder — a frame beats a stall.
    #[cfg(feature = "screenshot")]
    pub async fn fetch_band_images(&self, urls: Vec<String>) {
        const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
        const PER_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
        // Same hard block the static script/stylesheet loaders enforce: a
        // `Network.setBlockedURLs` match must not reach the wire from the
        // render path either (the frame keeps its placeholder).
        let urls: Vec<String> = urls
            .into_iter()
            .filter(|u| {
                if self.url_blocked(u) {
                    tracing::info!("Blocked band image by Network.setBlockedURLs: {}", u);
                    false
                } else {
                    true
                }
            })
            .collect();
        let base = self.url_string();
        let client = self.http_client.clone();
        #[cfg(feature = "stealth")]
        let stealth = self.stealth_client.clone();
        let futs = urls.into_iter().map(|u| {
            let client = client.clone();
            let base = base.clone();
            #[cfg(feature = "stealth")]
            let stealth = stealth.clone();
            async move {
                let Ok(parsed) = url::Url::parse(&u) else { return None };
                if crate::diting_js::ops::validate_fetch_url(&parsed).is_err() {
                    return None;
                }
                let resp = tokio::time::timeout(PER_REQUEST_TIMEOUT, async {
                    #[cfg(feature = "stealth")]
                    if let Some(ref s) = stealth {
                        return s.fetch_subresource(&parsed, Some(base.as_str())).await.ok();
                    }
                    #[allow(unreachable_code)]
                    client
                        .fetch_subresource(&parsed, Some(base.as_str()))
                        .await
                        .ok()
                })
                .await
                .ok()
                .flatten()?;
                if resp.status != 200 || resp.body.is_empty() || resp.body.len() > MAX_BODY_BYTES {
                    return None;
                }
                Some((u, resp.body))
            }
        });
        let got: Vec<(String, Vec<u8>)> = futures::future::join_all(futs)
            .await
            .into_iter()
            .flatten()
            .collect();
        for (u, body) in got {
            self.store_band_image(u, body);
        }
    }

    /// The root scroller's mirrored offset (screencast damage signatures and
    /// frame metadata).
    #[cfg(feature = "screenshot")]
    pub fn scroll_offset(&self) -> (f32, f32) {
        self.js
            .as_ref()
            .map(|js| js.with_state(|st| st.scroll_offset))
            .unwrap_or((0.0, 0.0))
    }

    /// Advance the CSS animation clock. The video pump calls this per frame
    /// before painting; a static render never does, so animated SVGs frame
    /// their finished state (poster semantics).
    #[cfg(feature = "screenshot")]
    pub fn set_css_time(&mut self, t: f64) {
        let Some(js) = &mut self.js else { return };
        let _ = js.execute_script(
            "<css-time>",
            &format!("__diting_domRaw('set_css_time', '{t:.6}')"),
        );
    }

    /// Longest CSS animation on the page (max delay+duration over every
    /// element's computed `animation`), from the last layout run. 0 when no
    /// animations exist — the video pump falls back to `__timelines` then.
    #[cfg(feature = "screenshot")]
    pub fn css_animation_extent(&self) -> f64 {
        self.js
            .as_ref()
            .map(|js| js.with_state(|st| st.css_extent.get()))
            .unwrap_or(0.0)
    }

    /// The live tree's mutation epoch — part of the screencast damage
    /// signature, so DOM changes retrigger frames while a static scroll
    /// position does not.
    #[cfg(feature = "screenshot")]
    pub fn dom_epoch(&self) -> u64 {
        self.js
            .as_ref()
            .and_then(|js| js.with_state(|st| st.dom.as_ref().map(|d| d.epoch())))
            .unwrap_or(0)
    }

    /// How many full taffy solves this page has run. Test probe for the
    /// #395 paint-only path: transform/opacity style writes must repaint
    /// without moving this counter. Compiled whenever the render stack is
    /// (not cfg(test)): the product crate's own tests probe it across the
    /// workspace split — same posture as `layout_rev` below.
    #[cfg(feature = "screenshot")]
    pub fn layout_solve_count(&self) -> u64 {
        self.js
            .as_ref()
            .map(|js| js.with_state(|st| st.solves.get()))
            .unwrap_or(0)
    }

    /// How many band paints this page has produced. Test probe for the
    /// video pump's static-frame reuse (#398): held frames must skip the
    /// paint entirely, so this counter sits below the pumped frame count.
    /// Compiled whenever the render stack is (not cfg(test)) — the product
    /// crate's /video tests probe it across the workspace split.
    #[cfg(feature = "screenshot")]
    pub fn band_paint_count(&self) -> u64 {
        self.js
            .as_ref()
            .map(|js| js.with_state(|st| st.band_paints.get()))
            .unwrap_or(0)
    }

    /// Layout invalidation revision — the other half of the screencast damage
    /// signature. The tree epoch above is a shape stamp: attribute-level
    /// writes (style/class/attr) drop the layout cache without allocating
    /// nodes, so without this rev a style change freezes the cast while
    /// layout probes report fresh geometry.
    #[cfg(feature = "screenshot")]
    pub fn layout_rev(&self) -> u64 {
        self.js
            .as_ref()
            .and_then(|js| js.with_state(|st| Some(st.layout_rev.get())))
            .unwrap_or(0)
    }

    /// Drop the override and return to the persona viewport everywhere.
    pub fn clear_viewport_override(&mut self) {
        self.viewport_override = None;
        self.dpr_override = None;
        if let Some(js) = &mut self.js {
            let _ = js.execute_script("<viewport>", "__diting_clearViewport()");
        }
    }

    /// Replay the stored override into the current realm. No-op when none
    /// is set, so navigation on a default session costs one branch.
    pub(super) fn apply_viewport_override(&mut self) {
        let Some((w, h, mobile)) = self.viewport_override else { return; };
        // 0 = "persona default" per the CDP deviceScaleFactor semantics the
        // bootstrap side implements; a pinned dpr rides every replay.
        let dpr = self.dpr_override.map(|d| d.to_string()).unwrap_or_else(|| "0".into());
        if let Some(js) = &mut self.js {
            let _ = js.execute_script(
                "<viewport>",
                &format!("__diting_setViewport({w}, {h}, {mobile}, {dpr})"),
            );
        }
    }
}
