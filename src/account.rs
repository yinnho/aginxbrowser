//! Named login identities — the multi-account layer.
//!
//! The process-global [`crate::server::SHARED_COOKIE_JAR`] is deliberate for
//! anonymous stateless traffic (repeat-visitor cookies cut CAPTCHA rates) but
//! it makes multi-account work impossible: two Taobao logins share one jar,
//! and their `cookie2`/`_tb_token_` cookies clobber each other, last writer
//! wins. An *account* is the fix: a named identity (Chrome-profile
//! semantics — one jar per identity, not a site×account matrix) whose cookies
//! and storage live in a private jar, written back to the store under the
//! account's name. Concurrent sessions on one account share the live jar
//! (two tabs, one profile); different accounts never touch each other, and
//! none of them touch the anonymous shared jar — isolation is itself
//! anti-correlation, since shared cookie history is a risk-control linkage
//! signal.
//!
//! Accounts are keyed `(owner, name)` from day one: local single-user runs
//! collapse to one owner, hosted multi-caller deployments keep callers
//! separate (same scoping the cache store uses).

use diting::diting_net::cookies::CookieJar;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Accounts are addressed by bare name; keep the charset tight so names stay
/// single-path-segment, filename-safe and unambiguous in logs. 1..64 of
/// [a-zA-Z0-9_-].
pub fn validate_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "invalid account name {:?}: 1-64 chars of [a-zA-Z0-9_-]",
            &name[..name.len().min(32)]
        ))
    }
}

/// Live jars for accounts that currently have (or recently had) a session.
/// Concurrent same-account sessions share one jar — the "two tabs, one
/// profile" semantics — while different accounts get different jars. The
/// registry outlives idle session eviction: a session that idles out and a
/// later `session_create {account}` pick up the same warm jar, no re-seed
/// from disk needed.
type JarMap = Mutex<HashMap<(String, String), Arc<CookieJar>>>;
static LIVE_JARS: std::sync::LazyLock<JarMap> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// Get-or-create the live jar for `(owner, name)`. Keys use the canonical
/// owner ([`crate::store::norm_owner`]) so every face lands on one jar per
/// identity — every face lands on one jar per identity
/// ("rest" under the default global scope). A fresh jar is seeded
/// from the stored account record (if any) so a post-restart
/// `session_create {account}` starts where the last session left off.
pub fn jar_for(owner: &str, name: &str) -> Arc<CookieJar> {
    validate_name(name).expect("account name validated at the API boundary");
    let key = (crate::store::norm_owner(owner), name.to_string());
    if let Some(jar) = LIVE_JARS.lock().expect("live jar map poisoned").get(&key) {
        return jar.clone();
    }
    let jar = Arc::new(CookieJar::new());
    if let Some((text, _)) = crate::store::load_account(owner, name) {
        if let Ok(record) = serde_json::from_str::<serde_json::Value>(&text) {
            let cookies: Vec<String> = record["cookies"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            seed_jar(&jar, &cookies);
        }
    }
    LIVE_JARS
        .lock()
        .expect("live jar map poisoned")
        .insert(key, jar.clone());
    jar
}

/// Seed a jar from full Set-Cookie strings (the `Cookies` command's export
/// form). `CookieJar::set_cookie` applies RFC 6265 domain acceptance against
/// the request URL, so each entry is anchored at its own `Domain=` attribute
/// — that's what keeps `.taobao.com`-style sibling-domain logins alive.
/// Entries without a `Domain=` can't be re-anchored and are skipped (the
/// export form always carries it).
fn seed_jar(jar: &CookieJar, cookies: &[String]) {
    for c in cookies {
        let domain = c
            .split(';')
            .skip(1)
            .filter_map(|attr| attr.trim().split_once('='))
            .find(|(k, _)| k.trim().eq_ignore_ascii_case("domain"))
            .map(|(_, v)| v.trim().trim_start_matches('.').to_string());
        let Some(domain) = domain.filter(|d| !d.is_empty()) else {
            continue;
        };
        let Ok(url) = url::Url::parse(&format!("https://{domain}/")) else {
            continue;
        };
        jar.set_cookie_seeded(c, &url);
    }
}

/// The stored record, if any: `(record_json, updated_at_unix_secs)`. Writers
/// go through [`crate::store::mutate_account`] (field-scoped, atomic) — a
/// bare save helper here would reintroduce the lost-update race this layer
/// just closed (verify stomping cookie write-backs).
pub fn load(owner: &str, name: &str) -> Option<(String, i64)> {
    crate::store::load_account(owner, name)
}

/// `https://host[:port]` for an http(s) URL — `location.origin` semantics
/// (default ports fold away), `None` for anything else (about:blank,
/// unparsable strings). The storage-replay gate in session_thread compares
/// the live page's origin against the blob's captured origin through this.
pub(crate) fn url_origin(url: &str) -> Option<String> {
    let u = url::Url::parse(url).ok()?;
    if u.scheme() != "http" && u.scheme() != "https" {
        return None;
    }
    let host = u.host_str()?;
    Some(match u.port() {
        Some(p) => format!("{}://{host}:{p}", u.scheme()),
        None => format!("{}://{host}", u.scheme()),
    })
}

/// The record's captured web storage as a session-create seed blob:
/// `{"local_storage": …, "session_storage": …, "origin": …}` (issue #141).
/// Cookies alone can't carry a localStorage-token login — the jar replay
/// leaves those sessions logged out after a restart. The origin rides the
/// blob because storage only belongs to the site it was captured from: the
/// session's pending-injection gate holds the blob until a navigation
/// actually lands there (start_url-less sessions sit on about:blank first).
/// `None` when there is nothing worth replaying: no record, both maps
/// empty, or a capture taken off a real origin — never seed a blank page's
/// keys onto a real site.
pub(crate) fn record_storage(owner: &str, name: &str) -> Option<serde_json::Value> {
    let (text, _) = load(owner, name)?;
    let rec: serde_json::Value = serde_json::from_str(&text).ok()?;
    storage_seed_from_record(&rec)
}

/// Pure half of [`record_storage`]: the seed blob from a record value, so
/// the gating rules are testable without the store.
fn storage_seed_from_record(rec: &serde_json::Value) -> Option<serde_json::Value> {
    let ls = rec.get("local_storage")?;
    let ss = rec.get("session_storage")?;
    if ls.as_object().is_none_or(|m| m.is_empty())
        && ss.as_object().is_none_or(|m| m.is_empty())
    {
        return None;
    }
    let origin = url_origin(rec.get("url").and_then(serde_json::Value::as_str)?)?;
    Some(serde_json::json!({
        "local_storage": ls,
        "session_storage": ss,
        "origin": origin,
    }))
}

/// Delete an account: stored record AND live jar. Cookie values are
/// credentials — delete means gone.
pub fn delete(owner: &str, name: &str) -> bool {
    let key = (crate::store::norm_owner(owner), name.to_string());
    let mut live = LIVE_JARS.lock().expect("live jar map poisoned");
    live.remove(&key);
    crate::store::delete_account(owner, name)
}

/// A named account's device identity: the User-Agent every request carries,
/// and the hardware seed the JS persona (screen/dpr/GPU/canvas) draws from.
/// One stable device per identity — jar isolation alone doesn't stop
/// risk-control linkage: two logins sharing one fingerprint read as "one
/// device with two accounts", which is exactly the correlation the layer
/// exists to prevent.
#[derive(Clone)]
pub struct Persona {
    pub fp_seed: u64,
    pub user_agent: String,
}

/// UA pool for a fresh persona: Windows and macOS Chrome 145 only. Both
/// entries match the fleet-claimed chrome155 major on family and version
/// (zero `warn_on_ua_tls_mismatch` tells), and the TLS emulation OS follows
/// the UA, so Win↔Mac is the free axis — hardware differentiation rides the
/// fp_seed. Linux is excluded: the TLS/OS contract and the product default
/// are Win↔Mac; X11 jars on a Mac host are out of family. Drawn from the
/// profiles pool (not hardcoded) so a pool refresh follows along.
fn persona_ua_pool() -> Vec<&'static str> {
    diting::diting_browser::profiles::PROFILES
        .iter()
        .filter(|p| {
            p.user_agent.contains("Chrome/155.0.0.0")
                && (p.user_agent.contains("Windows NT") || p.user_agent.contains("Macintosh"))
        })
        .map(|p| p.user_agent)
        .collect()
}

