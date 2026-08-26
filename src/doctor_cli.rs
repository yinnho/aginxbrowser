//! `aginxbrowser doctor` — standalone self-check for self-hosters.
//!
//! Runs BEFORE the server boots (and before the V8 warmup in main): binary
//! capabilities, the bundled font supply, environment posture, and one live
//! egress probe — the four things that decide "why doesn't my instance
//! fetch/screenshot anything". Human-readable output; exit code 1 iff a
//! hard check failed, so scripts and containers can gate on it.

use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Ok,
    Info,
    Warn,
    Fail,
}

impl Status {
    fn tag(self) -> &'static str {
        match self {
            Status::Ok => "[ok]",
            Status::Info => "[info]",
            Status::Warn => "[warn]",
            Status::Fail => "[fail]",
        }
    }
}

pub struct Check {
    pub status: Status,
    pub name: &'static str,
    pub detail: String,
}

fn check(status: Status, name: &'static str, detail: impl Into<String>) -> Check {
    Check { status, name, detail: detail.into() }
}

/// Compiled-in features — the same booleans `/doctor` reports over HTTP,
/// readable without a listener (self-hosters behind firewalls especially).
fn features_check() -> Check {
    let mut feats: Vec<&str> = Vec::new();
    #[cfg(feature = "screenshot")]
    feats.push("screenshot");
    #[cfg(feature = "stealth")]
    feats.push("stealth");
    let detail = if feats.is_empty() {
        "none (plain build: no /screenshot, no stealth TLS)".to_string()
    } else {
        format!("{} — rebuild with --features stealth,screenshot if missing", feats.join("+"))
    };
    check(Status::Ok, "features", detail)
}

/// Probe the bundled CJK bundle the way paint consumes it — ink, not a cmap
/// dump (the batch-3b lesson: .notdef advances a full em while the raster
/// stays empty, so the ink check is the one that bites).
#[cfg(feature = "screenshot")]
fn fonts_check() -> Check {
    let book = crate::diting_fonts::font_book();
    let raster = book.rasterize("汉字Abc", 24.0, false, [0, 0, 0, 255], 24.0 * 1.2);
    if raster.ink_bbox().is_some() {
        check(Status::Ok, "fonts", "bundled CJK bundle inks 汉字 (GB2312 + symbols)")
    } else {
        check(Status::Fail, "fonts", "bundled fonts parse but rasterize no ink — bundle corrupt")
    }
}

/// The font bundle ships with the screenshot feature; a plain build can't
/// rasterize at all (and /screenshot 404s) — that's the thing to surface.
#[cfg(not(feature = "screenshot"))]
fn fonts_check() -> Check {
    check(Status::Warn, "fonts", "screenshot feature off — no bundled fonts, /screenshot unavailable")
}

fn env_checks() -> Vec<Check> {
    let mut out = Vec::new();
    let bind = std::env::var("AGINXBROWSER_BIND").unwrap_or_else(|_| "0.0.0.0:8089".into());
    out.push(check(
        Status::Info,
        "bind",
        format!("{bind} (env AGINXBROWSER_BIND; prefer 127.0.0.1 behind a proxy)"),
    ));
    match std::env::var("AGINXBROWSER_PROXY") {
        Ok(p) if !p.is_empty() => out.push(check(Status::Info, "proxy", format!("{p}"))),
        _ => out.push(check(Status::Info, "proxy", "none — direct-first, auto-fallback per fetch")),
    }
    if std::env::var_os("AGINXBROWSER_ALLOW_PRIVATE_NETWORK").is_some() {
        out.push(check(
            Status::Warn,
            "ssrf",
            "AGINXBROWSER_ALLOW_PRIVATE_NETWORK is set: private/loopback URLs become fetchable — dev only",
        ));
    }
    out
}

/// One real HTTPS round trip — the difference between "instance broken" and
/// "network blocked". 10s cap so a firewalled box doesn't hang the doctor.
async fn egress_check() -> Check {
    let started = Instant::now();
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent(format!("aginxbrowser-doctor/{}", env!("CARGO_PKG_VERSION")))
        .build()
    {
        Ok(c) => c,
        Err(e) => return check(Status::Fail, "egress", format!("client build failed: {e}")),
    };
    match client.get("https://example.com").send().await {
        Ok(resp) => {
            let ms = started.elapsed().as_millis();
            check(
                Status::Ok,
                "egress",
                format!("https://example.com → {} in {ms}ms", resp.status().as_u16()),
            )
        }
        Err(e) => check(
            Status::Fail,
            "egress",
            format!("https://example.com unreachable: {e} — check DNS/firewall, or set AGINXBROWSER_PROXY"),
        ),
    }
}

/// Pure renderer, unit-tested: aligned two-column lines, then a summary
/// footer with the exit-relevant counts.
pub fn render(checks: &[Check]) -> String {
    let mut out = String::new();
    out.push_str(&format!("aginxbrowser {} doctor\n\n", env!("CARGO_PKG_VERSION")));
    let width = checks.iter().map(|c| c.name.len()).max().unwrap_or(0);
    for c in checks {
        out.push_str(&format!(
            "  {:<7}{:<width$}  {}\n",
            c.status.tag(),
            c.name,
            c.detail,
            width = width
        ));
    }
    let fails = checks.iter().filter(|c| c.status == Status::Fail).count();
    let warns = checks.iter().filter(|c| c.status == Status::Warn).count();
    out.push('\n');
    if fails > 0 {
        out.push_str(&format!("{fails} failed, {warns} warning(s). "));
    } else {
        out.push_str(&format!("all checks passed ({warns} warning(s)). "));
    }
    out.push_str("start the server: aginxbrowser\n");
    out
}

/// Entry point from main's arg dispatch. Returns the process exit code.
pub async fn run() -> i32 {
    let mut checks = vec![features_check(), fonts_check()];
    checks.extend(env_checks());
    checks.push(egress_check().await);
    let code = if checks.iter().any(|c| c.status == Status::Fail) { 1 } else { 0 };
    print!("{}", render(&checks));
    println!("Like it? ⭐ Star us → https://github.com/yinnho/aginxbrowser");
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_aligns_and_counts_failures() {
        let checks = vec![
            check(Status::Ok, "features", "screenshot+stealth"),
            check(Status::Warn, "ssrf", "private net allowed"),
            check(Status::Fail, "egress", "unreachable"),
        ];
        let text = render(&checks);
        assert!(text.contains("[ok]   "), "status tags padded: {}", text);
        assert!(text.contains("1 failed, 1 warning(s)."), "summary counts: {}", text);
        // Name column aligned to the longest name.
        let ok_line = text.lines().find(|l| l.contains("features")).unwrap();
        let ssrf_line = text.lines().find(|l| l.contains("ssrf")).unwrap();
        assert_eq!(ok_line.find("screenshot"), ssrf_line.find("private"));
    }

    #[test]
    fn render_all_pass_has_no_fail_summary() {
        let checks = vec![check(Status::Ok, "features", "x"), check(Status::Info, "proxy", "none")];
        let text = render(&checks);
        assert!(text.contains("all checks passed"), "{}", text);
        assert!(!text.contains("failed"), "{}", text);
    }

    #[test]
    #[cfg(feature = "screenshot")]
    fn bundled_fonts_ink() {
        assert_eq!(fonts_check().status, Status::Ok);
    }
}
