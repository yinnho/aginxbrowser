//! Timeline video pump — the time-slicing layer above the engine.
//!
//! Takes a live [`Page`] whose scripts registered GSAP-style timelines in
//! `window.__timelines` (objects with `duration()` and `pause(t)`), seeks
//! them frame by frame, paints each viewport band, and pipes the raw RGBA
//! stream into ffmpeg's stdin — the MP4 comes back as bytes. Everything runs
//! in-process: no CDP, no PNG round-trip, no wall clock in the frame values
//! (`t = i / fps`, the seek protocol hyperframes established).
//!
//! The invalidation chain that makes per-frame seeks repaint is deliberate:
//! `tl.pause(t)` writes `el.style.transform/opacity`, the bootstrap's
//! `_push()` lands that as a `set_attribute` op, and op_dom_inner drops the
//! memoized layout for exactly the mutation commands — so the next
//! `viewport_band_frame` re-cascades with the fresh inline styles. Since the
//! geometry/collection cache split (#395), transform/opacity writes only
//! drop the collection half: the taffy solve is cached and `layout_collect`
//! re-runs against the fresh styles — what real browsers hand to the
//! compositor. Geometry writes (width, …) still drop both.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::diting_browser::Page;
use crate::diting_layout::text::FontBook;

/// One rendered timeline video.
#[derive(Debug)]
pub struct TimelineVideo {
    /// MP4 file bytes (H.264, yuv420p).
    pub mp4: Vec<u8>,
    /// Frames actually written to ffmpeg.
    pub frames: u32,
    /// Longest registered timeline, seconds (from `duration()`).
    pub timeline_secs: f64,
    /// Total video length = timeline + hold tail, seconds.
    pub duration_secs: f64,
    /// Encoded pixel size (the request viewport floored to even — yuv420p).
    pub width: u32,
    pub height: u32,
    /// Whether an audio track (BGM and/or narration) was muxed in.
    pub has_audio: bool,
    /// Whether a soft subtitle track was muxed in.
    pub has_subtitles: bool,
    /// Whether the cues were also burned into the frame pixels.
    pub burned_subtitles: bool,
}

#[derive(Debug)]
pub enum VideoError {
    /// `window.__timelines` stayed empty until the wait deadline.
    NoTimelines,
    /// No live document behind the page (pre-navigation or torn down).
    NoLiveDocument,
    /// Timelines exist but none reports a positive finite duration.
    ZeroDuration,
    /// Timeline + hold tail exceeds the safety cap.
    DurationCapped { asked: f64, cap: f64 },
    /// The requested audio track could not be fetched/validated — the caller
    /// asked for sound, so a silent MP4 would be the wrong output.
    AudioFetchFailed(String),
    /// The inline subtitles could not be staged (oversize or unwritable
    /// temp) — same rule: the caller asked for a subtitle track.
    SubtitleStaging(String),
    /// ffmpeg not found on PATH.
    NoFfmpeg,
    /// ffmpeg exited non-zero; carries its stderr tail.
    FfmpegFailed(String),
    /// A frame's pixel buffer didn't match the dimensions ffmpeg was sized
    /// with (viewport changed mid-render).
    FrameSizeMismatch { expect: usize, got: usize, frame: u32 },
}

impl std::fmt::Display for VideoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VideoError::NoTimelines => {
                write!(f, "window.__timelines never populated before the deadline")
            }
            VideoError::NoLiveDocument => write!(f, "no live document to paint frames from"),
            VideoError::ZeroDuration => write!(f, "no timeline reports a positive duration"),
            VideoError::DurationCapped { asked, cap } => {
                write!(f, "timeline {asked:.1}s exceeds the {cap:.0}s safety cap")
            }
            VideoError::AudioFetchFailed(tail) => {
                write!(f, "audio track unavailable: {tail}")
            }
            VideoError::SubtitleStaging(tail) => {
                write!(f, "subtitles rejected: {tail}")
            }
            VideoError::NoFfmpeg => write!(f, "ffmpeg not found on PATH"),
            VideoError::FfmpegFailed(tail) => write!(f, "ffmpeg failed: {tail}"),
            VideoError::FrameSizeMismatch { expect, got, frame } => {
                write!(f, "frame {frame} is {got} bytes, expected {expect}")
            }
        }
    }
}

impl std::error::Error for VideoError {}

/// Background music / narration muxed into the MP4 as a second ffmpeg input.
/// The URL is fetched through the page's own HTTP client (same policy family
/// as the band-image fetches, with a bigger cap — music files are MBs);
/// ffmpeg probes the container by content, so mp3/wav/ogg/m4a all work.
#[derive(Debug, Clone)]
pub struct AudioTrack {
    pub url: String,
    /// Linear multiplier 0..=2; 1 = as authored.
    pub volume: f32,
    /// Fade out over the final N seconds of the video. 0 = none.
    pub fade_out_secs: f32,
    /// Loop the track to cover the whole video (BGM semantics for a clip
    /// shorter than the timeline). Default true.
    pub loop_audio: bool,
}

/// One voiceover clip placed on the video timeline. Agents generate these
/// with any TTS and hand us the URL — same fetch policy as the BGM track.
#[derive(Debug, Clone)]
pub struct NarrationClip {
    pub url: String,
    /// Where this clip starts, seconds from video t=0.
    pub start_secs: f64,
    /// Linear multiplier 0..=2; 1 = as authored.
    pub volume: f32,
}

pub struct TimelineVideoOptions {
    pub fps: f64,
    pub viewport: (f32, f32),
    /// Freeze the final timeline state for this many extra seconds.
    pub hold_tail_secs: f64,
    /// Safety cap on timeline + hold tail.
    pub max_duration_secs: f64,
    /// How long to wait for `window.__timelines` to appear.
    pub wait_timelines: Duration,
    /// Background audio to mux in, if any.
    pub audio: Option<AudioTrack>,
    /// Voiceover clips, each placed at its own start time and mixed into the
    /// one AAC track. Never looped — narration that repeats is an authoring
    /// bug, not a semantic.
    pub narration: Vec<NarrationClip>,
    /// Inline SRT text muxed as a soft (toggleable) mov_text track.
    pub subtitles_srt: Option<String>,
    /// ISO language tag for the subtitle track ("eng", "zh"), shown by
    /// players when labeling the track.
    pub subtitles_language: Option<String>,
    /// Burn the cues into the frame pixels too (hardsub). `None` = burn
    /// whenever `subtitles_srt` is present — QuickTime, WeChat and most
    /// social embeds ignore the soft mov_text track, pixels never do.
    /// `Some(false)` keeps the soft track only.
    pub burn_subtitles: Option<bool>,
}