/// Product-default Mac Chrome/155 UA (profiles `DEFAULT_PROFILE_INDEX`).
fn default_mac_chrome155_ua() -> &'static str {
    diting::diting_browser::profiles::PROFILES
        .iter()
        .find(|p| {
            p.user_agent.contains("Macintosh") && p.user_agent.contains("Chrome/155.0.0.0")
        })
        .map(|p| p.user_agent)
        .unwrap_or(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/155.0.0.0 Safari/537.36",
        )
}

/// Prefer a Mac-only draw when the host is Darwin, or when the operator pins
/// `AGINXBROWSER_PERSONA_OS=mac`. `=win` / `=any` opts out of the Darwin default.
fn prefer_mac_persona() -> bool {
    if let Ok(v) = std::env::var("AGINXBROWSER_PERSONA_OS") {
        let v = v.trim().to_ascii_lowercase();
        if matches!(v.as_str(), "mac" | "macos" | "macintosh" | "darwin") {
            return true;
        }
        if matches!(v.as_str(), "win" | "windows" | "any" | "all" | "off" | "0") {
            return false;
        }
    }
    cfg!(target_os = "macos")
}

/// Chrome major from a UA string (`Chrome/154` → 154). None when absent.
fn chrome_major_from_ua(ua: &str) -> Option<u32> {
    let rest = ua.split("Chrome/").nth(1)?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// The Chrome major the whole fleet claims: persona UA, GL tables, uad
/// brands, sec-ch-ua all say 155. Kept local so account code does not depend
/// on the stealth feature gate. The TLS handshake itself stays on the
/// chrome145 *shape* — wreq-util rc.31's ceiling (rc.31's BoringSSL name
/// table has no mldsa sigalgs, so the 150+ sigalgs delta is inexpressible);
/// that residual is an accepted A-155 trade, revisit on a wreq 0.16 migration.
fn default_tls_chrome_major() -> u32 {
    155
}

/// Rewrite a UA whose Chrome major disagrees with the fleet-claimed major
/// onto a same-OS Chrome/155 from the persona pool (Mac fallback). Returns
/// the original string when already aligned.
fn coerce_ua_to_tls(ua: &str) -> String {
    if chrome_major_from_ua(ua) == Some(default_tls_chrome_major()) {
        return ua.to_string();
    }
    let pool = persona_ua_pool();
    let prefer_mac = ua.contains("Macintosh") || ua.contains("Mac OS X");
    let prefer_win = ua.contains("Windows NT");
    let pick = pool
        .iter()
        .copied()
        .find(|p| prefer_mac && p.contains("Macintosh"))
        .or_else(|| pool.iter().copied().find(|p| prefer_win && p.contains("Windows NT")))
        .or_else(|| pool.first().copied())
        .unwrap_or_else(default_mac_chrome155_ua);
    tracing::warn!(
        "persona UA Chrome major mismatches TLS chrome{}; rewriting to coherent pool UA",
        default_tls_chrome_major()
    );
    pick.to_string()
}

/// Resolve the UA hint for a fresh account persona teach:
/// 1. explicit `persona_ua` / import UA when non-empty (TLS-coerced)
/// 2. else `AGINXBROWSER_UA` when set (TLS-coerced)
/// 3. else Mac Chrome/155 when [`prefer_mac_persona`]
/// 4. else `None` → Win/Mac pool lottery in [`draw_persona`]
pub fn resolve_ua_hint(explicit: Option<&str>) -> Option<String> {
    if let Some(u) = explicit.map(str::trim).filter(|u| !u.is_empty()) {
        return Some(coerce_ua_to_tls(u));
    }
    if let Ok(u) = std::env::var("AGINXBROWSER_UA") {
        let u = u.trim().to_string();
        if !u.is_empty() {
            return Some(coerce_ua_to_tls(&u));
        }
    }
    if prefer_mac_persona() {
        return Some(default_mac_chrome155_ua().to_string());
    }
    None
}

/// Draw a fresh persona: random fp_seed (same uuid cut `Page::new` uses) and
/// a UA — `ua_hint` after TLS coerce when given (an imported cURL's User-Agent
/// is the human's device, but Chrome major must still match the fleet chrome155
/// handshake), else a pick from the Chrome-145 pool keyed off the seed. On
/// Darwin (or `AGINXBROWSER_PERSONA_OS=mac`) the pool draw is Mac-only.
fn draw_persona(ua_hint: Option<&str>) -> Persona {
    let fp_seed = u64::from_be_bytes(uuid::Uuid::new_v4().into_bytes()[..8].try_into().unwrap());
    let pool = persona_ua_pool();
    let user_agent = match ua_hint.map(str::trim).filter(|u| !u.is_empty()) {
        Some(u) => coerce_ua_to_tls(u),
        None => {
            let draw_pool: Vec<&'static str> = if prefer_mac_persona() {
                pool.iter()
                    .copied()
                    .filter(|u| u.contains("Macintosh"))
                    .collect()
            } else {
                pool
            };
            let draw_pool = if draw_pool.is_empty() {
                persona_ua_pool()
            } else {
                draw_pool
            };
            draw_pool[(fp_seed as usize) % draw_pool.len()].to_string()
        }
    };
    Persona {
        fp_seed,
        user_agent,
    }
}

