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
    // Element-level cfg keeps the list feature-true without mut-ness.
    let feats: Vec<&str> = vec![
        #[cfg(feature = "screenshot")]
        "screenshot",
        #[cfg(feature = "blitz-reference")]
        "blitz-reference",
        #[cfg(feature = "stealth")]
        "stealth",
    ];
    let detail = if feats.is_empty() {
        "none (plain build: no /screenshot, no stealth TLS)".to_string()
    } else {
        format!("{} — rebuild with --features stealth,screenshot if missing", feats.join("+"))
    };
    check(Status::Ok, "features", detail)
}

/// The robots.txt stance is a product decision made visible: it defaults to
/// skipped (real-time acquisition layer, not a crawler), and the one env
/// that turns it on should show up in doctor so a misconfigured instance
/// explains itself.
fn robots_check() -> Check {
    if std::env::var("AGINXBROWSER_HONOR_ROBOTS")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        check(Status::Ok, "robots", "HONOR_ROBOTS set — /fetch, /screenshot and /download check robots.txt (operator opt-in)")
    } else {
        check(Status::Ok, "robots", "robots.txt not consulted by default (AGINXBROWSER_HONOR_ROBOTS=1 to opt in)")
    }
}

/// Probe the bundled CJK bundle the way paint consumes it — ink, not a cmap
/// dump (the batch-3b lesson: .notdef advances a full em while the raster
/// stays empty, so the ink check is the one that bites).
#[cfg(feature = "screenshot")]
fn fonts_check() -> Check {
    let book = diting::diting_fonts::font_book();
    let raster = book.rasterize("汉字Abc", 24.0, false, [0, 0, 0, 255], 24.0 * 1.2, false, None);
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

/// Pull the version token out of `ffmpeg -version`'s first line
/// ("ffmpeg version 7.1.1 Copyright ..." → "7.1.1"; distro suffixes like
/// "4.4.2-0ubuntu0.22.04.1" survive). Pure half, unit-tested.
fn ffmpeg_version_word(first_line: &str) -> Option<&str> {
    let after = first_line.split("version ").nth(1)?;
    after.split_whitespace().next()
}

/// `/video` + `render_video` pipe frames into an external ffmpeg — the only
/// capability with an external binary dependency. The agent-browser #2011
/// lesson: a doctor that stays silent about ffmpeg reports "healthy" on a
/// box where every recording dies with EPIPE. Missing ffmpeg is a warning,
/// not a failure — everything except video keeps working, and systemd
/// shouldn't gate startup on it.
fn ffmpeg_check() -> Check {
    match std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
    {
        Ok(out) if out.status.success() => {
            let head = String::from_utf8_lossy(&out.stdout);
            let first = head.lines().next().unwrap_or("");
            let detail = match ffmpeg_version_word(first) {
                Some(v) => format!("ffmpeg {v} on PATH — /video ready"),
                None => format!("ffmpeg on PATH — /video ready ({})", first.chars().take(60).collect::<String>()),
            };
            check(Status::Ok, "ffmpeg", detail)
        }
        _ => check(
            Status::Warn,
            "ffmpeg",
            "not found on PATH — /video and render_video cannot produce MP4s; \
             all other capabilities unaffected (brew install ffmpeg / apt install ffmpeg)",
        ),
    }
}

/// The `/doctor` HTTP face of [`ffmpeg_check`]: `null` when absent (with the
/// reason), `{"version": ".."}` when found. Same one-shot `ffmpeg -version`
/// probe, shaped for agents deciding whether to call `render_video`.
pub fn ffmpeg_probe_json() -> serde_json::Value {
    match std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
    {
        Ok(out) if out.status.success() => {
            let first = String::from_utf8_lossy(&out.stdout);
            let first = first.lines().next().unwrap_or("");
            match ffmpeg_version_word(first) {
                Some(v) => serde_json::json!({ "version": v }),
                None => serde_json::Value::Null,
            }
        }
        _ => serde_json::Value::Null,
    }
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
        Ok(p) if !p.is_empty() => out.push(check(Status::Info, "proxy", p)),
        _ => {
            out.push(check(Status::Info, "proxy", "none — direct-first, auto-fallback per fetch"));
            // The engine ignores standard proxy env vars (every client pins
            // the implicit matcher off); say so before a shell proxy looks
            // "configured" while fetches go direct.
            if let Some(env) = crate::config::standard_proxy_env() {
                out.push(check(
                    Status::Info,
                    "proxy",
                    format!("{env} set — ignored by the engine; set AGINXBROWSER_PROXY to use it"),
                ));
            }
        }
    }
    if std::env::var_os("AGINXBROWSER_ALLOW_PRIVATE_NETWORK").is_some() {
        out.push(check(
            Status::Warn,
            "ssrf",
            "AGINXBROWSER_ALLOW_PRIVATE_NETWORK is set: private/loopback URLs become fetchable — dev only",
        ));
    }
    if diting::diting_net::client::allow_file_access() {
        out.push(check(
            Status::Warn,
            "file-access",
            "file:// reads are enabled (--allow-file-access / AGINXBROWSER_ALLOW_FILE_ACCESS): \
             any client that reaches this port can read local files the process can",
        ));
    }
    out
}

/// One real HTTPS round trip — the difference between "instance broken" and
/// "network blocked". 10s cap so a firewalled box doesn't hang the doctor.
async fn egress_check() -> Check {
    let started = Instant::now();
    // Same client policy as the engine (no implicit env proxy) so the check
    // answers the question that matters: can the ENGINE reach the web as
    // configured — not "does this shell have a working proxy".
    let client = match diting::diting_net::client::reqwest_builder_no_env_proxy()
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
    let mut checks = vec![features_check(), fonts_check(), ffmpeg_check(), robots_check()];
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

    #[test]
    fn ffmpeg_version_word_extracts_token() {
        assert_eq!(
            ffmpeg_version_word("ffmpeg version 7.1.1 Copyright (c) 2000-2025 the FFmpeg developers"),
            Some("7.1.1")
        );
        // Distro builds carry package suffixes — keep the whole token.
        assert_eq!(
            ffmpeg_version_word("ffmpeg version 4.4.2-0ubuntu0.22.04.1 Copyright ..."),
            Some("4.4.2-0ubuntu0.22.04.1")
        );
        // Non-standard first lines (static builds sometimes brand differently).
        assert_eq!(ffmpeg_version_word("ffmpeg version n7.1-latest"), Some("n7.1-latest"));
        assert_eq!(ffmpeg_version_word("garbage with no marker"), None);
        assert_eq!(ffmpeg_version_word(""), None);
    }

    #[test]
    fn ffmpeg_check_reports_at_least_warn() {
        // The check runs the real PATH: found → Ok with the version named,
        // missing → Warn naming /video as the only casualty. Either way the
        // doctor no longer stays silent about the one external dependency.
        let c = ffmpeg_check();
        assert!(matches!(c.status, Status::Ok | Status::Warn));
        assert!(c.detail.contains("video"), "{}", c.detail);
    }
}
