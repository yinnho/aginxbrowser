//! CAPTCHA detection, reporting, and optional auto-solve.
//!
//! Three-layer approach (inspired by BrowserAct):
//! 1. Environment layer — TLS fingerprint + proxy (handled by wreq/stealth)
//! 2. Execution layer — auto-solve via external service (2captcha etc.)
//! 3. Human layer — report CAPTCHA events in API response so caller can escalate

use serde::Serialize;

/// Type of CAPTCHA encountered.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptchaType {
    CloudflareTurnstile,
    RecaptchaV2,
    RecaptchaV3,
    Hcaptcha,
    SliderCaptcha,
    Unknown,
}

/// A CAPTCHA event reported in an API response.
#[derive(Debug, Clone, Serialize)]
pub struct CaptchaEvent {
    /// Engine name ("baidu", "google", etc.). Empty for /fetch.
    pub engine: String,
    /// What kind of CAPTCHA was detected.
    pub captcha_type: CaptchaType,
    /// The URL that triggered the CAPTCHA.
    pub url: String,
    /// Whether auto-solve was attempted.
    pub auto_solve_attempted: bool,
    /// Whether auto-solve succeeded.
    pub auto_solve_succeeded: bool,
}

/// External CAPTCHA solving service.
#[derive(Debug, Clone)]
pub enum CaptchaService {
    TwoCaptcha,
    AntiCaptcha,
}

/// Configuration for an external CAPTCHA solving service.
#[derive(Debug, Clone)]
pub struct CaptchaSolverConfig {
    pub api_key: String,
    pub service: CaptchaService,
    pub default_timeout_secs: u64,
}

/// Result from attempting to auto-solve a CAPTCHA. The token / reason
/// payloads are consumed by the injection step that submits the solution —
/// wired when a solver is configured (hosted runs keep
/// CAPTCHA_SOLVER_API_KEY unset and always get NotAttempted).
#[allow(dead_code)]
pub enum CaptchaSolveResult {
    Solved { token: String },
    Failed { reason: String },
    NotAttempted,
}

/// Detect CAPTCHA type from a URL and optional HTML body.
pub fn detect_captcha_type(url: &str, html: Option<&str>) -> Option<CaptchaType> {
    // URL-based detection
    if url.contains("sorry.google.com") || url.contains("/sorry/") {
        return Some(CaptchaType::RecaptchaV2);
    }
    if url.contains("/antispider") || url.contains("wappass.baidu.com") {
        return Some(CaptchaType::SliderCaptcha);
    }
    if url.contains("challenge-platform") || url.contains("challenges.cloudflare.com") {
        return Some(CaptchaType::CloudflareTurnstile);
    }

    // HTML-based detection
    if let Some(html) = html {
        if html.contains("g-recaptcha") || html.contains("data-sitekey") {
            return Some(CaptchaType::RecaptchaV2);
        }
        if html.contains("h-captcha") || html.contains("hcaptcha.com") {
            return Some(CaptchaType::Hcaptcha);
        }
        if html.contains("challenges.cloudflare.com") || html.contains("cf-turnstile") {
            return Some(CaptchaType::CloudflareTurnstile);
        }
        // Google "unusual traffic" page
        if html.contains("unusual traffic") || html.contains("/sorry/") {
            return Some(CaptchaType::RecaptchaV2);
        }
        // Baidu/Sogou antispider
        if html.contains("wappass.baidu.com") || html.contains("/antispider") {
            return Some(CaptchaType::SliderCaptcha);
        }
        if html.contains("\u{7528}\u{6237}\u{9891}\u{7387}\u{9650}\u{5236}") {
            // 用户频率限制
            return Some(CaptchaType::SliderCaptcha);
        }
    }

    None
}

