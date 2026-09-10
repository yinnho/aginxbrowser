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
//! `viewport_band_frame` re-cascades with the fresh inline styles.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::diting_browser::Page;

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
            VideoError::NoFfmpeg => write!(f, "ffmpeg not found on PATH"),
            VideoError::FfmpegFailed(tail) => write!(f, "ffmpeg failed: {tail}"),
            VideoError::FrameSizeMismatch { expect, got, frame } => {
                write!(f, "frame {frame} is {got} bytes, expected {expect}")
            }
        }
    }
}

impl std::error::Error for VideoError {}

pub struct TimelineVideoOptions {
    pub fps: f64,
    pub viewport: (f32, f32),
    /// Freeze the final timeline state for this many extra seconds.
    pub hold_tail_secs: f64,
    /// Safety cap on timeline + hold tail.
    pub max_duration_secs: f64,
    /// How long to wait for `window.__timelines` to appear.
    pub wait_timelines: Duration,
}

impl Default for TimelineVideoOptions {
    fn default() -> Self {
        Self {
            fps: 24.0,
            viewport: (1280.0, 720.0),
            hold_tail_secs: 0.5,
            max_duration_secs: 120.0,
            wait_timelines: Duration::from_secs(10),
        }
    }
}

/// Per-body cap for band-image fetches (matches the CDP band path).
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

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
    let (w, h, first) = seek_and_paint(page, viewport, 0.0).await?;
    let expect = w as usize * h as usize * 4;

    // MP4 muxing needs a seekable output, so the file goes to a temp path
    // (read back and removed below) while frames stream through stdin.
    static PUMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let out_path = std::env::temp_dir().join(format!(
        "aginxbrowser-pump-{}-{}.mp4",
        std::process::id(),
        PUMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let mut child = Command::new("ffmpeg")
        .arg("-hide_banner")
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
        .arg("pipe:0")
        .arg("-c:v")
        .arg("libx264")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-y")
        .arg(&out_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| VideoError::NoFfmpeg)?;
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
    send(first, 0)?;
    written += 1;
    for i in 1..frames {
        let t = i as f64 / opts.fps;
        let (_, _, rgba) = seek_and_paint(page, viewport, t).await?;
        send(rgba, i)?;
        written += 1;
    }
    // Closing stdin is ffmpeg's end-of-stream signal; the MP4 trailer lands
    // in the temp file after that.
    drop(stdin);
    let status = child.wait().map_err(|e| VideoError::FfmpegFailed(e.to_string()))?;
    let mp4 = std::fs::read(&out_path).unwrap_or_default();
    let _ = std::fs::remove_file(&out_path);
    if !status.success() {
        let tail: String = err_reader.join().unwrap_or_default().chars().rev().take(400).collect();
        return Err(VideoError::FfmpegFailed(tail.chars().rev().collect()));
    }
    Ok(TimelineVideo {
        mp4,
        frames: written,
        timeline_secs: timeline,
        duration_secs: total,
        width: w,
        height: h,
    })
}

/// Seek every registered timeline to `t`, then paint one viewport band. The
/// first pass may report missing image URLs — those fetch through the page's
/// own client and the band repaints before returning (placeholders only if a
/// fetch fails; a frame beats a stall).
async fn seek_and_paint(
    page: &mut Page,
    viewport: (f32, f32),
    t: f64,
) -> Result<(u32, u32, Vec<u8>), VideoError> {
    let seek = format!(
        "(() => {{ for (const k in window.__timelines) {{ \
         try {{ window.__timelines[k].pause({t:.4}); }} catch (e) {{}} }} }})()"
    );
    let _ = page.evaluate(&seek);
    let Some((frame, missing)) = page.viewport_band_frame(0.0, 0.0, viewport) else {
        return Err(VideoError::NoLiveDocument);
    };
    if missing.is_empty() {
        return Ok((frame.width, frame.height, frame.rgba));
    }
    fetch_missing_images(page, missing).await;
    let Some((frame, _)) = page.viewport_band_frame(0.0, 0.0, viewport) else {
        return Err(VideoError::NoLiveDocument);
    };
    Ok((frame.width, frame.height, frame.rgba))
}

/// Fetch the img bodies band paint is missing, through the page's own HTTP
/// client with the document as Referer. Same per-URL policy as the CDP band
/// path: SSRF gate, 3 s timeout, 200-only, ≤2 MiB.
async fn fetch_missing_images(page: &Page, urls: Vec<String>) {
    let base = page.url_string();
    for u in urls {
        let Ok(parsed) = url::Url::parse(&u) else { continue };
        if crate::diting_js::ops::validate_fetch_url(&parsed).is_err() {
            continue;
        }
        let fetched = tokio::time::timeout(
            Duration::from_secs(3),
            page.context.http_client.fetch_subresource(&parsed, Some(base.as_str())),
        )
        .await;
        if let Ok(Ok(resp)) = fetched {
            if resp.status == 200 && !resp.body.is_empty() && resp.body.len() <= MAX_BODY_BYTES {
                page.store_band_image(u, resp.body);
            }
        }
    }
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
    /// still repaint differently — drop_layout on set_attribute is what makes
    /// band_frame see fresh styles.
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
        };
        let video = render_timeline_video(&mut page, &opts)
            .await
            .expect("pump renders");
        assert_eq!(video.frames, 20, "2s @ 10fps");
        assert_eq!(video.duration_secs, 2.0);
        // MP4 starts with a ftyp box: 4-byte size, then "ftyp".
        assert_eq!(&video.mp4[4..8], b"ftyp", "mp4 magic");
        assert!(video.mp4.len() > 4_000, "a real encoded stream, not a bare header: {} bytes", video.mp4.len());
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