/// Read the persona off a stored record. Partial or empty entries read as
/// "no persona yet" so the next session draws a complete one.
fn read_persona(record: &serde_json::Value) -> Option<Persona> {
    let fp_seed = record["persona"]["fp_seed"].as_u64()?;
    let user_agent = record["persona"]["user_agent"]
        .as_str()
        .filter(|u| !u.is_empty())?;
    Some(Persona {
        fp_seed,
        user_agent: user_agent.to_string(),
    })
}

/// Decide the record's persona in place, teach-once: a stored complete
/// persona is returned as-is (coherent) or normalized onto the TLS-coherent
/// UA (a stale Chrome major); an absent/partial persona inserts `drawn`.
/// Pure so the persistence path (which cannot re-point the global store per
/// test) stays out of the decision.
fn teach_persona(record: &mut serde_json::Value, drawn: &Persona) -> Persona {
    match read_persona(record) {
        Some(p) => {
            let fixed = coerce_ua_to_tls(&p.user_agent);
            if fixed == p.user_agent {
                return p;
            }
            // Normalize stored stale-major jars onto the fleet chrome155 once.
            let normalized = Persona {
                fp_seed: p.fp_seed,
                user_agent: fixed,
            };
            record["persona"] = serde_json::json!({
                "fp_seed": normalized.fp_seed,
                "user_agent": normalized.user_agent,
            });
            normalized
        }
        None => {
            record["persona"] = serde_json::json!({
                "fp_seed": drawn.fp_seed,
                "user_agent": drawn.user_agent,
            });
            drawn.clone()
        }
    }
}

/// Get-or-draw the account's device persona, teach-once like verify: the
/// first call draws (fp_seed random, UA = `ua_hint` when the caller has the
/// human's real one — the import_curl path) and writes it into the stored
/// record; every later call returns the remembered pair, so an account is
/// one stable device across sessions and restarts. When the store is
/// disabled the draw is best-effort per call (same semantics as jar seeding).
pub fn persona_for(owner: &str, name: &str, ua_hint: Option<&str>) -> Persona {
    validate_name(name).expect("account name validated at the API boundary");
    // Fast path: a stored coherent persona needs no write at all — every
    // session create runs this, and an unconditional RMW would take the
    // store lock each time for nothing.
    if let Some((text, _)) = load(owner, name) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(p) = read_persona(&v) {
                if coerce_ua_to_tls(&p.user_agent) == p.user_agent {
                    return p;
                }
            }
        }
    }
    // Teach/normalize through the atomic field mutation: load→decide→save
    // under one store lock, so the per-command cookie write-back
    // (capture_account) can neither be rolled back by this save nor land a
    // stale record copy on top of a persona this path just drew. Deciding
    // inside the closure also makes concurrent first-draws converge on one
    // device instead of each caller keeping its own random fp_seed.
    let drawn = draw_persona(ua_hint);
    crate::store::mutate_account(owner, name, |record| teach_persona(record, &drawn))
        .unwrap_or(drawn)
}

/// One row per account for `GET /accounts`. Metadata
/// only — cookie values never appear (they're credentials; the count and
/// domains are enough to tell identities apart).
#[derive(Debug, serde::Serialize)]
pub struct AccountSummary {
    pub name: String,
    /// Registrable domains the account holds cookies for.
    pub domains: Vec<String>,
    pub cookie_count: usize,
    pub updated_at: i64,
    /// The remembered verify spec, if this account was ever verified.
    pub verify_url: Option<String>,
    pub verify_predicate: Option<String>,
    /// Last verdict, if any: `{logged_in, checked_at, url}`.
    pub verify_last: Option<serde_json::Value>,
    /// The persona's User-Agent, if this identity has drawn its device
    /// identity. The fp_seed stays server-side — callers can't do anything
    /// with it, and not echoing it keeps the summary metadata-only.
    pub persona_ua: Option<String>,
}

pub fn list(owner: &str) -> Vec<AccountSummary> {
    crate::store::list_accounts(owner)
}