/// Load CAPTCHA solver config from environment variables.
///
/// Reads:
/// - `CAPTCHA_SOLVER_API_KEY` (required)
/// - `CAPTCHA_SOLVER_SERVICE` (default: "2captcha")
/// - `CAPTCHA_SOLVER_TIMEOUT_SECS` (default: 120)
pub fn load_solver_config_from_env() -> Option<CaptchaSolverConfig> {
    let api_key = std::env::var("CAPTCHA_SOLVER_API_KEY").ok()?;
    if api_key.is_empty() {
        return None;
    }
    let service = match std::env::var("CAPTCHA_SOLVER_SERVICE")
        .unwrap_or_default()
        .to_lowercase()
        .as_str()
    {
        "anticaptcha" => CaptchaService::AntiCaptcha,
        _ => CaptchaService::TwoCaptcha,
    };
    let timeout = std::env::var("CAPTCHA_SOLVER_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(120);
    Some(CaptchaSolverConfig {
        api_key,
        service,
        default_timeout_secs: timeout,
    })
}

/// Attempt to auto-solve a CAPTCHA using an external service.
///
/// For reCAPTCHA/hCaptcha: extracts the sitekey from HTML, submits to the
/// solver API, polls for result, returns the token.
/// For other types: returns NotAttempted (unsupported for auto-solve).
pub async fn auto_solve_captcha(
    url: &str,
    html: &str,
    captcha_type: &CaptchaType,
    config: &CaptchaSolverConfig,
) -> CaptchaSolveResult {
    let sitekey = match captcha_type {
        CaptchaType::RecaptchaV2 | CaptchaType::RecaptchaV3 => {
            extract_sitekey(html, "g-recaptcha", "data-sitekey")
        }
        CaptchaType::Hcaptcha => extract_sitekey(html, "h-captcha", "data-sitekey"),
        CaptchaType::CloudflareTurnstile => extract_sitekey(html, "cf-turnstile", "data-sitekey"),
        CaptchaType::SliderCaptcha | CaptchaType::Unknown => {
            return CaptchaSolveResult::NotAttempted;
        }
    };

    let sitekey = match sitekey {
        Some(k) => k,
        None => {
            return CaptchaSolveResult::Failed {
                reason: "could not extract sitekey from HTML".into(),
            }
        }
    };

    match config.service {
        CaptchaService::TwoCaptcha => solve_with_2captcha(url, &sitekey, captcha_type, config).await,
        CaptchaService::AntiCaptcha => {
            // Placeholder — same API shape, different endpoints.
            CaptchaSolveResult::NotAttempted
        }
    }
}

/// Extract a sitekey from HTML by finding the container element and reading an attribute.
fn extract_sitekey(html: &str, container_class: &str, attr: &str) -> Option<String> {
    let pattern = format!("{}=\"", attr);
    // Find the container class first, then look for the sitekey attribute nearby.
    let class_pos = html.find(container_class)?;
    let search_area = &html[class_pos..];
    let attr_pos = search_area.find(&pattern)?;
    let rest = &search_area[attr_pos + pattern.len()..];
    let end = rest.find('"').unwrap_or(rest.len());
    let key = &rest[..end];
    if key.is_empty() {
        None
    } else {
        Some(key.to_string())
    }
}

/// Solve a CAPTCHA via the 2captcha API.
async fn solve_with_2captcha(
    url: &str,
    sitekey: &str,
    captcha_type: &CaptchaType,
    config: &CaptchaSolverConfig,
) -> CaptchaSolveResult {
    let method = match captcha_type {
        CaptchaType::RecaptchaV2 => "userrecaptcha",
        CaptchaType::RecaptchaV3 => "userrecaptcha",
        CaptchaType::Hcaptcha => "hcaptcha",
        CaptchaType::CloudflareTurnstile => "turnstile",
        _ => return CaptchaSolveResult::NotAttempted,
    };

    let client = reqwest::Client::new();

    // Submit task
    let res = client
        .post("https://2captcha.com/in.php")
        .form(&[
            ("key", config.api_key.as_str()),
            ("method", method),
            ("sitekey", sitekey),
            ("pageurl", url),
            ("json", "1"),
        ])
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await;

    let resp = match res {
        Ok(r) => r,
        Err(e) => {
            return CaptchaSolveResult::Failed {
                reason: format!("submit failed: {e}"),
            }
        }
    };

    #[derive(serde::Deserialize)]
    struct SubmitResponse {
        status: u8,
        request: String,
    }

    let submit = match resp.json::<SubmitResponse>().await {
        Ok(s) => s,
        Err(e) => {
            return CaptchaSolveResult::Failed {
                reason: format!("submit parse error: {e}"),
            }
        }
    };

    if submit.status != 1 {
        return CaptchaSolveResult::Failed {
            reason: format!("submit rejected: {}", submit.request),
        };
    }

    let task_id = submit.request;

    // Poll for result (first poll after 5s, then every 2s)
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(config.default_timeout_secs);

    while std::time::Instant::now() < deadline {
        let res = client
            .get("https://2captcha.com/res.php")
            .query(&[
                ("key", config.api_key.as_str()),
                ("action", "get"),
                ("id", &task_id),
                ("json", "1"),
            ])
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await;

        match res {
            Ok(r) => {
                #[derive(serde::Deserialize)]
                struct PollResponse {
                    status: u8,
                    request: String,
                }
                match r.json::<PollResponse>().await {
                    Ok(poll) => {
                        if poll.status == 1 {
                            return CaptchaSolveResult::Solved {
                                token: poll.request,
                            };
                        }
                        if poll.request == "CAPCHA_NOT_READY" {
                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                            continue;
                        }
                        return CaptchaSolveResult::Failed {
                            reason: format!("poll error: {}", poll.request),
                        };
                    }
                    Err(e) => {
                        return CaptchaSolveResult::Failed {
                            reason: format!("poll parse error: {e}"),
                        }
                    }
                }
            }
            Err(e) => {
                return CaptchaSolveResult::Failed {
                    reason: format!("poll request failed: {e}"),
                }
            }
        }
    }

    CaptchaSolveResult::Failed {
        reason: "timeout waiting for solution".into(),
    }
}