impl Default for TimelineVideoOptions {
    fn default() -> Self {
        Self {
            fps: 24.0,
            viewport: (1280.0, 720.0),
            hold_tail_secs: 0.5,
            max_duration_secs: 120.0,
            wait_timelines: Duration::from_secs(10),
            audio: None,
            narration: Vec::new(),
            subtitles_srt: None,
            subtitles_language: None,
            burn_subtitles: None,
        }
    }
}

/// Audio bodies get their own, larger cap — a three-minute 128 kbps MP3 is
/// already ~2.8 MB, so the image cap would reject ordinary music files.
const MAX_AUDIO_BYTES: usize = 16 * 1024 * 1024;
const AUDIO_FETCH_TIMEOUT: Duration = Duration::from_secs(8);
/// Inline SRT cap: subtitle text is kilobytes; anything past this is a
/// caller bug, not a track.
const MAX_SUBTITLE_BYTES: usize = 64 * 1024;

/// Per-phase pump timings for the AGINXBROWSER_VIDEO_TRACE knob: where the
/// frame budget goes — JS seek, band paint (any relayout inside
/// ensure_layout_run included; AGINXBROWSER_LAYOUT_TRACE splits that phase
/// further), image refetch, and the ffmpeg stdin write.
#[derive(Default)]
struct PumpPhases {
    frames: u32,
    eval: std::time::Duration,
    band: std::time::Duration,
    images: std::time::Duration,
    write: std::time::Duration,
}

impl PumpPhases {
    fn record(&mut self, eval: std::time::Duration, band: std::time::Duration, images: std::time::Duration) {
        self.frames += 1;
        self.eval += eval;
        self.band += band;
        self.images += images;
    }
}

/// One parsed subtitle cue: active for `[start, end)` seconds.
struct SubCue {
    start: f64,
    end: f64,
    text: String,
}

/// A cue rasterized once: white glyphs over an 8-direction black outline
/// (the hardsub look every player shows — QuickTime, WeChat and most social
/// embeds ignore the mov_text soft track, pixels never do). Straight-alpha
/// RGBA, composited bottom-center onto every frame the cue covers. A cue
/// spans many frames and text raster is the one expensive step here, so
/// tiles cache per cue.
struct CueTile {
    width: usize,
    height: usize,
    data: Vec<u8>,
}

/// Burned-in subtitles: the same inline SRT the soft mov_text track muxes
/// from, parsed, rasterized on first use per cue, and painted onto the pump
/// frames before they reach ffmpeg. Both tracks ride together — soft for
/// players that honor it, burn for everything else.
struct Hardsub {
    cues: Vec<SubCue>,
    tiles: Vec<Option<CueTile>>,
}

impl Hardsub {
    fn none() -> Self {
        Self { cues: Vec::new(), tiles: Vec::new() }
    }

    /// Parse the inline SRT. Malformed blocks are skipped, never fatal —
    /// burning must not reject a video the soft-track path accepted.
    fn parse(srt: &str) -> Self {
        let mut cues = Vec::new();
        for block in srt.replace("\r\n", "\n").split("\n\n") {
            let lines: Vec<&str> = block.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
            let Some(t_i) = lines.iter().position(|l| l.contains("-->")) else { continue };
            let Some((start, end)) = lines[t_i].split_once("-->") else { continue };
            let (Some(start), Some(end)) =
                (parse_srt_timestamp(start), parse_srt_timestamp(end))
            else {
                continue;
            };
            if end <= start {
                continue;
            }
            let text = lines[t_i + 1..].join("\n");
            if !text.is_empty() {
                cues.push(SubCue { start, end, text });
            }
        }
        let tiles: Vec<Option<CueTile>> = (0..cues.len()).map(|_| None).collect();
        Self { cues, tiles }
    }

    fn is_empty(&self) -> bool {
        self.cues.is_empty()
    }

    /// Composite the cue active at `t` onto the frame, rasterizing its tile
    /// on first use. Later cues win on overlap (SRT convention).
    fn burn(&mut self, frame: &mut [u8], w: u32, h: u32, t: f64, fonts: &FontBook) {
        let Some(i) = self.cues.iter().rposition(|c| c.start <= t && t < c.end) else {
            return;
        };
        let tile = self.tiles[i].get_or_insert_with(|| {
            rasterize_cue_tile(&self.cues[i].text, w, h, fonts)
        });
        let x0 = (w as i64 - tile.width as i64) / 2;
        let y0 = h as i64 - tile.height as i64 - (h / 12) as i64;
        composite_tile(frame, w as usize, h as usize, TileRef::from(&*tile), x0, y0);
    }
}

/// `"HH:MM:SS,mmm"` (or `.mmm`) → seconds. SRT mandates the comma; players
/// in the wild also accept the dot, so both parse.
fn parse_srt_timestamp(s: &str) -> Option<f64> {
    // Cue settings can trail the timestamp ("... --> ... x1:0 x2:100").
    let s = s.split_whitespace().next()?;
    let (hms, ms) = match (s.rfind(','), s.rfind('.')) {
        (Some(c), None) => s.split_at(c),
        (None, Some(d)) => s.split_at(d),
        // Both present: whichever sits later is the ms separator.
        (Some(c), Some(d)) => s.split_at(c.max(d)),
        (None, None) => return None,
    };
    let mut parts = hms.split(':');
    let (h, m, sec) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let ms: f64 = ms[1..].parse().ok()?;
    let h: f64 = h.parse().ok()?;
    let m: f64 = m.parse().ok()?;
    let sec: f64 = sec.parse().ok()?;
    Some(h * 3600.0 + m * 60.0 + sec + ms / 1000.0)
}

/// Rasterize one cue into a self-contained tile. Metrics scale with the
/// frame so the same SRT reads the same at 720p and 4K: font ≈ height/20,
/// stroke ≈ font/12, wrap at 90% width. The black outline is the same raster
/// offset in 8 directions — a real stroke pass would need a new rasterizer,
/// offset compositing gets within a pixel of libass's look at hardsub sizes.
fn rasterize_cue_tile(text: &str, w: u32, h: u32, fonts: &FontBook) -> CueTile {
    let font_size = h as f32 / 20.0;
    let line_height = font_size * 1.4;
    let wrap_at = w as f32 * 0.9;
    let stroke = ((font_size / 12.0).round() as i32).max(1);
    let pad = stroke as usize;
    let white = fonts.rasterize_wrapped(text, font_size, true, [255, 255, 255, 255], wrap_at, line_height);
    let black = fonts.rasterize_wrapped(text, font_size, true, [12, 12, 12, 255], wrap_at, line_height);
    let width = white.width + pad * 2;
    let height = white.height + pad * 2;
    let mut data = vec![0u8; width * height * 4];
    let stroke_i64 = stroke as i64;
    for (dx, dy) in [
        (-stroke, 0), (stroke, 0), (0, -stroke), (0, stroke),
        (-stroke, -stroke), (-stroke, stroke), (stroke, -stroke), (stroke, stroke),
    ] {
        composite_tile(
            &mut data, width, height, TileRef::from(&black),
            stroke_i64 + dx as i64, stroke_i64 + dy as i64,
        );
    }
    composite_tile(&mut data, width, height, TileRef::from(&white), stroke_i64, stroke_i64);
    CueTile { width, height, data }
}

