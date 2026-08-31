//! Streaming HTTP(S) download layer — self-developed, zero new dependencies.
//!
//! Reuses diting_net's HttpClient primitives (cookie jar, proxy, SSRF guard)
//! but streams the body chunk-by-chunk to disk instead of buffering it in
//! memory, computing SHA-256 incrementally along the way. Supports Range-based
//! resume (206 append / 200 restart), Content-Disposition filename discovery,
//! and a chunk-level stall timeout so a dead connection cannot hang forever.

use anyhow::{anyhow, Context, Result};
use futures_util::StreamExt;
use reqwest::header::{ACCEPT, CONTENT_DISPOSITION, COOKIE, LOCATION, RANGE, USER_AGENT};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use url::Url;

use crate::config::proxy_from_env;
use crate::diting_net::client::validate_url;
use crate::diting_net::{CookieJar, HttpClient};
use crate::config::should_auto_proxy;

/// Per-chunk stall budget: if no bytes arrive for this long, give up instead
/// of hanging forever (dead NAT mapping, half-closed proxy socket).
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REDIRECTS: usize = 20;
const MAX_FILENAME_LEN: usize = 180;
/// Hard ceiling on total bytes written to disk per call. A runaway URL must
/// never be able to fill the disk; genuinely large files fit well under this.
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const DEFAULT_FILENAME: &str = "download";

#[derive(Debug, Clone, serde::Deserialize)]
pub struct DownloadRequest {
    pub url: String,
    /// Explicit output filename. When omitted: Content-Disposition → URL tail
    /// → "download".
    #[serde(default)]
    pub filename: Option<String>,
    /// Resume an interrupted download when a local partial file exists
    /// (Range: bytes=N- → 206 append, or 200 → restart).
    #[serde(default)]
    pub resume: bool,
    /// Route through AGINXBROWSER_PROXY (auto-enabled for known blocked domains).
    #[serde(default)]
    pub use_proxy: bool,
    /// Set-Cookie-style entries ("name=value") scoped to the target host.
    #[serde(default)]
    pub cookies: Vec<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct DownloadResponse {
    /// Final URL after redirects.
    pub url: String,
    /// Absolute path of the completed file on disk.
    pub path: String,
    pub filename: String,
    /// Total bytes written to the file by THIS call (append counts only the
    /// appended portion; 0 when the file was already complete).
    pub size_bytes: u64,
    pub content_type: Option<String>,
    /// SHA-256 over the complete file content on disk.
    pub sha256: String,
    /// True when an existing partial file was continued via Range/206.
    pub resumed: bool,
}

/// Stream-download `req.url` to the download directory.
///
/// Runs entirely on Tokio (no V8 state involved), safe to await directly from
/// the axum runtime.
pub async fn do_download(req: DownloadRequest) -> Result<DownloadResponse> {
    crate::rate::check_domain(&req.url).map_err(anyhow::Error::msg)?;
    let start_url =
        Url::parse(req.url.trim()).with_context(|| format!("invalid url: {}", req.url))?;
    ensure_http_scheme(&start_url)?;
    // Same SSRF policy as /fetch: loopback/RFC1918/link-local rejected unless
    // AGINXBROWSER_ALLOW_PRIVATE_NETWORK is opted in. Never a request param.
    validate_url(&start_url, false)?;

    let proxy = if req.use_proxy || should_auto_proxy(&req.url) {
        proxy_from_env()
    } else {
        None
    };
    let jar = Arc::new(CookieJar::new());
    for raw in &req.cookies {
        jar.set_cookie(raw.trim(), &start_url);
    }
    let client = HttpClient::with_full_options(jar.clone(), proxy.as_deref(), false);
    let rc = client.request_client(&req.url).await;
    let ua = client.user_agent.read().await.clone();

    // Follow redirects manually so every hop passes the SSRF guard, and learn
    // the final response headers (including Content-Disposition) before
    // touching disk. The first body is a plain un-ranged GET.
    let mut current = start_url.clone();
    let mut resp = None;
    for _ in 0..=MAX_REDIRECTS {
        let mut builder = rc.get(current.as_str());
        builder = builder.header(USER_AGENT, &ua).header(ACCEPT, "*/*");
        let cookie_header = jar.get_cookie_header(&current);
        if !cookie_header.is_empty() {
            builder = builder.header(COOKIE, cookie_header);
        }
        let r = builder.send().await?;
        let status = r.status();
        for sc in r.headers().get_all("set-cookie") {
            if let Ok(s) = sc.to_str() {
                jar.set_cookie(s, &current);
            }
        }
        if status.is_redirection() {
            if let Some(loc) = r.headers().get(LOCATION).and_then(|v| v.to_str().ok()) {
                let next = current.join(loc)?;
                ensure_http_scheme(&next)?;
                validate_url(&next, false)?;
                current = next;
                continue;
            }
            return Err(anyhow!("redirect {} without Location header", status.as_u16()));
        }
        resp = Some(r);
        break;
    }
    let resp = resp.ok_or_else(|| anyhow!("too many redirects (> {})", MAX_REDIRECTS))?;

    let status = resp.status();
    if !(status == reqwest::StatusCode::OK || status == reqwest::StatusCode::PARTIAL_CONTENT) {
        return Err(anyhow!("http {}: download failed", status.as_u16()));
    }

    let headers = resp.headers().clone();
    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(';').next().unwrap_or(s).trim().to_string());

