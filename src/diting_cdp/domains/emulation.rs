use serde_json::{json, Value};

use crate::diting_cdp::dispatch::CdpContext;

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
            // Validate exactly as Chromium does even though the single-realm
            // engine has no compositor viewport to resize yet: clients (and
            // tests) rely on the error surface for malformed input.
            let _width = metric_dimension(params, "width")?;
            let _height = metric_dimension(params, "height")?;
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
            if params.get("mobile").and_then(Value::as_bool).is_none() {
                return Err("Emulation.setDeviceMetricsOverride requires boolean mobile".to_string());
            }
            optional_metric_dimension(params, "screenWidth")?;
            optional_metric_dimension(params, "screenHeight")?;
            Ok(json!({}))
        }
        "clearDeviceMetricsOverride" => Ok(json!({})),
        "setDefaultBackgroundColorOverride" => {
            default_background_color(params)?;
            Ok(json!({}))
        }
        // Touch emulation does not affect layout; ack for compatibility.
        "setTouchEmulationEnabled" => Ok(json!({})),
        "setFocusEmulationEnabled" => Ok(json!({})),
        "setEmulatedMedia" => Ok(json!({})),
        "setUserAgentOverride" => {
            let ua = params.get("userAgent").and_then(|v| v.as_str()).unwrap_or("");
            if !ua.is_empty() {
                if let Some(page) = ctx.get_session_page(session_id) {
                    page.http_client.set_user_agent(ua).await;
                }
            }
            Ok(json!({}))
        }
        _ => Ok(json!({})),
    }
}
