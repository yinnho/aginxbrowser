//! The phone glass is this process's viewport — when a page is handed to us.
//!
//! 屏幕所有权 = /run/aginxbrowser/show.html 这个文件：
//!   在且非空  → 浏览器持屏：开 DRM（抢 master）、渲染、上屏；
//!   消失/清空 → 关掉 DRM fd（master 随 open 文件释放），空转等下一页。
//! 开机画面（term 首页）是默认持屏方；系统调起浏览器时先写页、再让位，
//! 浏览器这边抢到 master 就画。看完删页文件，term 回位轮询重新拿走。
//! 浏览器自己不画对话框、不画待机字标——那是 term 的事；这里只渲染
//! 被调起时交给我们的那一页。

#[cfg(feature = "screenshot")]
mod on {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;
    use std::time::Duration;

    use crate::browser::Browser;
    use crate::page::Page;
    use crate::panel_drm::Drm;

    /// 系统调起时写下的当前页（tmpl::open 原子写到这里）。也是所有权标记。
    const SHOW_PATH: &str = "/run/aginxbrowser/show.html";
    const TOUCH: &str = "/dev/input/event2";

    #[repr(C)]
    struct InputEvent {
        _sec: i64,
        _usec: i64,
        type_: u16,
        code: u16,
        value: i32,
    }

    const EV_ABS: u16 = 0x03;
    const ABS_MT_TRACKING_ID: u16 = 0x39;
    const ABS_MT_POSITION_X: u16 = 0x35;
    const ABS_MT_POSITION_Y: u16 = 0x36;

    enum Gesture {
        None,
        /// 竖向拖动增量（屏幕像素，向下为正）。
        Drag(f32),
        Tap(i32, i32),
        /// 松手时一整段的竖向位移（屏幕像素）。
        Lift(i32),
    }

    struct Touch {
        fd: std::fs::File,
        /// Screen pixels per raw sensor unit.
        sy: f32,
        sx: f32,
        down: bool,
        dragged: bool,
        /// Position of this touch has not arrived yet.
        fresh: bool,
        /// A Y was already seen in this packet, before tracking-id.
        y_in_frame: bool,
        x: i32,
        y: i32,
        start_y: i32,
        last_y: i32,
        origin_x: i32,
        origin_y: i32,
        placed: bool,
    }

    fn abs_span(fd: i32, code: u16) -> Option<i32> {
        #[repr(C)]
        struct AbsInfo {
            _value: i32,
            minimum: i32,
            maximum: i32,
            _fuzz: i32,
            _flat: i32,
            _resolution: i32,
        }
        // EVIOCGABS(code) = _IOR('E', 0x40 + code, input_absinfo)
        let nr = 0x40u32 + code as u32;
        let ioc = (2u32 << 30)
            | ((std::mem::size_of::<AbsInfo>() as u32) << 16)
            | ((b'E' as u32) << 8)
            | nr;
        let mut info = AbsInfo {
            _value: 0,
            minimum: 0,
            maximum: 0,
            _fuzz: 0,
            _flat: 0,
            _resolution: 0,
        };
        if unsafe { libc::ioctl(fd, ioc as _, &mut info) } != 0 {
            return None;
        }
        let span = info.maximum - info.minimum;
        if span > 1 { Some(span) } else { None }
    }

    impl Touch {
        fn open(screen_w: i32, screen_h: i32) -> Option<Touch> {
            let fd = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
                .open(TOUCH)
                .ok()?;
            let raw = fd.as_raw_fd();
            let sx = abs_span(raw, ABS_MT_POSITION_X)
                .map(|s| screen_w as f32 / s as f32)
                .unwrap_or(1.0);
            let sy = abs_span(raw, ABS_MT_POSITION_Y)
                .map(|s| screen_h as f32 / s as f32)
                .unwrap_or(1.0);
            tracing::info!("panel: touch scale {sx:.3} {sy:.3}");
            Some(Touch {
                fd,
                sy,
                sx,
                down: false,
                dragged: false,
                fresh: true,
                y_in_frame: false,
                x: 0,
                y: 0,
                start_y: 0,
                last_y: 0,
                origin_x: 0,
                origin_y: 0,
                placed: false,
            })
        }

        fn screen_xy(&self) -> (i32, i32) {
            (
                (self.x as f32 * self.sx) as i32,
                (self.y as f32 * self.sy) as i32,
            )
        }

