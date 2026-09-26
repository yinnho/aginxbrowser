use serde_json::{json, Value};

use crate::cdp::dispatch::CdpContext;

const MAX_DEVICE_METRIC_DIMENSION: i64 = 10_000_000;

fn metric_dimension(params: &Value, name: &str) -> Result<u32, String> {
    let value = params
        .get(name)
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("Emulation.setDeviceMetricsOverride requires integer {name}"))?;
    if !(0..=MAX_DEVICE_METRIC_DIMENSION).contains(&value) {
        return Err(format!(
            "Emulation.setDeviceMetricsOverride {name} must be between 0 and {MAX_DEVICE_METRIC_DIMENSION}"
        ));
    }
    Ok(value as u32)
}

fn optional_metric_dimension(params: &Value, name: &str) -> Result<Option<u32>, String> {
    params
        .get(name)
        .map(|_| metric_dimension(params, name))
        .transpose()
}

fn default_background_color(params: &Value) -> Result<Option<[u8; 4]>, String> {
    let Some(color) = params.get("color") else {
        return Ok(None);
    };
    let color = color.as_object().ok_or(
        "Emulation.setDefaultBackgroundColorOverride color must be an RGBA object",
    )?;
    let channel = |name: &str| -> Result<u8, String> {
        let value = color.get(name).and_then(Value::as_i64).ok_or_else(|| {
            format!(
                "Emulation.setDefaultBackgroundColorOverride requires integer color.{name}"
            )
        })?;
        Ok(value.clamp(0, 255) as u8)
    };
    let alpha = match color.get("a") {
        Some(value) => value.as_f64().ok_or(
            "Emulation.setDefaultBackgroundColorOverride color.a must be a number",
        )?,
        None => 1.0,
    };
    if !alpha.is_finite() {
        return Err("Emulation.setDefaultBackgroundColorOverride color.a must be finite".to_string());
    }
    Ok(Some([
        channel("r")?,
        channel("g")?,
        channel("b")?,
        ((alpha as f32).clamp(0.0, 1.0) * 255.0).round() as u8,
    ]))
}