/// A borrowed RGBA tile — the burn compositor's blit unit, wrapping both
/// the cached cue tiles and the raw rasters the font stack returns.
struct TileRef<'a> {
    width: usize,
    height: usize,
    data: &'a [u8],
}

impl<'a> From<&'a CueTile> for TileRef<'a> {
    fn from(t: &'a CueTile) -> Self {
        Self { width: t.width, height: t.height, data: &t.data }
    }
}

impl<'a> From<&'a crate::diting_layout::text::TextRaster> for TileRef<'a> {
    fn from(r: &'a crate::diting_layout::text::TextRaster) -> Self {
        Self { width: r.width, height: r.height, data: &r.data }
    }
}

/// Source-over blit of an RGBA tile into an RGBA buffer, clipped at the
/// destination bounds. Same straight-alpha model as paint.rs `over`, but
/// maintaining the destination alpha — cue tiles start transparent.
fn composite_tile(dst: &mut [u8], dw: usize, dh: usize, src: TileRef, x0: i64, y0: i64) {
    let TileRef { width: sw, height: sh, data: src } = src;
    if sw == 0 || sh == 0 {
        return;
    }
    let (gx0, gx1) = (x0.max(0), (x0 + sw as i64).min(dw as i64));
    let (gy0, gy1) = (y0.max(0), (y0 + sh as i64).min(dh as i64));
    if gx0 >= gx1 || gy0 >= gy1 {
        return;
    }
    for gy in gy0..gy1 {
        let srow = ((gy - y0) as usize * sw + (gx0 - x0) as usize) * 4;
        let drow = (gy as usize * dw + gx0 as usize) * 4;
        for gx in gx0..gx1 {
            let s = srow + (gx - gx0) as usize * 4;
            let d = drow + (gx - gx0) as usize * 4;
            let sa = src[s + 3] as u32;
            if sa == 0 {
                continue;
            }
            let da = dst[d + 3] as u32;
            let out_a = sa + da * (255 - sa) / 255;
            for c in 0..3 {
                let sc = src[s + c] as u32 * sa * 255;
                let dc = dst[d + c] as u32 * da * (255 - sa);
                dst[d + c] = ((sc + dc) / (255 * out_a)) as u8;
            }
            dst[d + 3] = out_a as u8;
        }
    }
}