/// Check whether an account is still logged in, and remember how.
///
/// The verify spec is taught once: the first call passes `url` + `predicate`
/// (a JS expression that evaluates truthy on a logged-in page, e.g.
/// `!!document.querySelector('.user-nick')`); later calls can be bare — the
/// remembered spec reruns. The check runs in a scratch session created AS the
/// account (private jar, same egress), so the dispatch write-back refreshes
/// the stored cookies along the way. Verdict — `{logged_in, url, checked_at}`
/// — is saved as `verify.last_result` and returned.
pub async fn verify(
    owner: &str,
    name: &str,
    url: Option<&str>,
    predicate: Option<&str>,
) -> Result<serde_json::Value, String> {
    validate_name(name)?;
    let record = match load(owner, name) {
        Some((text, _)) => serde_json::from_str::<serde_json::Value>(&text)
            .unwrap_or(serde_json::json!({"version": 1})),
        None => serde_json::json!({"version": 1}),
    };
    let vurl = url
        .map(str::to_string)
        .or_else(|| record["verify"]["url"].as_str().map(str::to_string))
        .or_else(|| {
            record["url"]
                .as_str()
                .filter(|u| !u.is_empty() && *u != "about:blank")
                .map(str::to_string)
        });
    let pred = predicate
        .map(str::to_string)
        .or_else(|| record["verify"]["predicate"].as_str().map(str::to_string));
    let (Some(vurl), Some(pred)) = (vurl, pred) else {
        return Err(
            "no verify spec: pass url + predicate once so it is remembered for this account \
             (predicate = a JS expression that is truthy on a logged-in page)"
                .into(),
        );
    };
    // Teach-first: a caller-supplied spec wins even when the eval below
    // fails, so a broken predicate can be corrected on the next call.
    // Field-scoped atomic write — the scratch session's own per-command
    // cookie write-backs must survive this save (they run while the probe
    // below navigates; the loaded `record` copy is already stale by then).
    let _ = crate::store::mutate_account(owner, name, |rec| {
        rec["verify"] = serde_json::json!({ "url": vurl, "predicate": pred });
    });

    let use_proxy = record["use_proxy"].as_bool().unwrap_or(false);
    let mut mgr = crate::session::SESSIONS.lock().await;
    mgr.evict_expired();
    // No start_url: the Navigate below is the initial load (awaited), so
    // the predicate never races an in-flight goto.
    let sid = mgr.create(
        None,
        use_proxy,
        vec![],
        None,
        None,
        None,
        false,
        false,
        Some((owner.to_string(), name.to_string())),
    );
    let outcome = run_verify_probe(&mut mgr, &sid, &vurl, &pred).await;
    mgr.close(&sid);

    let verdict = match outcome {
        Ok(v) => v,
        Err(e) => {
            // Keep the learned spec, drop the stale last_result — the check
            // itself failed, which is not a logout verdict. Field-scoped:
            // the probe session's cookie refresh survives this save.
            let _ = crate::store::mutate_account(owner, name, |rec| {
                rec["verify"] = serde_json::json!({ "url": vurl, "predicate": pred });
            });
            return Err(format!("verify probe failed: {e}"));
        }
    };
    crate::store::mutate_account(owner, name, |rec| {
        rec["verify"] = serde_json::json!({
            "url": vurl,
            "predicate": pred,
            "last_result": verdict,
        });
    })?;
    Ok(serde_json::json!({
        "name": name,
        "logged_in": verdict["logged_in"],
        "url": verdict["url"],
        "checked_at": verdict["checked_at"],
        "verify_url": vurl,
    }))
}

/// Navigate the scratch session and evaluate the predicate, capturing both
/// its truthiness and the page URL in one round-trip (the wrapped script
/// returns `{v, url}`).
async fn run_verify_probe(
    mgr: &mut crate::session::SessionManager,
    sid: &str,
    url: &str,
    predicate: &str,
) -> Result<serde_json::Value, String> {
    mgr.send(sid, |reply| crate::session::SessionCommand::Navigate {
        url: url.to_string(),
        reply,
    })
    .await
    .map_err(|e| e.to_string())?;
    // Wrap so a non-boolean predicate (element handle, string nick, count)
    // still round-trips as JSON, and the landed URL comes along free.
    let script = format!(
        "(function(){{ try {{ return JSON.stringify({{v: ({predicate}), url: location.href}}); }} \
         catch(e) {{ return JSON.stringify({{err: String(e)}}); }} }})()"
    );
    let v = mgr
        .send(sid, |reply| crate::session::SessionCommand::Eval {
            script: script.clone(),
            timeout_ms: Some(10_000),
            reply,
        })
        .await
        .map_err(|e| e.to_string())?;
    let text = v.as_str().ok_or("predicate returned a non-string")?;
    let parsed: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("probe result parse: {e}"))?;
    if let Some(err) = parsed["err"].as_str() {
        return Err(format!("predicate threw: {err}"));
    }
    let checked_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    Ok(serde_json::json!({
        "logged_in": json_truthy(&parsed["v"]),
        "url": parsed["url"].as_str().unwrap_or(url),
        "checked_at": checked_at,
    }))
}