    // Filename resolution: explicit param → Content-Disposition → URL tail →
    // default. The .part suffix marks in-flight data; renamed on success.
    let filename = match req
        .filename
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(explicit) => sanitize_filename(explicit),
        None => headers
            .get(CONTENT_DISPOSITION)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_content_disposition)
            .map(|s| sanitize_filename(&s))
            .unwrap_or_else(|| {
                filename_from_url(&current)
                    .map(|s| sanitize_filename(&s))
                    .unwrap_or_else(|| DEFAULT_FILENAME.to_string())
            }),
    };
    let dir = download_dir()?;
    let final_path = dir.join(&filename);
    let part_path = dir.join(format!("{}.part", filename));

    // Resume: if a partial file exists under the resolved name, re-issue a
    // ranged GET and continue from its length. 206 → append; 416 → rejected;
    // 200 → server ignored Range, restart from zero below.
    let existing_len = std::fs::metadata(&final_path)
        .or_else(|_| std::fs::metadata(&part_path))
        .ok()
        .filter(|m| m.len() > 0)
        .map(|m| m.len());
    if req.resume {
        if let Some(len) = existing_len {
            let mut builder = rc.get(current.as_str());
            builder = builder
                .header(USER_AGENT, &ua)
                .header(ACCEPT, "*/*")
                .header(RANGE, format!("bytes={}-", len));
            let rr = builder.send().await?;
            let rs = rr.status();
            if rs == reqwest::StatusCode::PARTIAL_CONTENT {
                return stream_to_file(
                    rr,
                    &part_path,
                    len,
                    content_type,
                    current.to_string(),
                    &final_path,
                    &filename,
                    true,
                )
                .await;
            }
            if rs == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
                return Err(anyhow!(
                    "resume rejected: http 416 (partial file of {}B unusable)",
                    len
                ));
            }
            // 200 or any error status falls through to the plain body in `resp`.
        }
    }

    stream_to_file(
        resp,
        &part_path,
        0,
        content_type,
        current.to_string(),
        &final_path,
        &filename,
        false,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn stream_to_file(
    resp: reqwest::Response,
    part_path: &Path,
    start_offset: u64,
    content_type: Option<String>,
    final_url: String,
    final_path: &Path,
    filename: &str,
    resumed_flag: bool,
) -> Result<DownloadResponse> {
    // Append mode only when continuing a ranged transfer; a plain 200 always
    // rewrites from zero (server ignored our Range).
    let appending = start_offset > 0;
    let mut file = if appending {
        tokio::fs::OpenOptions::new()
            .append(true)
            .open(part_path)
            .await
            .with_context(|| format!("open partial {}", part_path.display()))?
    } else {
        tokio::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(part_path)
            .await
            .with_context(|| format!("create {}", part_path.display()))?
    };

    let hasher = Arc::new(Mutex::new(Sha256::new()));
    if appending {
        // Hash the already-on-disk prefix once so the digest covers the whole
        // finished file, not just this session's appended bytes.
        let prefix = tokio::fs::read(part_path)
            .await
            .with_context(|| format!("hashing existing partial {}", part_path.display()))?;
        hasher.lock().unwrap().update(&prefix);
    }

    let writer_task: JoinHandle<Result<u64>> = {
        let hasher = hasher.clone();
        tokio::spawn(async move {
            let mut total: u64 = 0;
            let mut stream = resp.bytes_stream();
            loop {
                let next = match timeout(IDLE_TIMEOUT, stream.next()).await {
                    Ok(item) => item,
                    Err(_) => {
                        return Err(anyhow!(
                            "download stalled: no data for {}s",
                            IDLE_TIMEOUT.as_secs()
                        ));
                    }
                };
                let Some(chunk) = next else { break };
                let chunk = chunk.map_err(|e| anyhow!("body read failed: {e}"))?;
                file.write_all(&chunk)
                    .await
                    .map_err(|e| anyhow!("disk write failed: {e}"))?;
                hasher.lock().unwrap().update(&chunk);
                total += chunk.len() as u64;
                if start_offset + total > MAX_FILE_BYTES {
                    return Err(anyhow!(
                        "exceeds {}B hard limit",
                        MAX_FILE_BYTES
                    ));
                }
            }
            file.flush().await.map_err(|e| anyhow!("flush failed: {e}"))?;
            Ok(total)
        })
    };

    let written = writer_task.await.map_err(|e| anyhow!("writer crashed: {e}"))??;

    tokio::fs::rename(part_path, final_path)
        .await
        .with_context(|| format!("finalize {}", final_path.display()))?;

    let digest = hasher.lock().unwrap().clone().finalize();
    let sha256 = digest.iter().map(|b| format!("{:02x}", b)).collect::<String>();

    Ok(DownloadResponse {
        url: final_url,
        path: final_path.display().to_string(),
        filename: filename.to_string(),
        size_bytes: written,
        content_type,
        sha256,
        resumed: resumed_flag,
    })
}

fn ensure_http_scheme(url: &Url) -> Result<()> {
    match url.scheme() {
        "http" | "https" => Ok(()),
        other => Err(anyhow!("unsupported scheme '{}': only http/https", other)),
    }
}

fn download_dir() -> Result<PathBuf> {
    let base = std::env::var("AGINXBROWSER_DOWNLOAD_DIR").unwrap_or_else(|_| ".".to_string());
    let dir = PathBuf::from(base);
    if !dir.exists() {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("create download dir {}", dir.display()))?;
    }
    Ok(dir)
}