/// Render the page's registered timelines to an MP4. See the module docs for
/// the seek protocol. The page must already be navigated; this waits for the
/// timelines itself.
pub async fn render_timeline_video(
    page: &mut Page,
    opts: &TimelineVideoOptions,
) -> Result<TimelineVideo, VideoError> {
    // Wait for the timelines registry. A page still loading scripts populates
    // it from a <script> at some point after load; settle() between polls lets
    // timers/fetches progress so the wait isn't a busy spin on a frozen loop.
    let started = Instant::now();
    loop {
        // V8 numbers round-trip as JSON f64 even for whole values, so
        // as_u64() on Number(1.0) is None — read through as_f64.
        let count = page
            .evaluate("Object.keys(window.__timelines || {}).length")
            .as_f64()
            .unwrap_or(0.0) as u64;
        if count > 0 {
            break;
        }
        if started.elapsed() >= opts.wait_timelines {
            return Err(VideoError::NoTimelines);
        }
        page.settle(50).await;
    }

    let timeline = page
        .evaluate(
            "(() => { let d = 0; \
             for (const k in window.__timelines) d = Math.max(d, +window.__timelines[k].duration() || 0); \
             return d; })()",
        )
        .as_f64()
        .unwrap_or(0.0);
    if !timeline.is_finite() || timeline <= 0.0 {
        return Err(VideoError::ZeroDuration);
    }
    let total = timeline + opts.hold_tail_secs;
    if total > opts.max_duration_secs {
        return Err(VideoError::DurationCapped { asked: total, cap: opts.max_duration_secs });
    }
    let frames = ((total * opts.fps).ceil() as u64).max(1) as u32;

    // Even dimensions only: yuv420p chroma subsampling rejects odd sizes, so
    // the viewport floors to even before the first paint (and every later
    // frame must match what ffmpeg was sized with).
    let viewport = (
        ((opts.viewport.0.floor() as u32) & !1) as f32,
        ((opts.viewport.1.floor() as u32) & !1) as f32,
    );

    // Frame 0 first: it fixes the pixel size ffmpeg gets spawned with and
    // absorbs the one-time band-image fetch pass before encoding starts.
    let mut phases = PumpPhases::default();
    let (w, h, mut first) = seek_and_paint(page, viewport, 0.0, &mut phases).await?;
    let expect = w as usize * h as usize * 4;

    // MP4 muxing needs a seekable output, so the file goes to a temp path
    // (read back and removed below) while frames stream through stdin.
    static PUMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = PUMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let out_path = std::env::temp_dir().join(format!(
        "aginxbrowser-pump-{}-{seq}.mp4",
        std::process::id()
    ));

    // Audio, when requested: BGM track plus narration clips all become the
    // same shape (fetched bytes + a place on the timeline), parked in temp
    // files as ffmpeg inputs 1..N (stdin is the frame stream, input 0).
    // `total` is known here so the fade can be clamped to the video length.
    let mut fade_out = 0.0_f64;
    let mut audio_inputs: Vec<(Vec<u8>, u64, f32, bool)> = Vec::new();
    if let Some(a) = &opts.audio {
        audio_inputs.push((
            fetch_audio(page, &a.url).await?,
            0,
            a.volume.clamp(0.0, 2.0),
            a.loop_audio,
        ));
        fade_out = (a.fade_out_secs.max(0.0) as f64).min(total);
    }
    for clip in &opts.narration {
        audio_inputs.push((
            fetch_audio(page, &clip.url).await?,
            (clip.start_secs.max(0.0) * 1000.0).round() as u64,
            clip.volume.clamp(0.0, 2.0),
            false,
        ));
    }
    let audio_paths: Vec<std::path::PathBuf> = audio_inputs
        .iter()
        .enumerate()
        .map(|(i, (bytes, ..))| {
            let p = std::env::temp_dir()
                .join(format!("aginxbrowser-pump-{}-{seq}.a{i}", std::process::id()));
            std::fs::write(&p, bytes)
                .map_err(|e| VideoError::AudioFetchFailed(format!("audio temp write: {e}")))
                .map(|_| p)
        })
        .collect::<Result<_, _>>()?;

    // Subtitles, two layers from the same SRT: staged as the last input and
    // muxed as a soft mov_text track (a text codec ffmpeg carries natively —
    // no libass needed for muxing), and burned into the frame pixels by the
    // pump's own font stack. Burning is on whenever subtitles ride because
    // QuickTime, WeChat and most social embeds ignore mov_text; opt out with
    // burn_subtitles: Some(false) for the soft track only.
    let srt_text = opts
        .subtitles_srt
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let mut hardsub = match (opts.burn_subtitles.unwrap_or(true), srt_text) {
        (true, Some(srt)) => Hardsub::parse(srt),
        _ => Hardsub::none(),
    };
    // font_book() re-parses its faces per call, so the burn path takes it
    // once per render and hands the book around — cues rasterize lazily
    // against it.
    let hardsub_fonts = (!hardsub.is_empty()).then(crate::diting_fonts::font_book);
    if let Some(fonts) = &hardsub_fonts {
        hardsub.burn(&mut first, w, h, 0.0, fonts);
    }
    let subtitles_path: Option<std::path::PathBuf> = srt_text
        .map(|srt| {
            if srt.len() > MAX_SUBTITLE_BYTES {
                return Err(VideoError::SubtitleStaging(format!(
                    "subtitles_srt is {} bytes (cap {})",
                    srt.len(),
                    MAX_SUBTITLE_BYTES
                )));
            }
            let p = std::env::temp_dir()
                .join(format!("aginxbrowser-pump-{}-{seq}.srt", std::process::id()));
            std::fs::write(&p, srt)
                .map_err(|e| VideoError::SubtitleStaging(format!("subtitle temp write: {e}")))?;
            Ok(p)
        })
        .transpose()?;

    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-f")
        .arg("rawvideo")
        .arg("-pixel_format")
        .arg("rgba")
        .arg("-video_size")
        .arg(format!("{w}x{h}"))
        .arg("-framerate")
        .arg(format!("{}", opts.fps))
        .arg("-i")
        .arg("pipe:0");
    for (path, (_, _, _, looped)) in audio_paths.iter().zip(&audio_inputs) {
        // `-stream_loop` is an input option: it must precede this -i. Looping
        // makes the stream unbounded, so output length is pinned by -t below
        // instead of -shortest (which only checks stream ends).
        if *looped {
            cmd.arg("-stream_loop").arg("-1");
        }
        cmd.arg("-i").arg(path);
    }
    if let Some(sp) = &subtitles_path {
        cmd.arg("-i").arg(sp);
    }
    if !audio_inputs.is_empty() {
        // One audio source rides a plain -af chain; two or more need
        // filter_complex to delay each clip onto the timeline and amix them.
        if audio_inputs.len() == 1 {
            let (_, delay_ms, volume, _) = &audio_inputs[0];
            let mut filters = Vec::new();
            if (*volume - 1.0).abs() > f32::EPSILON {
                filters.push(format!("volume={volume:.3}"));
            }
            if *delay_ms > 0 {
                filters.push(format!("adelay={delay_ms}:all=1"));
            }
            if fade_out > 0.0 {
                filters.push(format!("afade=t=out:st={:.3}:d={fade_out:.3}", total - fade_out));
            }
            if !filters.is_empty() {
                cmd.arg("-af").arg(filters.join(","));
            }
        } else {
            let mut graph = String::new();
            let mut refs: Vec<String> = Vec::new();
            for (idx, (_, delay_ms, volume, _)) in audio_inputs.iter().enumerate() {
                // Input index idx+1: 0 is the rawvideo pipe.
                let mut parts = Vec::new();
                if (*volume - 1.0).abs() > f32::EPSILON {
                    parts.push(format!("volume={volume:.3}"));
                }
                if *delay_ms > 0 {
                    parts.push(format!("adelay={delay_ms}:all=1"));
                }
                if parts.is_empty() {
                    // No filter needed — feed the input straight into amix.
                    refs.push(format!("[{}:a]", idx + 1));
                } else {
                    graph.push_str(&format!("[{}:a]{}[s{idx}];", idx + 1, parts.join(",")));
                    refs.push(format!("[s{idx}]"));
                }
            }
            // normalize=0: amix's default 1/N scaling would halve narration
            // volume just for having a quiet BGM under it.
            let mut mix = format!(
                "{}amix=inputs={}:duration=longest:normalize=0",
                refs.join(""),
                audio_inputs.len()
            );
            if fade_out > 0.0 {
                mix.push_str(&format!(",afade=t=out:st={:.3}:d={fade_out:.3}", total - fade_out));
            }
            graph.push_str(&format!("{mix}[aout]"));
            cmd.arg("-filter_complex")
                .arg(graph)
                .arg("-map")
                .arg("0:v")
                .arg("-map")
                .arg("[aout]");
        }
        cmd.arg("-c:a").arg("aac").arg("-b:a").arg("160k");
        cmd.arg("-t").arg(format!("{total:.3}"));
    }
    if subtitles_path.is_some() {
        // With 0-1 audio inputs ffmpeg's default stream selection picks the
        // srt stream on its own; the explicit -map above disables defaults,
        // so subtitles must be mapped explicitly there.
        if audio_inputs.len() >= 2 {
            cmd.arg("-map").arg(format!("{}:s:0", 1 + audio_inputs.len()));
        }
        cmd.arg("-c:s").arg("mov_text");
        if let Some(lang) = opts.subtitles_language.as_deref().filter(|l| !l.is_empty()) {
            cmd.arg("-metadata:s:s:0").arg(format!("language={lang}"));
        }
    }
    cmd.arg("-c:v")
        .arg("libx264")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-y")
        .arg(&out_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|_| VideoError::NoFfmpeg)?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let mut stderr = child.stderr.take().expect("piped stderr");
    // Drain ffmpeg's stderr off-thread: a chatty enough pipe would otherwise
    // fill its buffer and deadlock the encoder mid-stream.
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    });

    let mut written: u32 = 0;
    let mut send = |rgba: Vec<u8>, frame: u32| -> Result<(), VideoError> {
        if rgba.len() != expect {
            return Err(VideoError::FrameSizeMismatch { expect, got: rgba.len(), frame });
        }
        stdin
            .write_all(&rgba)
            .and_then(|_| stdin.flush())
            .map_err(|e| VideoError::FfmpegFailed(format!("stdin write: {e}")))
    };
    let t_send = Instant::now();
    send(first, 0)?;
    phases.write += t_send.elapsed();
    written += 1;
    let pump_wall = Instant::now();
    for i in 1..frames {
        let t = i as f64 / opts.fps;
        let (_, _, mut rgba) = seek_and_paint(page, viewport, t, &mut phases).await?;
        if let Some(fonts) = &hardsub_fonts {
            hardsub.burn(&mut rgba, w, h, t, fonts);
        }
        let t_send = Instant::now();
        send(rgba, i)?;
        phases.write += t_send.elapsed();
        written += 1;
    }
    // Closing stdin is ffmpeg's end-of-stream signal; the MP4 trailer lands
    // in the temp file after that.
    drop(stdin);
    let status = child.wait().map_err(|e| VideoError::FfmpegFailed(e.to_string()))?;
    let mp4 = std::fs::read(&out_path).unwrap_or_default();
    let _ = std::fs::remove_file(&out_path);
    for p in &audio_paths {
        let _ = std::fs::remove_file(p);
    }
    if let Some(p) = &subtitles_path {
        let _ = std::fs::remove_file(p);
    }
    if !status.success() {
        let tail: String = err_reader.join().unwrap_or_default().chars().rev().take(400).collect();
        return Err(VideoError::FfmpegFailed(tail.chars().rev().collect()));
    }
    // Debug knob (AGINXBROWSER_VIDEO_TRACE=1): pump phase summary, the video
    // counterpart of AGINXBROWSER_LAYOUT_TRACE. `band` includes any relayout
    // ensure_layout_run triggered (LAYOUT_TRACE splits that phase further).
    if std::env::var("AGINXBROWSER_VIDEO_TRACE").is_ok() {
        eprintln!(
            "[video-trace] frames={} wall={:?} eval={:?} band={:?} images={:?} write={:?}",
            phases.frames,
            pump_wall.elapsed(),
            phases.eval,
            phases.band,
            phases.images,
            phases.write
        );
    }
    Ok(TimelineVideo {
        mp4,
        frames: written,
        timeline_secs: timeline,
        duration_secs: total,
        width: w,
        height: h,
        has_audio: !audio_inputs.is_empty(),
        has_subtitles: subtitles_path.is_some(),
        burned_subtitles: hardsub_fonts.is_some(),
    })
}