/// Generic login-gate probe: one eval, selector-driven, site-agnostic —
/// counts password inputs, OTP/SMS-code inputs (name/id/placeholder/
/// autocomplete; bare "code" excluded: postal/discount/carousel noise),
/// QR surfaces (img/canvas/iframe with qr-shaped class/id/src, 二维码 alt),
/// and slider/geetest containers. Returns markers (evidence, capped) so the
/// agent can judge loose hits.
const GATE_PROBE: &str = r#"(function(){
  var markers = [];
  function mark(tag, m) {
    if (markers.length < 8) markers.push(tag + ':' + String(m).trim().slice(0, 120));
  }
  function hits(sel, tag) {
    var els = document.querySelectorAll(sel);
    for (var i = 0; i < els.length && i < 3; i++) {
      var el = els[i];
      mark(tag, (el.getAttribute('class') || '') + ' ' + (el.id || '') + ' ' +
               (el.getAttribute('src') || ''));
    }
    return els.length;
  }
  var password = document.querySelectorAll('input[type=password]').length;
  var smsRe = /(one[-_ ]?time|otp|sms|验证码)/;
  var sms = 0;
  var inputs = document.querySelectorAll('input');
  for (var i = 0; i < inputs.length && i < 200; i++) {
    var el = inputs[i];
    var hay = [el.name, el.id, el.placeholder, el.getAttribute('autocomplete')]
      .filter(function (v) { return typeof v === 'string' && v; })
      .join(' ').toLowerCase();
    if (smsRe.test(hay)) { sms++; mark('sms', hay); }
  }
  var qr = hits('img[src*="qr"],img[class*="qr"],img[id*="qr"],img[alt*="二维码"],' +
                'img[title*="二维码"],canvas[class*="qr"],canvas[id*="qr"],' +
                'iframe[src*="qr"],iframe[class*="qr"],iframe[id*="qr"],' +
                '[class*="qrcode"],[id*="qrcode"],[class*="二维码"],[id*="二维码"]', 'qr');
  var slider = hits('[class*="geetest"],[id*="geetest"],[class*="slider"],[id*="slider"],' +
                    '[class*="nc_"],[id*="nc_"],[class*="slideverify"],[id*="slideverify"],' +
                    '[class*="滑块"],[id*="滑块"]', 'slider');
  return JSON.stringify({ url: location.href, password: password, sms: sms,
                          qr: qr, slider: slider, markers: markers });
})()"#;

/// Egress rule for the wizard: a recorded account keeps its route (one
/// identity, one egress); the param only seeds a fresh or persona-only
/// record. Same stance verify takes reading `record["use_proxy"]`.
fn resolve_use_proxy(record: &serde_json::Value, param: bool) -> bool {
    record["use_proxy"].as_bool().unwrap_or(param)
}

/// Probe counts → the human/agent step labels. Advisory: loose selectors
/// (a carousel's "slider" class) cost one extra hint string; nothing gates
/// on `needs` except whether the wizard waits.
fn classify_gates(probe: &serde_json::Value) -> Vec<&'static str> {
    let mut needs = Vec::new();
    for (key, label) in [
        ("password", "password"),
        ("sms", "sms"),
        ("qr", "qr"),
        ("slider", "slider"),
    ] {
        if probe[key].as_u64().unwrap_or(0) > 0 {
            needs.push(label);
        }
    }
    needs
}

/// One call to start (or finish) a named account's login: opens the login
/// page as the account (private jar, device persona), probes which generic
/// login gates the page shows (password / sms / qr / slider), classifies
/// where it landed, and — when a `predicate` is given and no human step is
/// detected — waits once for the automatic bounce, then teaches + stamps
/// the account's verify spec. The browser stays generic: it detects and
/// describes, it never fills credentials or solves challenges.
///
/// No server-side wait loop: every session command holds the engine-wide
/// SESSIONS guard for its duration, and the human finishing a QR scan in
/// /live drives the same routes — a long wait would freeze them. When
/// gates are present the call returns immediately with the session and a
/// handoff; the human finishes there (cookies write back after every
/// action), and re-calling with the same arguments closes the loop — the
/// account's shared jar already holds the login, so the second call finds
/// no gates and the predicate matches.
pub async fn login(
    owner: &str,
    name: &str,
    url: &str,
    predicate: Option<&str>,
    use_proxy_param: bool,
    timeout_ms: u64,
) -> Result<serde_json::Value, String> {
    validate_name(name)?;
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(format!("URL must be http(s): {url}"));
    }
    let record = match load(owner, name) {
        Some((text, _)) => serde_json::from_str::<serde_json::Value>(&text)
            .unwrap_or(serde_json::json!({"version": 1})),
        None => serde_json::json!({"version": 1}),
    };
    let use_proxy = resolve_use_proxy(&record, use_proxy_param);

    enum Outcome {
        LoggedIn { success_url: String, elapsed_ms: u64 },
        Open {
            session_id: String,
            page_url: String,
            needs: Vec<&'static str>,
            markers: Vec<serde_json::Value>,
            verdict: String,
            expires_in_secs: Option<u64>,
        },
    }

    let outcome = {
        let mut mgr = crate::session::SESSIONS.lock().await;
        mgr.evict_expired();
        // TTL 1800: a slow human QR scan must not get evicted mid-handoff
        // (default idle TTL would). No start_url: the Navigate below is the
        // awaited initial load, so the probe never races an in-flight goto
        // (same stance as verify).
        let sid = mgr.create(
            None,
            use_proxy,
            vec![],
            None,
            Some(1800),
            None,
            false,
            false,
            Some((owner.to_string(), name.to_string())),
        );
        if let Err(e) = mgr
            .send(&sid, |reply| crate::session::SessionCommand::Navigate {
                url: url.to_string(),
                reply,
            })
            .await
        {
            mgr.close(&sid);
            return Err(format!("navigation failed: {e}"));
        }
        let probe: serde_json::Value = match mgr
            .send(&sid, |reply| crate::session::SessionCommand::Eval {
                script: GATE_PROBE.to_string(),
                timeout_ms: Some(10_000),
                reply,
            })
            .await
        {
            Ok(v) => {
                let text = v.as_str().ok_or("gate probe returned a non-string")?;
                match serde_json::from_str(text) {
                    Ok(p) => p,
                    Err(e) => {
                        mgr.close(&sid);
                        return Err(format!("gate probe result parse: {e}"));
                    }
                }
            }
            Err(e) => {
                mgr.close(&sid);
                return Err(format!("gate probe eval: {e}"));
            }
        };
        let needs = classify_gates(&probe);
        let verdict = match mgr
            .send(&sid, |reply| crate::session::SessionCommand::Verdict { reply })
            .await
        {
            Ok(sheet) => serde_json::from_str::<serde_json::Value>(&sheet)
                .map(|s| {
                    s["verdict"].as_str().unwrap_or("unknown").to_string()
                })
                .unwrap_or_else(|_| "unknown".to_string()),
            Err(e) => {
                mgr.close(&sid);
                return Err(format!("verdict: {e}"));
            }
        };
        let expires_in_secs = mgr.expires_in_secs(&sid);

        let mut logged_in = None;
        if let Some(pred) = predicate {
            if needs.is_empty() {
                let timeout = timeout_ms.clamp(1_000, 120_000);
                let pred = pred.to_string();
                if let Ok(payload) = mgr
                    .send(&sid, |reply| crate::session::SessionCommand::Wait {
                        selector: None,
                        predicate: Some(pred),
                        timeout_ms: timeout,
                        reply,
                    })
                    .await
                {
                    let elapsed_ms = serde_json::from_str::<serde_json::Value>(&payload)
                        .ok()
                        .and_then(|v| v["elapsed_ms"].as_u64())
                        .unwrap_or(0);
                    let success_url = mgr
                        .send(&sid, |reply| crate::session::SessionCommand::Url { reply })
                        .await
                        .map_err(|e| e.to_string())?;
                    mgr.close(&sid);
                    logged_in = Some((success_url, elapsed_ms));
                }
                // Wait Err (timeout): fall through with the session open.
            }
        }

        match logged_in {
            Some((success_url, elapsed_ms)) => Outcome::LoggedIn {
                success_url,
                elapsed_ms,
            },
            None => Outcome::Open {
                session_id: sid,
                page_url: probe["url"].as_str().unwrap_or(url).to_string(),
                needs,
                markers: probe["markers"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default(),
                verdict,
                expires_in_secs,
            },
        }
    };

    // Phase 2, guard released: verify locks SESSIONS itself, so it must run
    // outside the block above. The success-time URL teaches the spec — the
    // login page itself usually keeps the predicate false on a revisit.
    match outcome {
        Outcome::LoggedIn {
            success_url,
            elapsed_ms,
        } => {
            let verify = match verify(owner, name, Some(&success_url), predicate).await {
                Ok(v) => v,
                // The login itself succeeded and the cookies persisted; a
                // probe failure is not a login failure.
                Err(e) => serde_json::json!({"error": e}),
            };
            Ok(serde_json::json!({
                "name": name,
                "status": "logged_in",
                "url": success_url,
                "elapsed_ms": elapsed_ms,
                "verify": verify,
            }))
        }
        Outcome::Open {
            session_id,
            page_url,
            needs,
            markers,
            verdict,
            expires_in_secs,
        } => {
            let mut reply = serde_json::json!({
                "name": name,
                "status": if predicate.is_some() { "waiting" } else { "opened" },
                "needs": needs,
                "markers": markers,
                "verdict": verdict,
                "url": page_url,
                "session_id": session_id,
                "expires_in_secs": expires_in_secs,
            });
            if needs.is_empty() {
                reply["next"] = serde_json::json!(format!(
                    "poll session_wait {{session_id, predicate}} (the automatic bounce may \
                     just be slow), or drive the login with session_input/session_click on \
                     {session_id}; afterwards stamp with account_verify {{name, url, \
                     predicate}} — cookies write back after every action"
                ));
            } else {
                reply["handoff"] = serde_json::json!(format!(
                    "human step ({}) — open /live?session={} on this engine's HTTP port and \
                     finish it there; cookies write back after every action, so afterwards \
                     re-call account_login with the same arguments (the account's shared jar \
                     already holds the finished login), or poll session_wait {{session_id, \
                     predicate}} and stamp with account_verify {{name, url, predicate}}",
                    needs.join(", "),
                    session_id
                ));
            }
            Ok(reply)
        }
    }
}