        fn note_origin(&mut self) {
            if self.placed {
                return;
            }
            let (x, y) = self.screen_xy();
            self.origin_x = x;
            self.origin_y = y;
            self.placed = true;
        }

        fn poll(&mut self) -> Gesture {
            let mut buf = [0u8; 24 * 16];
            let n = match self.fd.read(&mut buf) {
                Ok(n) => n,
                Err(_) => return Gesture::None,
            };
            // Kernel input_event is 24 bytes. repr(C) pads the struct to
            // its alignment, so size_of must not be used to slice the stream.
            let mut drag = 0f32;
            let mut saw_drag = false;
            let mut tap: Option<(i32, i32)> = None;
            let mut lift: Option<i32> = None;
            self.y_in_frame = false;
            for chunk in buf[..n].chunks_exact(24) {
                let ev = unsafe { std::ptr::read_unaligned(chunk.as_ptr() as *const InputEvent) };
                match (ev.type_, ev.code) {
                    (EV_ABS, ABS_MT_TRACKING_ID) => {
                        if ev.value < 0 {
                            if self.down {
                                let (x, y) = self.screen_xy();
                                if !self.dragged {
                                    tap = Some((x, y));
                                } else {
                                    lift = Some(y - self.origin_y);
                                }
                            }
                            self.down = false;
                            self.dragged = false;
                            self.fresh = true;
                            self.placed = false;
                        } else {
                            self.down = true;
                            self.dragged = false;
                            self.placed = false;
                            if self.y_in_frame {
                                self.start_y = self.y;
                                self.last_y = self.y;
                                self.fresh = false;
                                self.note_origin();
                            } else {
                                // tracking-id arrived before this touch's Y.
                                // Anchoring from the previous Y reads as a
                                // jump across the screen.
                                self.fresh = true;
                            }
                        }
                    }
                    (EV_ABS, ABS_MT_POSITION_X) => {
                        self.x = ev.value;
                        if self.down {
                            self.note_origin();
                            let sx = (ev.value as f32 * self.sx) as i32;
                            if (sx - self.origin_x).abs() > 36 {
                                self.dragged = true;
                            }
                        }
                    }
                    (EV_ABS, ABS_MT_POSITION_Y) => {
                        self.y = ev.value;
                        self.y_in_frame = true;
                        if self.fresh {
                            self.fresh = false;
                            self.start_y = ev.value;
                            self.last_y = ev.value;
                            self.note_origin();
                        } else if self.down {
                            let dy = self.y - self.last_y;
                            if (self.y - self.start_y).abs() > 30 {
                                self.dragged = true;
                            }
                            if self.dragged && dy != 0 {
                                drag += dy as f32 * self.sy;
                                saw_drag = true;
                                self.last_y = self.y;
                            }
                        }
                    }
                    _ => {}
                }
            }
            if let Some(dy) = lift {
                Gesture::Lift(dy)
            } else if saw_drag {
                Gesture::Drag(drag)
            } else if let Some((x, y)) = tap {
                Gesture::Tap(x, y)
            } else {
                Gesture::None
            }
        }
    }

    /// 盯的页。env 覆盖仅供 host 试验；机上永远是 SHOW_PATH。
    fn watch_path() -> String {
        if let Ok(p) = std::env::var("AGINXBROWSER_PANEL_HTML") {
            if !p.is_empty() {
                return p;
            }
        }
        SHOW_PATH.to_string()
    }

    /// 页在且非空才持屏。空文件不算——写页方是 tmp+rename 原子写，正常
    /// 不会出现半截；万一有，空页当「没页」处理，别拿白屏霸住屏。
    fn current_html(path: &str) -> Option<String> {
        std::fs::read_to_string(path)
            .ok()
            .filter(|s| !s.trim().is_empty())
    }

    fn file_stamp(path: &str) -> Option<std::time::SystemTime> {
        std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
    }