/// Fetch background/narration audio through the page's own HTTP client with
/// the document as Referer — same posture as the band-image fetch, with a
/// music-sized cap. SSRF policy is the client's own gate (context-level
/// `allow_private_network`, re-validated per hop); a requested track that
/// can't be fetched is an error, not a silent video.
async fn fetch_audio(page: &Page, url: &str) -> Result<Vec<u8>, VideoError> {
    let err = |msg: String| Err(VideoError::AudioFetchFailed(msg));
    let Ok(parsed) = url::Url::parse(url) else {
        return err(format!("bad audio url {url:?}"));
    };
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return err(format!("audio url scheme {scheme:?} not allowed (http/https only)"));
    }
    let base = page.url_string();
    match tokio::time::timeout(
        AUDIO_FETCH_TIMEOUT,
        page.context.http_client.fetch_subresource(&parsed, Some(base.as_str())),
    )
    .await
    {
        Ok(Ok(resp)) if resp.status == 200 && !resp.body.is_empty() && resp.body.len() <= MAX_AUDIO_BYTES => {
            Ok(resp.body)
        }
        Ok(Ok(resp)) => err(format!(
            "audio fetch: status {}, {} bytes (cap {})",
            resp.status,
            resp.body.len(),
            MAX_AUDIO_BYTES
        )),
        Ok(Err(e)) => err(format!("audio fetch: {e}")),
        Err(_) => err("audio fetch timed out".to_string()),
    }
}

/// Seek every registered timeline to `t`, then paint one viewport band. The
/// first pass may report missing image URLs — those fetch through the page's
/// own client and the band repaints before returning (placeholders only if a
/// fetch fails; a frame beats a stall). Phase timings accumulate into
/// `phases` (the AGINXBROWSER_VIDEO_TRACE knob).
async fn seek_and_paint(
    page: &mut Page,
    viewport: (f32, f32),
    t: f64,
    phases: &mut PumpPhases,
) -> Result<(u32, u32, Vec<u8>), VideoError> {
    let seek = format!(
        "(() => {{ for (const k in window.__timelines) {{ \
         try {{ window.__timelines[k].pause({t:.4}); }} catch (e) {{}} }} }})()"
    );
    let t_eval = Instant::now();
    let _ = page.evaluate(&seek);
    let eval_dt = t_eval.elapsed();
    // The camera's window.scrollTo lands in the JS root-scroller state (the
    // bootstrap mirrors it to set_scroll_offset) — read it back so the band
    // paints the section the page is actually showing. Painting (0, 0)
    // unconditionally froze every scrolling timeline at the document top;
    // the intro video never noticed because its camera never moved.
    let (sx, sy) = page.scroll_offset();
    let t_band = Instant::now();
    let Some((frame, missing)) = page.viewport_band_frame(sx, sy, viewport) else {
        return Err(VideoError::NoLiveDocument);
    };
    let band_dt = t_band.elapsed();
    if missing.is_empty() {
        phases.record(eval_dt, band_dt, std::time::Duration::ZERO);
        return Ok((frame.width, frame.height, frame.rgba));
    }
    let t_img = Instant::now();
    page.fetch_band_images(missing).await;
    let img_dt = t_img.elapsed();
    let t_band = Instant::now();
    let Some((frame, _)) = page.viewport_band_frame(sx, sy, viewport) else {
        return Err(VideoError::NoLiveDocument);
    };
    phases.record(eval_dt, band_dt + t_band.elapsed(), img_dt);
    Ok((frame.width, frame.height, frame.rgba))
}

#[cfg(all(test, feature = "screenshot"))]
mod tests {
    use super::*;
    use crate::diting_browser::lifecycle::WaitUntil;
    use crate::diting_browser::{BrowserContext, Page as EnginePage};
    use std::net::TcpListener;
    use std::sync::Arc;