/// JS truthiness over a JSON value — the predicate's contract is "truthy when
/// logged in", and the round-trip flattens JS values to JSON, so the same
/// falsy set applies: false, 0, "", null, missing.
fn json_truthy(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        serde_json::Value::String(s) => !s.is_empty() && s != "false" && s != "0",
        serde_json::Value::Object(_) | serde_json::Value::Array(_) => true,
        serde_json::Value::Null => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The seed blob (#141): captured storage replays only with the origin
    /// it was captured from attached; nothing-empty, blank-origin and
    /// url-less captures all stay `None`.
    #[test]
    fn storage_seed_is_origin_gated_from_the_record() {
        let rec = serde_json::json!({
            "url": "https://www.xinzao.com/dashboard?id=7",
            "local_storage": {"token": "abc"},
            "session_storage": {},
        });
        let seed = storage_seed_from_record(&rec).expect("storage present");
        assert_eq!(seed["origin"], "https://www.xinzao.com");
        assert_eq!(seed["local_storage"]["token"], "abc");
        assert_eq!(seed["session_storage"], serde_json::json!({}));

        // Both maps empty -> nothing worth replaying.
        let empty = serde_json::json!({
            "url": "https://www.xinzao.com/",
            "local_storage": {},
            "session_storage": {},
        });
        assert!(storage_seed_from_record(&empty).is_none());

        // A capture taken on about:blank carries no real origin — never
        // replay those keys onto a real site.
        let blank = serde_json::json!({
            "url": "about:blank",
            "local_storage": {"token": "abc"},
            "session_storage": {},
        });
        assert!(storage_seed_from_record(&blank).is_none());

        // Storage without a captured url has no origin to gate on — stay
        // conservative rather than fire unconditionally.
        let nolink = serde_json::json!({"local_storage": {"token": "a"}});
        assert!(storage_seed_from_record(&nolink).is_none());
    }

    /// `url_origin` mirrors `location.origin`: default ports fold away,
    /// explicit ports stay, non-http(s) schemes are `None`.
    #[test]
    fn url_origin_matches_location_origin_semantics() {
        assert_eq!(
            url_origin("https://a.com/x/y?q=1"),
            Some("https://a.com".to_string())
        );
        assert_eq!(
            url_origin("http://a.com:8080/"),
            Some("http://a.com:8080".to_string())
        );
        assert_eq!(
            url_origin("https://a.com:443/"),
            Some("https://a.com".to_string()),
            "default port folds, like location.origin"
        );
        assert_eq!(url_origin("about:blank"), None);
        assert_eq!(url_origin("not a url"), None);
    }

    fn scrub() {
        // Name-spaced key so parallel tests can't collide.
        LIVE_JARS
            .lock()
            .unwrap()
            .retain(|(o, _), _| o.starts_with("keep:"));
    }

    #[test]
    fn account_names_are_validated() {
        assert!(validate_name("taobao-scraper").is_ok());
        assert!(validate_name("A_9").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("采集号").is_err());
        assert!(validate_name("has space").is_err());
        assert!(validate_name("a/b").is_err());
        assert!(validate_name(&"x".repeat(65)).is_err());
        assert!(validate_name(&"x".repeat(64)).is_ok());
    }

    // The crown invariant of the multi-account layer: same cookie name,
    // different accounts, no clobbering — and none of them leak into the
    // process-global anonymous jar (that linkage is exactly what a
    // risk-control system reads).
    #[test]
    fn two_accounts_same_cookie_name_do_not_clobber() {
        scrub();
        let a = jar_for("keep:test-owner", "scraper");
        let b = jar_for("keep:test-owner", "publisher");
        let url = url::Url::parse("https://www.taobao.com/").unwrap();
        a.set_cookie("cookie2=SCRAPER; Domain=.taobao.com; Path=/", &url);
        b.set_cookie("cookie2=PUBLISHER; Domain=.taobao.com; Path=/", &url);

        let from_a = a.get_cookie_header(&url);
        let from_b = b.get_cookie_header(&url);
        assert!(from_a.contains("SCRAPER"), "got: {from_a}");
        assert!(!from_a.contains("PUBLISHER"), "got: {from_a}");
        assert!(from_b.contains("PUBLISHER"), "got: {from_b}");
        assert!(!from_b.contains("SCRAPER"), "got: {from_b}");

        // And the shared jar stays clean of both.
        let shared = crate::server::shared_cookie_jar_for_tests();
        let shared_header = shared.get_cookie_header(&url);
        assert!(
            !shared_header.contains("SCRAPER") && !shared_header.contains("PUBLISHER"),
            "shared jar leaked: {shared_header}"
        );
        // Restore the shared jar for other tests.
        shared.clear();
        scrub();
    }

    #[test]
    fn concurrent_jar_for_returns_same_jar() {
        scrub();
        let a = jar_for("keep:test-owner", "warm");
        let b = jar_for("keep:test-owner", "warm");
        let url = url::Url::parse("https://example.com/").unwrap();
        a.set_cookie("sid=1; Domain=example.com; Path=/", &url);
        assert!(b.get_cookie_header(&url).contains("sid=1"));
        scrub();
    }

    #[test]
    fn seed_jar_anchors_on_domain_attribute() {
        let jar = CookieJar::new();
        seed_jar(
            &jar,
            &[
                "cookie2=t; Domain=.taobao.com; Path=/".to_string(),
                "bare=nodomain".to_string(), // skipped — nothing to anchor on
            ],
        );
        let taobao = url::Url::parse("https://h5api.m.taobao.com/").unwrap();
        let header = jar.get_cookie_header(&taobao);
        assert!(
            header.contains("cookie2=t"),
            "subdomain sees the cookie: {header}"
        );
        let other = url::Url::parse("https://example.com/").unwrap();
        assert!(!jar.get_cookie_header(&other).contains("cookie2"));
    }

    // #203 (taobao half): the account record / session snapshot carries the
    // session `Cookies` export form. Before Expires rode along, every
    // revived session replayed cookies the server had already retired —
    // taobao's risk face answers that with sessionExpired + a havana login
    // bounce. Seeding must drop dead-dated entries like Chrome does.
    #[test]
    fn seed_jar_honors_expires_from_the_export_form() {
        let jar = CookieJar::new();
        seed_jar(
            &jar,
            &[
                "dead=old; Domain=.taobao.com; Path=/; Expires=Thu, 01 Jan 1970 00:00:00 GMT".to_string(),
                "live=fresh; Domain=.taobao.com; Path=/; Expires=Fri, 01 Jan 2027 00:00:00 GMT".to_string(),
                "session=ok; Domain=.taobao.com; Path=/".to_string(),
            ],
        );
        let url = url::Url::parse("https://item.upload.taobao.com/").unwrap();
        let header = jar.get_cookie_header(&url);
        assert!(header.contains("live=fresh"), "got: {header}");
        assert!(header.contains("session=ok"), "got: {header}");
        assert!(!header.contains("dead="), "expired entry must not replay: {header}");
    }

    #[test]
    fn classify_gates_maps_probe_counts_to_needs() {
        let probe = |pw, sms, qr, sl| {
            serde_json::json!({
                "url": "https://x.com/login", "password": pw, "sms": sms, "qr": qr,
                "slider": sl, "markers": []
            })
        };
        assert_eq!(classify_gates(&probe(1, 0, 0, 0)), ["password"]);
        assert_eq!(classify_gates(&probe(0, 2, 0, 0)), ["sms"]);
        // The xiaohongshu shape: a QR surface and nothing else.
        assert_eq!(classify_gates(&probe(0, 0, 1, 0)), ["qr"]);
        assert_eq!(classify_gates(&probe(0, 0, 0, 3)), ["slider"]);
        assert_eq!(
            classify_gates(&probe(1, 1, 1, 1)),
            ["password", "sms", "qr", "slider"]
        );
        // No gates → empty → the wizard takes the wait path.
        assert!(classify_gates(&probe(0, 0, 0, 0)).is_empty());
        // Missing fields read as zero, never a panic.
        assert!(classify_gates(&serde_json::json!({"url": "https://a.io/"})).is_empty());
    }

    #[test]
    fn use_proxy_record_wins_over_param() {
        // One identity, one egress: the recorded route survives a later
        // call's default-false param; the param only seeds a record without
        // one.
        assert!(resolve_use_proxy(&serde_json::json!({"use_proxy": true}), false));
        assert!(resolve_use_proxy(&serde_json::json!({"use_proxy": true}), true));
        assert!(resolve_use_proxy(&serde_json::json!({}), true));
        assert!(!resolve_use_proxy(&serde_json::json!({}), false));
    }

    // The pool is the persona's TLS-coherence contract: Chrome 145 only
    // (zero mismatch with the fleet-claimed chrome155), both desktop OSes
    // present so Win/Mac is the differentiation axis — never Linux/X11.
    #[test]
    fn persona_pool_is_chrome155_win_and_mac() {
        let pool = persona_ua_pool();
        assert!(pool.len() >= 2, "pool: {pool:?}");
        for ua in &pool {
            assert!(ua.contains("Chrome/155.0.0.0"), "out of family: {ua}");
            assert!(
                !ua.contains("Linux") && !ua.contains("X11"),
                "Linux must stay out of persona pool: {ua}"
            );
            assert!(
                ua.contains("Windows NT") || ua.contains("Macintosh"),
                "pool entry must be Win or Mac: {ua}"
            );
        }
        assert!(pool.iter().any(|u| u.contains("Windows NT")));
        assert!(pool.iter().any(|u| u.contains("Macintosh")));
    }

    #[test]
    fn coerce_ua_rewrites_chrome_major_mismatch() {
        let bad = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/154.0.0.0 Safari/537.36";
        let fixed = coerce_ua_to_tls(bad);
        assert!(fixed.contains("Chrome/155.0.0.0"), "got {fixed}");
        assert!(fixed.contains("Macintosh"), "got {fixed}");
        let good = default_mac_chrome155_ua();
        assert_eq!(coerce_ua_to_tls(good), good);
    }

    // persona_for's teach-once splits into these pure halves because the
    // global store behind load/save is a OnceLock pinned at first use — env
    // knobs can't re-point it per test. Persistence is covered by the store
    // roundtrip tests.
    #[test]
    fn draw_persona_honors_hint_then_pool() {
        let real = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/152.0.0.0 Safari/537.36";
        // Out-of-family Chrome major is rewritten onto fleet chrome155 same-OS.
        let drawn = draw_persona(Some(real)).user_agent;
        assert!(drawn.contains("Chrome/155.0.0.0"), "got {drawn}");
        assert!(drawn.contains("Windows NT"), "got {drawn}");
        // Empty hints fall through to the pool — membership, not two random
        // draws agreeing (the pool holds two UAs, so that's a coin flip).
        let blank = draw_persona(Some("  "));
        assert!(
            persona_ua_pool().contains(&blank.user_agent.as_str()),
            "blank hint should draw from the pool, got {}",
            blank.user_agent
        );
        for _ in 0..20 {
            let p = draw_persona(None);
            assert!(
                persona_ua_pool().contains(&p.user_agent.as_str()),
                "drawn UA outside pool: {}",
                p.user_agent
            );
            assert!(
                !p.user_agent.contains("Linux"),
                "drawn Linux UA: {}",
                p.user_agent
            );
        }
        // Distinct identities get distinct hardware (collision odds 2^-64).
        assert_ne!(draw_persona(None).fp_seed, draw_persona(None).fp_seed);
    }

    #[test]
    fn read_persona_roundtrip_and_rejects_partial() {
        let p = draw_persona(None);
        let record =
            serde_json::json!({"persona": {"fp_seed": p.fp_seed, "user_agent": p.user_agent}});
        let back = read_persona(&record).expect("complete persona reads back");
        assert_eq!(back.fp_seed, p.fp_seed);
        assert_eq!(back.user_agent, p.user_agent);
        // Partial or empty entries mean "draw fresh", never a half persona.
        assert!(read_persona(&serde_json::json!({})).is_none());
        assert!(read_persona(&serde_json::json!({"persona": {"fp_seed": 7}})).is_none());
        assert!(
            read_persona(&serde_json::json!({"persona": {"fp_seed": 7, "user_agent": ""}}))
                .is_none()
        );
    }

    // teach_persona is the in-lock half of persona_for: whatever it decides
    // is what lands in the store under one lock hold. These cover the three
    // outcomes without the global store (OnceLock-pinned, see above).
    #[test]
    fn teach_persona_inserts_normalizes_and_keeps() {
        let drawn = draw_persona(Some("Mozilla/5.0 (Windows NT 10.0; Win64; x64) Chrome/155.0.0.0"));
        // Absent → insert drawn.
        let mut rec = serde_json::json!({"version": 1});
        assert_eq!(teach_persona(&mut rec, &drawn).fp_seed, drawn.fp_seed);
        assert_eq!(rec["persona"]["fp_seed"], drawn.fp_seed);
        // Present + coherent → returned as-is, record untouched.
        let again = teach_persona(&mut rec, &drawn);
        assert_eq!(again.fp_seed, drawn.fp_seed);
        assert_eq!(rec["persona"]["fp_seed"], drawn.fp_seed);
        // Present + stale Chrome major → normalized onto the TLS-coherent
        // UA, keeping the fp_seed (the device identity survives the UA fix).
        let mut stale = serde_json::json!({
            "persona": {"fp_seed": 42, "user_agent":
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) Chrome/154.0.0.0 Safari/537.36"}
        });
        let normalized = teach_persona(&mut stale, &drawn);
        assert_eq!(normalized.fp_seed, 42);
        assert!(normalized.user_agent.contains("Chrome/155.0.0.0"), "got {}", normalized.user_agent);
        assert_eq!(stale["persona"]["fp_seed"], 42);
        // Partial persona → drawn (a half persona is "no persona yet").
        let mut partial = serde_json::json!({"persona": {"fp_seed": 9}});
        let p = teach_persona(&mut partial, &drawn);
        assert_eq!(p.fp_seed, drawn.fp_seed);
        assert_eq!(partial["persona"]["user_agent"], drawn.user_agent);
    }
}