    fn data_url(html: &str) -> String {
        let mut enc = String::from("data:text/html;charset=utf-8,");
        for b in html.as_bytes() {
            match *b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    enc.push(*b as char);
                }
                _ => enc.push_str(&format!("%{b:02X}")),
            }
        }
        enc
    }

    async fn load_html(page: &mut Page, w: u32, h: u32, html: &str) -> Result<(), String> {
        page.set_viewport_override(w as f32, h as f32, true, None);
        page.goto(&data_url(html)).await.map_err(|e| e.to_string())?;
        Ok(())
    }

    /// The whole page, already turned into screen pixels. Scrolling copies
    /// a window out of this; it does not rasterize text again.
    struct PageCache {
        px: Vec<u32>,
        w: usize,
        h: usize,
    }

    fn put_band(cache: &mut PageCache, y0: usize, rgba: &[u8], fw: u32, fh: u32) {
        let w = (fw as usize).min(cache.w);
        let rows = (fh as usize).min(cache.h.saturating_sub(y0));
        let stride = fw as usize * 4;
        if rgba.len() < rows * stride {
            return;
        }
        for row in 0..rows {
            let src = &rgba[row * stride..];
            let dst = &mut cache.px[(y0 + row) * cache.w..(y0 + row) * cache.w + w];
            for x in 0..w {
                let i = x * 4;
                dst[x] = ((src[i] as u32) << 16) | ((src[i + 1] as u32) << 8) | src[i + 2] as u32;
            }
        }
    }

    async fn band(page: &mut Page, y: f32, vp: (f32, f32)) -> Result<diting::diting_js::ops::BandFrame, String> {
        let Some((frame, missing)) = page.inner.viewport_band_frame(0.0, y, vp) else {
            return Err("no band frame".into());
        };
        if missing.is_empty() {
            return Ok(frame);
        }
        page.inner.fetch_band_images(missing).await;
        page.inner
            .viewport_band_frame(0.0, y, vp)
            .map(|(f, _)| f)
            .ok_or_else(|| "no band frame".to_string())
    }

    async fn build_cache(page: &mut Page, vw: u32, vh: u32) -> Result<PageCache, String> {
        let t0 = std::time::Instant::now();
        let vp = (vw as f32, vh as f32);
        let first = band(page, 0.0, vp).await?;
        let content = first.content_size.1.ceil().max(vh as f32) as usize;
        let h = content.min(vh as usize * 6);
        let mut cache = PageCache {
            px: vec![0u32; vw as usize * h],
            w: vw as usize,
            h,
        };
        put_band(&mut cache, first.dy.round().max(0.0) as usize, &first.rgba, first.width, first.height);
        let mut y = vh as f32;
        let max_origin = (h as f32 - vh as f32).max(0.0);
        while y <= max_origin + 0.5 {
            let frame = band(page, y, vp).await?;
            let origin = frame.dy.round().max(0.0) as usize;
            put_band(&mut cache, origin, &frame.rgba, frame.width, frame.height);
            if frame.dy + 1.0 >= max_origin {
                break;
            }
            y += vh as f32;
        }
        tracing::info!(
            "panel: cached {}x{} in {}ms",
            cache.w,
            cache.h,
            t0.elapsed().as_millis()
        );
        Ok(cache)
    }

    fn show(drm: &mut Drm, cache: &PageCache, scroll: f32) {
        let w = drm.width as usize;
        let h = drm.height as usize;
        let max_y = cache.h.saturating_sub(h);
        let y0 = (scroll.round().max(0.0) as usize).min(max_y);
        let pitch = drm.pitch_px();
        let buf = drm.back_buf();
        for row in 0..h {
            let s = (y0 + row) * cache.w;
            let d = row * pitch;
            buf[d..d + w].copy_from_slice(&cache.px[s..s + w]);
        }
    }

    /// 抢屏。term 让位有先后（它的让位动作和我们的取屏是两笔），
    /// master busy / 屏幕节点未出都重试到拿到为止——不设上限：面板是
    /// 唯一能把这页画上去的进程，等到了就是它的事。
    async fn take_screen() -> Result<Drm, String> {
        let mut logged = false;
        loop {
            match Drm::open() {
                Ok(d) => return Ok(d),
                Err(e) if e == "master busy" || e.contains("No such file") => {
                    if !logged {
                        tracing::info!("panel: waiting for the screen ({e})");
                        logged = true;
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn run() -> Result<(), String> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        rt.block_on(async {
            let path = watch_path();
            // 引擎和触摸各建一次，跨让位/回位循环复用：diting 冷启是秒级，
            // 每次回屏重建会把「点卡片 → 出页」拖回秒等。
            let mut engine: Option<(Browser, Page)> = None;
            let mut touch: Option<Touch> = None;
            loop {
                let Some(html) = current_html(&path) else {
                    // 没页 = 开机画面（term）的屏。系统调起（写页）前不动。
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    continue;
                };
                let mut drm = take_screen().await?;
                let (w, h) = (drm.width, drm.height);
                tracing::info!("panel: took the screen {w}x{h}");
                if engine.is_none() {
                    let b = Browser::builder().build().map_err(|e| e.to_string())?;
                    let p = b.new_page().await.map_err(|e| e.to_string())?;
                    engine = Some((b, p));
                }
                if touch.is_none() {
                    touch = Touch::open(w as i32, h as i32);
                    if touch.is_none() {
                        tracing::warn!("panel: no {TOUCH}, swipes and taps are dead");
                    }
                }
                let page = &mut engine.as_mut().expect("engine built above").1;
                load_html(page, w, h, &html).await?;
                let mut cache = build_cache(page, w, h).await?;
                let mut scroll = 0f32;
                let mut max = (cache.h as f32 - h as f32).max(0.0);
                show(&mut drm, &cache, scroll);
                drm.initial_modeset().map_err(|e| e.to_string())?;
                tracing::info!("panel: showing the page");
                // 持屏内环：页没了 = 交屏；mtime 变了 = 重画；拖动滚、点链接走。
                let mut seen = file_stamp(&path);
                loop {
                    if current_html(&path).is_none() {
                        tracing::info!("panel: page gone, releasing the screen");
                        break;
                    }
                    let now_stamp = file_stamp(&path);
                    if now_stamp != seen {
                        seen = now_stamp;
                        if let Some(html) = current_html(&path) {
                            load_html(page, w, h, &html).await?;
                            cache = build_cache(page, w, h).await?;
                            scroll = 0.0;
                            max = (cache.h as f32 - h as f32).max(0.0);
                            show(&mut drm, &cache, scroll);
                            drm.present();
                            tracing::info!("panel: new page");
                        }
                        continue;
                    }
                    let gesture = touch.as_mut().map(|t| t.poll()).unwrap_or(Gesture::None);
                    match gesture {
                        Gesture::Lift(dy) => {
                            let next = (scroll - dy as f32).clamp(0.0, max);
                            if (next - scroll).abs() >= 1.0 {
                                scroll = next;
                                show(&mut drm, &cache, scroll);
                                drm.present();
                            }
                        }
                        Gesture::Drag(dy) => {
                            let next = (scroll - dy).clamp(0.0, max);
                            if (next - scroll).abs() >= 1.0 {
                                scroll = next;
                                show(&mut drm, &cache, scroll);
                                drm.present();
                            }
                        }
                        Gesture::Tap(x, y) => {
                            let expr = format!(
                                "(function(){{var e=document.elementFromPoint({x},{y});if(!e||!e.closest)return '';var a=e.closest('a');return a&&a.href?a.href:'';}})()"
                            );
                            let href = page.evaluate(&expr);
                            if let Some(url) = href.as_str().filter(|s| s.starts_with("http")) {
                                tracing::info!("panel: open {url}");
                                if page.goto(url).await.is_ok() {
                                    cache = build_cache(page, w, h).await?;
                                    scroll = 0.0;
                                    max = (cache.h as f32 - h as f32).max(0.0);
                                    show(&mut drm, &cache, scroll);
                                    drm.present();
                                }
                            }
                        }
                        Gesture::None => {
                            let fd = touch.as_ref().map(|t| t.fd.as_raw_fd()).unwrap_or(-1);
                            if fd >= 0 {
                                let mut pfd = libc::pollfd {
                                    fd,
                                    events: libc::POLLIN,
                                    revents: 0,
                                };
                                unsafe { libc::poll(&mut pfd, 1, 80) };
                            } else {
                                tokio::time::sleep(Duration::from_millis(80)).await;
                            }
                        }
                    }
                }
                // drm 弃置 = 关 fd = 交还 DRM master（master 按 open 文件
                // 记账）。term 的回位轮询会重新拿走。
            }
        })
    }

    pub fn start() {
        std::thread::Builder::new()
            .name("aginxbrowser-panel".into())
            .spawn(|| {
                if let Err(e) = run() {
                    tracing::error!("panel: {e}");
                }
            })
            .ok();
    }
}

#[cfg(feature = "screenshot")]
pub use on::start;

#[cfg(not(feature = "screenshot"))]
pub fn start() {
    tracing::warn!("panel: this build has no paint path (rebuild with --features screenshot)");
}