    /// Hand-written timeline stub: the same registry shape GSAP pages expose
    /// (duration() + pause(t) writing inline transform/opacity), a power2.out
    /// slide+fade — the pump test must not depend on fetching gsap.min.js.
    const STUB_HTML: &str = r#"<!doctype html><html><head><style>
html,body{margin:0;padding:0;width:800px;height:450px;overflow:hidden;background:#101418}
#box{position:absolute;left:40px;top:40px;width:200px;height:92px;background:rgb(255,92,0)}
#txt{position:absolute;left:44px;top:150px;color:#ffffff;font-size:24px}
</style></head><body>
<div id="box"></div><div id="txt">hello</div>
<script>
window.__timelines = { main: {
  duration: function () { return 2; },
  pause: function (t) {
    var u = Math.min(Math.max(t / 2, 0), 1);
    var e = 1 - Math.pow(1 - u, 3);
    var box = document.getElementById("box");
    box.style.transform = "translate3d(" + (-200 * (1 - e)).toFixed(4) + "px, 0px, 0px)";
    box.style.opacity = e.toFixed(4);
    var txt = document.getElementById("txt");
    txt.style.transform = "translate3d(0px, " + (5 * (1 - e)).toFixed(4) + "px, 0px)";
    txt.style.opacity = e.toFixed(4);
  }
}};
document.getElementById("box").style.transform = "translate3d(-200px, 0px, 0px)";
document.getElementById("box").style.opacity = "0";
</script></body></html>"#;

    fn spawn_html_server(html: &'static str) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut buf = [0u8; 1024];
                let _ = std::io::Read::read(&mut stream, &mut buf);
                let body = html.as_bytes();
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(body);
                let _ = stream.flush();
            }
        });
        port
    }

    fn test_page() -> EnginePage {
        let context = Arc::new(BrowserContext::with_storage_and_network(
            "video-test".into(),
            None,
            false,
            None,
            None,
            true, // allow_private_network: the fixture server is 127.0.0.1
            None,
        ));
        EnginePage::new("video-page".into(), context)
    }

    async fn navigated_stub_page() -> EnginePage {
        let port = spawn_html_server(STUB_HTML);
        let mut page = test_page();
        page.navigate_with_wait(
            &format!("http://127.0.0.1:{port}/stub.html"),
            WaitUntil::Load,
        )
        .await
        .expect("navigate stub fixture");
        page.settle_until_idle(5000).await;
        page
    }

    /// The invalidation chain the pump rides on: a style-attribute seek
    /// (no tree change, so the DomTree epoch is identical across seeks) must
    /// still repaint differently — the attr-level cache drop on set_attribute
    /// is what makes band_frame see fresh styles (the collection cache for
    /// transform/opacity, both caches for anything else).
    #[tokio::test(flavor = "current_thread")]
    async fn seek_to_band_frame_yields_distinct_frames() {
        let mut page = navigated_stub_page().await;
        let vp = (800.0, 450.0);
        let mut at = |t: f64| {
            page.evaluate(&format!(
                "window.__timelines.main.pause({t}); window.__timelines.main.pause({t}); undefined"
            ));
            page.viewport_band_frame(0.0, 0.0, vp).expect("band frame")
        };
        let (f0, _) = at(0.0);
        let (f1, _) = at(1.0);
        let (f2, _) = at(2.0);
        assert_eq!((f0.width, f0.height), (800, 450));
        assert_ne!(f0.rgba, f1.rgba, "mid-seek must repaint (epoch is equal — this is the attr invalidation)");
        assert_ne!(f1.rgba, f2.rgba, "end-seek must differ from mid");
        assert_ne!(f0.rgba, f2.rgba);
    }

    /// The #395 contract: a timeline seek writes only transform/opacity,
    /// so the frames repaint while the taffy solve count stays flat — the
    /// cached geometry is re-collected, never re-solved. A geometry write
    /// (width) must pay for a fresh solve again.
    #[tokio::test(flavor = "current_thread")]
    async fn paint_only_seek_reuses_the_geometry_solve() {
        let mut page = navigated_stub_page().await;
        let vp = (800.0, 450.0);
        let seek = |page: &mut EnginePage, t: f64| {
            page.evaluate(&format!(
                "window.__timelines.main.pause({t}); undefined"
            ));
            page.viewport_band_frame(0.0, 0.0, vp).expect("band frame")
        };
        // First paint solves (the elements' initial style writes were also
        // first-writes: no prior attribute to diff → full invalidation).
        let (f0, _) = seek(&mut page, 0.0);
        let after_first = page.layout_solve_count();
        assert!(after_first >= 1, "the first frame must have solved at least once");
        // Seeks differ in pixels…
        let (f1, _) = seek(&mut page, 1.0);
        let (f2, _) = seek(&mut page, 2.0);
        assert_ne!(f0.rgba, f1.rgba, "mid-seek repaints");
        assert_ne!(f1.rgba, f2.rgba, "end-seek repaints");
        // …but every one of them rode the cached solve.
        assert_eq!(
            page.layout_solve_count(),
            after_first,
            "paint-only seeks must not re-run the taffy solve"
        );
        // A geometry property write breaks the whitelist — full re-solve.
        page.evaluate("document.getElementById('box').style.width = '300px'; undefined");
        let (_, _) = seek(&mut page, 2.0);
        assert!(
            page.layout_solve_count() > after_first,
            "a width write must invalidate the geometry cache"
        );
    }

    /// Scrolling-camera fixture: pause(t) drives window.scrollTo down a
    /// two-screen document, the way the comparison video's camera does.
    const SCROLL_HTML: &str = r#"<!doctype html><html><head><style>