/// Final path component of the URL (query/fragment stripped by Url), decoded.
fn filename_from_url(url: &Url) -> Option<String> {
    let last = url.path().rsplit('/').next()?;
    if last.is_empty() {
        return None;
    }
    let trimmed = percent_decode(last).trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Strip directories, traversal, control characters; cap length keeping the
/// extension. Always yields a usable single-component name.
fn sanitize_filename(name: &str) -> String {
    let component: String = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .chars()
        .map(|c| if c.is_control() { '_' } else { c })
        .collect::<String>()
        .trim()
        .trim_matches('.')
        .to_string();
    let component = if component.is_empty() {
        DEFAULT_FILENAME.to_string()
    } else {
        component
    };
    if component.chars().count() > MAX_FILENAME_LEN {
        let ext: String = Path::new(&component)
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default();
        let stem_len = MAX_FILENAME_LEN.saturating_sub(ext.chars().count());
        component.chars().take(stem_len).collect::<String>() + &ext
    } else {
        component
    }
}

/// Extract filename from Content-Disposition, preferring RFC 5987
/// `filename*=UTF-8''...` over plain `filename="..."`.
fn parse_content_disposition(header: &str) -> Option<String> {
    let lower = header.to_ascii_lowercase();
    if let Some(pos) = lower.find("filename*=") {
        let rest = header[pos + "filename*=".len()..].trim();
        // charset'lang'percent-encoded-value
        if let Some((_charset, remainder)) = rest.split_once('\'') {
            if let Some((_lang, encoded)) = remainder.split_once('\'') {
                let cleaned = percent_decode(encoded).trim().to_string();
                if !cleaned.is_empty() {
                    return Some(cleaned);
                }
            }
        }
    }
    if let Some(pos) = lower.find("filename=") {
        let rest = header[pos + "filename=".len()..].trim();
        let value = rest
            .strip_prefix('"')
            .and_then(|v| v.split_once('"').map(|(inner, _)| inner))
            .unwrap_or_else(|| rest.split(';').next().unwrap_or(rest).trim());
        let cleaned = percent_decode(value).trim().to_string();
        if !cleaned.is_empty() {
            return Some(cleaned);
        }
    }
    None
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Some(b) = decode_hex_pair(bytes[i + 1], bytes[i + 2]) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn decode_hex_pair(hi: u8, lo: u8) -> Option<u8> {
    let hi = (hi as char).to_digit(16)?;
    let lo = (lo as char).to_digit(16)?;
    Some((hi * 16 + lo) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_rejects_traversal_and_separators() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("..\\..\\win\\system32"), "system32");
        assert_eq!(sanitize_filename(".."), "download");
        assert_eq!(sanitize_filename("."), "download");
        assert_eq!(sanitize_filename(""), "download");
        assert_eq!(sanitize_filename("  "), "download");
        assert_eq!(sanitize_filename("a\x00b\x1f.txt"), "a_b_.txt");
    }

    #[test]
    fn sanitize_caps_length_keeping_extension() {
        let long = format!("{}.tar.gz", "x".repeat(300));
        let out = sanitize_filename(&long);
        assert!(out.chars().count() <= MAX_FILENAME_LEN);
        assert!(out.ends_with(".gz"));
    }

    #[test]
    fn filename_from_url_takes_last_segment_decoded() {
        let u = Url::parse("https://x.test/a/b/%E7%9B%AE%E5%BD%95.zip?q=1").unwrap();
        assert_eq!(filename_from_url(&u).unwrap(), "目录.zip");
        let u = Url::parse("https://x.test/").unwrap();
        assert!(filename_from_url(&u).is_none());
    }

    #[test]
    fn content_disposition_variants_parse() {
        assert_eq!(
            parse_content_disposition("attachment; filename=\"report 2026.pdf\"").as_deref(),
            Some("report 2026.pdf")
        );
        assert_eq!(
            parse_content_disposition("attachment; filename=%E6%8A%A5%E5%91%8A.pdf").as_deref(),
            Some("报告.pdf")
        );
        // RFC 5987 wins over plain filename
        assert_eq!(
            parse_content_disposition(
                "attachment; filename=\"fallback.bin\"; filename*=UTF-8''%E7%9B%AE%E5%BD%95.zip"
            )
            .as_deref(),
            Some("目录.zip")
        );
        assert_eq!(parse_content_disposition("inline"), None);
    }

    #[test]
    fn percent_decode_plain_passthrough() {
        assert_eq!(percent_decode("abc.zip"), "abc.zip");
        assert_eq!(percent_decode("a%2zb"), "a%2zb"); // invalid hex untouched
        assert_eq!(percent_decode("a%2Fb"), "a/b"); // valid hex decoded
    }

    // ---- live streaming tests against a local fixture server ----
    // These bind 127.0.0.1, which the SSRF guard rejects unless the
    // private-network escape hatch is set. Serialize the env mutation with
    // diting_net's own lock.

    async fn spawn_fixture(body: Vec<u8>, headers: Vec<(String, String)>) -> (Url, JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { break };
                let body = body.clone();
                let headers = headers.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0u8; 8192];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let raw = String::from_utf8_lossy(&buf[..n]).to_string();
                    // reqwest normalizes request header names to lowercase.
                    let wants_range = raw.to_ascii_lowercase().contains("range:");
                    if wants_range {
                        let offset: u64 = raw
                            .to_ascii_lowercase()
                            .lines()
                            .find_map(|l| l.strip_prefix("range: bytes="))
                            .and_then(|v| v.trim_end_matches('-').trim().parse().ok())
                            .unwrap_or(0);
                        let mut head = String::from("HTTP/1.1 206 Partial Content\r\n");
                        for (k, v) in &headers {
                            head.push_str(&format!("{k}: {v}\r\n"));
                        }
                        head.push_str(&format!(
                            "Content-Range: bytes {}-{}/{}\r\n",
                            offset,
                            body.len().saturating_sub(1),
                            body.len()
                        ));
                        head.push_str(&format!(
                            "Content-Length: {}\r\n",
                            body.len() - offset as usize
                        ));
                        head.push_str("\r\n");
                        sock.write_all(head.as_bytes()).await.ok();
                        sock.write_all(&body[offset as usize..]).await.ok();
                    } else {
                        let mut head = String::from("HTTP/1.1 200 OK\r\n");
                        for (k, v) in &headers {
                            head.push_str(&format!("{k}: {v}\r\n"));
                        }
                        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
                        head.push_str("\r\n");
                        sock.write_all(head.as_bytes()).await.ok();
                        sock.write_all(&body).await.ok();
                    }
                });
            }
        });
        (
            Url::parse(&format!("http://{addr}/fixture.bin")).unwrap(),
            handle,
        )
    }

    async fn with_private_net<F, T>(f: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        let _g = crate::diting_net::PRIVATE_NET_ENV_LOCK.lock().unwrap();
        std::env::set_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK", "1");
        let out = f.await;
        std::env::remove_var("AGINXBROWSER_ALLOW_PRIVATE_NETWORK");
        out
    }

    fn tempfile_dir(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!("aginx-dl-test-{}-{}", name, std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        dir.to_string_lossy().into_owned()
    }

    #[tokio::test]
    async fn streams_body_with_correct_sha256() {
        with_private_net(async {
            let payload: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
            let expected = {
                let mut h = Sha256::new();
                h.update(&payload);
                h.finalize()
                    .iter()
                    .map(|b| format!("{:02x}", b))
                    .collect::<String>()
            };
            let (url, _srv) = spawn_fixture(payload, vec![]).await;
            let tmp = tempfile_dir("sha");
            std::env::set_var("AGINXBROWSER_DOWNLOAD_DIR", &tmp);

            let resp = do_download(DownloadRequest {
                url: url.to_string(),
                filename: None,
                resume: false,
                use_proxy: false,
                cookies: vec![],
            })
            .await
            .unwrap();

            assert_eq!(resp.sha256, expected);
            assert_eq!(resp.size_bytes, 100_000);
            assert_eq!(resp.filename, "fixture.bin");
            assert!(Path::new(&resp.path).exists());
            assert!(!resp.path.ends_with(".part"));
        })
        .await;
    }

    #[tokio::test]
    async fn resumes_partial_via_range() {
        with_private_net(async {
            let payload: Vec<u8> = (0..50_000u32).map(|i| (i % 249) as u8).collect();
            let (url, _srv) = spawn_fixture(payload.clone(), vec![]).await;
            let tmp = tempfile_dir("resume");
            std::env::set_var("AGINXBROWSER_DOWNLOAD_DIR", &tmp);
            let part = PathBuf::from(&tmp).join("fixture.bin.part");
            std::fs::write(&part, &payload[..10_000]).unwrap();

            let resp = do_download(DownloadRequest {
                url: url.to_string(),
                filename: None,
                resume: true,
                use_proxy: false,
                cookies: vec![],
            })
            .await
            .unwrap();

            assert!(resp.resumed);
            assert_eq!(resp.size_bytes, 40_000); // only the appended portion
            let on_disk = std::fs::read(PathBuf::from(&tmp).join("fixture.bin")).unwrap();
            assert_eq!(on_disk, payload);
        })
        .await;
    }

    #[tokio::test]
    async fn honors_content_disposition_filename() {
        with_private_net(async {
            let payload: Vec<u8> = b"hello world".to_vec();
            let (url, _srv) = spawn_fixture(
                payload,
                vec![(
                    "Content-Disposition".into(),
                    "attachment; filename=\"report-2026.pdf\"".into(),
                )],
            )
            .await;
            let tmp = tempfile_dir("cd");
            std::env::set_var("AGINXBROWSER_DOWNLOAD_DIR", &tmp);

            let resp = do_download(DownloadRequest {
                url: url.to_string(),
                filename: None,
                resume: false,
                use_proxy: false,
                cookies: vec![],
            })
            .await
            .unwrap();

            assert_eq!(resp.filename, "report-2026.pdf");
            assert!(Path::new(&resp.path).ends_with("report-2026.pdf"));
        })
        .await;
    }

    #[tokio::test]
    async fn rejects_non_http_scheme() {
        let err = do_download(DownloadRequest {
            url: "file:///etc/passwd".into(),
            filename: None,
            resume: false,
            use_proxy: false,
            cookies: vec![],
        })
        .await
        .unwrap_err();
        assert!(err.to_string().contains("unsupported scheme"));
    }
}