pub async fn handle(
    method: &str,
    params: &Value,
    ctx: &mut CdpContext,
    session_id: &Option<String>,
) -> Result<Value, String> {
    match method {
        "setDeviceMetricsOverride" => {
            // Chromium's rule: width/height 0 means "keep the current
            // value" — but there is no prior compositor metric to keep on
            // the first call, so 0 falls back to the live innerWidth/
            // innerHeight the persona published.
            let width = metric_dimension(params, "width")?;
            let height = metric_dimension(params, "height")?;
            let device_scale_factor = params
                .get("deviceScaleFactor")
                .and_then(Value::as_f64)
                .ok_or("Emulation.setDeviceMetricsOverride requires deviceScaleFactor")?;
            if !device_scale_factor.is_finite() || device_scale_factor < 0.0 {
                return Err(
                    "Emulation.setDeviceMetricsOverride requires a non-negative finite deviceScaleFactor"
                        .to_string(),
                );
            }
            let mobile = params
                .get("mobile")
                .and_then(Value::as_bool)
                .ok_or("Emulation.setDeviceMetricsOverride requires boolean mobile")?;
            optional_metric_dimension(params, "screenWidth")?;
            optional_metric_dimension(params, "screenHeight")?;
            if let Some(page) = ctx.get_session_page_mut(session_id) {
                let keep = |v: u32| if v == 0 { None } else { Some(v) };
                // deviceScaleFactor: > 0 pins window.devicePixelRatio; 0 is
                // Chromium's "use the default" — the persona's dpr here.
                let dpr = if device_scale_factor == 0.0 { None } else { Some(device_scale_factor) };
                let current = page.evaluate_with_timeout(
                    "innerWidth",
                    std::time::Duration::from_secs(2),
                );
                let current_h = page.evaluate_with_timeout(
                    "innerHeight",
                    std::time::Duration::from_secs(2),
                );
                let w = keep(width)
                    .or_else(|| current.as_f64().map(|v| v as u32))
                    .unwrap_or(1280);
                let h = keep(height)
                    .or_else(|| current_h.as_f64().map(|v| v as u32))
                    .unwrap_or(800);
                page.set_viewport_override(w as f32, h as f32, mobile, dpr);
            }
            Ok(json!({}))
        }
        "clearDeviceMetricsOverride" => {
            if let Some(page) = ctx.get_session_page_mut(session_id) {
                page.clear_viewport_override();
            }
            Ok(json!({}))
        }
        "setDefaultBackgroundColorOverride" => {
            default_background_color(params)?;
            Ok(json!({}))
        }
        // Touch emulation does not affect layout; ack for compatibility.
        "setTouchEmulationEnabled" => Ok(json!({})),
        "setFocusEmulationEnabled" => Ok(json!({})),
        // Media emulation (Playwright's page.emulateMedia / Puppeteer's
        // emulateMediaType): `features` replaces the prefers-* overrides,
        // `media` replaces the media type; a param left out keeps its
        // current value (Chrome semantics). `""` media clears back to
        // screen. Both absent = no-op ack.
        "setEmulatedMedia" => {
            let features = match params.get("features") {
                Some(Value::Array(items)) => {
                    let mut pairs = Vec::with_capacity(items.len());
                    for item in items {
                        let obj = item.as_object().ok_or(
                            "Emulation.setEmulatedMedia features entries must be objects",
                        )?;
                        let name = obj.get("name").and_then(Value::as_str).ok_or(
                            "Emulation.setEmulatedMedia features entries require string name",
                        )?;
                        let value = obj.get("value").and_then(Value::as_str).ok_or(
                            "Emulation.setEmulatedMedia features entries require string value",
                        )?;
                        pairs.push((name.to_string(), value.to_string()));
                    }
                    Some(pairs)
                }
                None => None,
                Some(_) => {
                    return Err(
                        "Emulation.setEmulatedMedia features must be an array".to_string()
                    )
                }
            };
            let media = match params.get("media") {
                // Chrome: null and "" both clear back to screen.
                Some(Value::String(s)) if !s.is_empty() => Some(Some(s.clone())),
                Some(Value::String(_)) | Some(Value::Null) => Some(None),
                None => None,
                Some(_) => {
                    return Err("Emulation.setEmulatedMedia media must be a string".to_string())
                }
            };
            if features.is_some() || media.is_some() {
                if let Some(page) = ctx.get_session_page_mut(session_id) {
                    page.set_emulated_media(features, media);
                }
            }
            Ok(json!({}))
        }
        // Chrome: empty timezoneId clears. Invalid ids error and do not
        // change the page. Date and Intl both read the pin.
        "setTimezoneOverride" => {
            let tz = params.get("timezoneId").and_then(Value::as_str).ok_or(
                "Emulation.setTimezoneOverride requires string timezoneId",
            )?;
            let Some(page) = ctx.get_session_page_mut(session_id) else {
                return Err("Emulation.setTimezoneOverride requires a page target".to_string());
            };
            if tz.is_empty() {
                page.set_timezone_override(None);
                return Ok(json!({}));
            }
            if !page.timezone_id_supported(tz) {
                return Err(format!("Invalid timezoneId: {tz}"));
            }
            page.set_timezone_override(Some(tz.to_string()));
            Ok(json!({}))
        }
        "setUserAgentOverride" => {
            let ua = params.get("userAgent").and_then(|v| v.as_str()).unwrap_or("");
            let lang = params.get("acceptLanguage").and_then(|v| v.as_str());
            let platform = params.get("platform").and_then(|v| v.as_str());
            if let Some(page) = ctx.get_session_page_mut(session_id) {
                page.set_user_agent_override(ua, lang, platform).await;
            }
            Ok(json!({}))
        }
        _ => Ok(json!({})),
    }
}