html,body{margin:0;padding:0;width:800px}
#top{width:800px;height:450px;background:rgb(16,20,24)}
#bot{width:800px;height:450px;background:rgb(80,200,60)}
</style></head><body>
<div id="top"></div><div id="bot"></div>
<script>
window.__timelines = { main: {
  duration: function () { return 2; },
  pause: function (t) {
    window.scrollTo(0, Math.round(450 * Math.min(Math.max(t / 2, 0), 1)));
  }
}};
</script></body></html>"#;

    /// A scrolling camera must paint the band the page scrolled to.
    /// seek_and_paint once passed (0, 0) to viewport_band_frame
    /// unconditionally, freezing every scrolling timeline at the document
    /// top — the intro video never caught it because its camera never moved.
    #[tokio::test(flavor = "current_thread")]
    async fn scrolling_camera_paints_the_band_the_page_scrolled_to() {
        let port = spawn_html_server(SCROLL_HTML);
        let mut page = test_page();
        page.navigate_with_wait(
            &format!("http://127.0.0.1:{port}/scroll.html"),
            WaitUntil::Load,
        )
        .await
        .expect("navigate scroll fixture");
        page.settle_until_idle(5000).await;
        let vp = (800.0, 450.0);
        let (_, _, top) = seek_and_paint(&mut page, vp, 0.0, &mut PumpPhases::default())
            .await
            .expect("frame at top");
        let (_, _, bot) = seek_and_paint(&mut page, vp, 2.0, &mut PumpPhases::default())
            .await
            .expect("frame at bottom");
        let px = |f: &[u8]| {
            let i = (225 * 800 + 400) * 4;
            (f[i], f[i + 1], f[i + 2])
        };
        assert_eq!(px(&top), (16, 20, 24), "camera at the top paints the dark screen");
        assert_eq!(px(&bot), (80, 200, 60), "camera scrolled down paints the lower screen");
    }

    /// End-to-end pump: stub timeline → in-process frames → ffmpeg pipe →
    /// MP4 bytes. 2 s @ 10 fps, no hold tail = 20 frames.
    #[tokio::test(flavor = "current_thread")]
    async fn render_stub_timeline_to_mp4() {
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let mut page = navigated_stub_page().await;
        let opts = TimelineVideoOptions {
            fps: 10.0,
            viewport: (800.0, 450.0),
            hold_tail_secs: 0.0,
            max_duration_secs: 30.0,
            wait_timelines: Duration::from_secs(5),
            ..Default::default()
        };
        let video = render_timeline_video(&mut page, &opts)
            .await
            .expect("pump renders");
        assert_eq!(video.frames, 20, "2s @ 10fps");
        assert_eq!(video.duration_secs, 2.0);
        // MP4 starts with a ftyp box: 4-byte size, then "ftyp".
        assert_eq!(&video.mp4[4..8], b"ftyp", "mp4 magic");
        assert!(!video.has_audio);
        assert!(!video.mp4.windows(4).any(|w| w == b"mp4a"), "no audio track when none requested");
        assert!(video.mp4.len() > 4_000, "a real encoded stream, not a bare header: {} bytes", video.mp4.len());
    }

    /// A minimal mono 16-bit PCM WAV of `secs` of 440 Hz — hand-rolled so the
    /// audio test carries no fixture file.
    fn sine_wav(secs: f64) -> Vec<u8> {
        let rate: u32 = 8000;
        let n = (rate as f64 * secs) as usize;
        let mut samples = Vec::with_capacity(n * 2);
        for i in 0..n {
            let t = i as f64 / rate as f64;
            let v = (t * 2.0 * std::f64::consts::PI * 440.0).sin() * 12000.0;
            samples.extend_from_slice(&(v as i16).to_le_bytes());
        }
        let data_len = samples.len() as u32;
        let mut w = Vec::with_capacity(44 + samples.len());
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&(36 + data_len).to_le_bytes());
        w.extend_from_slice(b"WAVEfmt ");
        w.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
        w.extend_from_slice(&1u16.to_le_bytes()); // PCM
        w.extend_from_slice(&1u16.to_le_bytes()); // mono
        w.extend_from_slice(&rate.to_le_bytes());
        w.extend_from_slice(&((rate * 2) as u32).to_le_bytes()); // byte rate
        w.extend_from_slice(&2u16.to_le_bytes()); // block align
        w.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
        w.extend_from_slice(b"data");
        w.extend_from_slice(&data_len.to_le_bytes());
        w.extend_from_slice(&samples);
        w
    }

    /// Serve `bytes` for every request on a fresh loopback port. The body is
    /// leaked into 'static — test-fixture lifetime.
    fn serve_bytes(content_type: &'static str, bytes: Vec<u8>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let bytes: &'static [u8] = Box::leak(bytes.into_boxed_slice());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut buf = [0u8; 1024];
                let _ = std::io::Read::read(&mut stream, &mut buf);
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    bytes.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(bytes);
                let _ = stream.flush();
            }
        });
        port
    }

    /// The audio half of the pump: a requested track is fetched, looped to
    /// cover the timeline, mixed at the asked volume/fade, and the MP4 comes
    /// back carrying an AAC track (mp4a sample entry) alongside H.264 — with
    /// the frame count and pinned duration unchanged.
    #[tokio::test(flavor = "current_thread")]
    async fn audio_track_muxes_aac_into_mp4() {
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let wav_port = serve_bytes("audio/wav", sine_wav(1.0));
        let mut page = navigated_stub_page().await;
        let opts = TimelineVideoOptions {
            fps: 10.0,
            viewport: (800.0, 450.0),
            hold_tail_secs: 0.0,
            max_duration_secs: 30.0,
            wait_timelines: Duration::from_secs(5),
            audio: Some(AudioTrack {
                url: format!("http://127.0.0.1:{wav_port}/bgm.wav"),
                volume: 0.7,
                fade_out_secs: 0.5,
                loop_audio: true,
            }),
            ..Default::default()
        };
        let video = render_timeline_video(&mut page, &opts)
            .await
            .expect("pump renders with audio");
        assert_eq!(video.frames, 20, "audio does not change the frame count");
        assert_eq!(video.duration_secs, 2.0, "-t pins the length");
        assert!(video.has_audio);
        assert_eq!(&video.mp4[4..8], b"ftyp", "mp4 magic");
        // The AAC sample entry only appears when a real audio track rode in.
        assert!(
            video.mp4.windows(4).any(|w| w == b"mp4a"),
            "mp4a sample entry present"
        );
        assert!(video.mp4.windows(4).any(|w| w == b"avc1"), "h264 still present");
    }

    /// A requested track that can't be fetched is an explicit error — never
    /// a silently silent video.
    #[tokio::test(flavor = "current_thread")]
    async fn unreachable_audio_is_an_error() {
        let mut page = navigated_stub_page().await;
        let opts = TimelineVideoOptions {
            wait_timelines: Duration::from_secs(5),
            audio: Some(AudioTrack {
                url: "http://127.0.0.1:9/nothing.wav".to_string(),
                volume: 1.0,
                fade_out_secs: 0.0,
                loop_audio: true,
            }),
            ..Default::default()
        };
        let err = render_timeline_video(&mut page, &opts).await.unwrap_err();
        assert!(matches!(err, VideoError::AudioFetchFailed(_)), "{err}");
    }

    /// Two SRT cues staged into the temp file the muxer reads.
    fn sample_srt() -> String {
        "1\n00:00:00,000 --> 00:00:01,000\nhello subtitle\n\n\
         2\n00:00:01,000 --> 00:00:02,000\nsecond cue\n"
            .to_string()
    }

    /// Timestamp forms the burn parser accepts: the SRT-mandated comma, the
    /// dot variant players in the wild emit, and trailing cue settings.
    #[test]
    fn srt_timestamp_parses_comma_dot_and_settings() {
        assert_eq!(parse_srt_timestamp("00:00:01,500"), Some(1.5));
        assert_eq!(parse_srt_timestamp("01:02:03.250"), Some(3723.25));
        assert_eq!(parse_srt_timestamp("00:00:10,000 x1:0 x2:100"), Some(10.0));
        assert_eq!(parse_srt_timestamp("garbage"), None);
        assert_eq!(parse_srt_timestamp("00:00"), None);
    }

    /// Malformed blocks and inverted windows are skipped, never fatal — the
    /// burn pass must not reject a video the soft-track path accepted.
    #[test]
    fn srt_parse_skips_malformed_blocks() {
        let hs = Hardsub::parse(
            "1\n00:00:00,000 --> 00:00:01,000\nhello\n\n\
             not a cue at all\n\n\
             2\n00:00:02,000 --> 00:00:03,000\nsecond\n",
        );
        assert_eq!(hs.cues.len(), 2);
        assert_eq!(hs.cues[0].text, "hello");
        assert_eq!(hs.cues[1].start, 2.0);
        assert!(Hardsub::parse("1\n00:00:05,000 --> 00:00:05,000\nx\n").is_empty());
    }

    /// The burn pass itself, pixel-level: on a flat dark frame the cue draws
    /// white glyph ink with black outline ink in the bottom band, and leaves
    /// the rest of the frame byte-identical.
    #[test]
    fn hardsub_paints_white_text_bottom_center() {
        let fonts = crate::diting_fonts::font_book();
        let (w, h) = (400u32, 240u32);
        let mut frame = vec![40u8; (w * h * 4) as usize];
        for px in frame.chunks_exact_mut(4) {
            px[3] = 255;
        }
        let mut hs = Hardsub::parse("1\n00:00:00,000 --> 00:00:02,000\n测试字幕\n");
        let pristine = frame.clone();
        hs.burn(&mut frame, w, h, 1.0, &fonts);
        // Outside the cue window nothing draws (same buffer, pristine copy).
        let mut outside = pristine.clone();
        hs.burn(&mut outside, w, h, 3.0, &fonts);
        assert_eq!(outside, pristine, "t past the cue burns nothing");

        let px = |f: &[u8], x: usize, y: usize| {
            let i = (y * w as usize + x) * 4;
            (f[i], f[i + 1], f[i + 2])
        };
        assert_eq!(px(&frame, 10, 10), (40, 40, 40), "top of frame untouched");
        let mut white = 0;
        let mut black = 0;
        for y in (h as usize * 3 / 4)..h as usize {
            for x in (w as usize / 4)..(w as usize * 3 / 4) {
                let p = px(&frame, x, y);
                if p.0 > 220 && p.1 > 220 && p.2 > 220 {
                    white += 1;
                }
                if p.0 < 60 && p.1 < 60 && p.2 < 60 {
                    black += 1;
                }
            }
        }
        assert!(white > 20, "white glyph ink in the subtitle band ({white} px)");
        assert!(black > 20, "black outline ink around it ({black} px)");
    }

    /// Probe the mp4's stream codecs with ffprobe; skips when ffprobe is
    /// absent (it ships with ffmpeg installs but the test must not require
    /// a second binary to prove the mux).
    fn ffprobe_codecs(mp4: &[u8]) -> Option<Vec<String>> {
        if Command::new("ffprobe").arg("-version").output().is_err() {
            eprintln!("skipping ffprobe leg: ffprobe not on PATH");
            return None;
        }
        let p = std::env::temp_dir().join(format!(
            "aginxbrowser-pump-probe-{}.mp4",
            std::process::id()
        ));
        std::fs::write(&p, mp4).ok()?;
        let out = Command::new("ffprobe")
            .arg("-v")
            .arg("error")
            .arg("-show_entries")
            .arg("stream=codec_name")
            .arg("-of")
            .arg("csv=p=0")
            .arg(&p)
            .output()
            .ok()?;
        let _ = std::fs::remove_file(&p);
        Some(
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(str::trim)
                .map(str::to_string)
                .collect(),
        )
    }

    /// Narration + BGM + subtitles together: the filter_complex amix path
    /// (two audio inputs, one delayed narration clip, looped BGM) plus the
    /// mov_text soft track — one MP4 carrying all three stream kinds with
    /// the frame count and pinned duration unchanged.
    #[tokio::test(flavor = "current_thread")]
    async fn narration_bgm_and_subtitles_ride_together() {
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let bgm_port = serve_bytes("audio/wav", sine_wav(1.0));
        let line_port = serve_bytes("audio/wav", sine_wav(0.6));
        let mut page = navigated_stub_page().await;
        let mut opts = TimelineVideoOptions {
            fps: 10.0,
            viewport: (800.0, 450.0),
            hold_tail_secs: 0.0,
            max_duration_secs: 30.0,
            wait_timelines: Duration::from_secs(5),
            audio: Some(AudioTrack {
                url: format!("http://127.0.0.1:{bgm_port}/bgm.wav"),
                volume: 0.2,
                fade_out_secs: 0.5,
                loop_audio: true,
            }),
            narration: vec![NarrationClip {
                url: format!("http://127.0.0.1:{line_port}/line1.wav"),
                start_secs: 1.0,
                volume: 1.0,
            }],
            subtitles_srt: Some(sample_srt()),
            subtitles_language: Some("eng".to_string()),
            ..Default::default()
        };
        let video = render_timeline_video(&mut page, &opts)
            .await
            .expect("pump renders narration+bgm+subs");
        assert_eq!(video.frames, 20, "audio does not change the frame count");
        assert_eq!(video.duration_secs, 2.0, "-t pins the length");
        assert!(video.has_audio);
        assert!(video.has_subtitles);
        assert!(video.burned_subtitles, "cues burn into the pixels by default");
        assert!(video.mp4.windows(4).any(|w| w == b"mp4a"), "AAC track");
        assert!(video.mp4.windows(4).any(|w| w == b"avc1"), "h264 track");
        if let Some(codecs) = ffprobe_codecs(&video.mp4) {
            assert!(codecs.iter().any(|c| c == "mov_text"), "soft subtitle stream: {codecs:?}");
            assert!(codecs.iter().any(|c| c == "aac"), "aac stream: {codecs:?}");
        }
        // Opt-out keeps the soft track without touching the pixels.
        opts.burn_subtitles = Some(false);
        let soft_only = render_timeline_video(&mut page, &opts)
            .await
            .expect("soft-only render");
        assert!(soft_only.has_subtitles);
        assert!(!soft_only.burned_subtitles);
    }

    /// A single narration clip rides the plain -af path (adelay places it) —
    /// exercises the filter syntax the amix path doesn't.
    #[tokio::test(flavor = "current_thread")]
    async fn single_narration_clip_delays_via_af() {
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let line_port = serve_bytes("audio/wav", sine_wav(0.8));
        let mut page = navigated_stub_page().await;
        let opts = TimelineVideoOptions {
            fps: 10.0,
            viewport: (800.0, 450.0),
            hold_tail_secs: 0.0,
            max_duration_secs: 30.0,
            wait_timelines: Duration::from_secs(5),
            narration: vec![NarrationClip {
                url: format!("http://127.0.0.1:{line_port}/line.wav"),
                start_secs: 1.2,
                volume: 0.9,
            }],
            ..Default::default()
        };
        let video = render_timeline_video(&mut page, &opts)
            .await
            .expect("pump renders single narration");
        assert!(video.has_audio);
        assert!(video.mp4.windows(4).any(|w| w == b"mp4a"), "AAC track");
        assert!(!video.has_subtitles);
    }

    /// A page with no timelines fails with the explicit error after the
    /// (shortened) deadline rather than hanging or panicking.
    #[tokio::test(flavor = "current_thread")]
    async fn no_timelines_is_an_error() {
        let port = spawn_html_server("<!doctype html><p>still</p>");
        let mut page = test_page();
        page.navigate_with_wait(&format!("http://127.0.0.1:{port}/x.html"), WaitUntil::Load)
            .await
            .unwrap();
        let opts = TimelineVideoOptions {
            wait_timelines: Duration::from_millis(200),
            ..Default::default()
        };
        let err = render_timeline_video(&mut page, &opts).await.unwrap_err();
        assert!(matches!(err, VideoError::NoTimelines), "{err}");
    }
}
