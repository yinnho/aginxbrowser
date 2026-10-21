//! Receipt timing: wall clock, per-step breakdown, live-QR pointer.

use serde_json::{json, Value};

/// Step start offsets (ms since the run began) recorded by the executor:
/// (pc, step id or op, start_ms). Durations are the gaps between starts, so a
/// step that fails mid-flight still gets its time.
pub(super) type StepStarts = std::sync::Arc<std::sync::Mutex<Vec<(usize, String, u128)>>>;

/// Run a flow and stamp the receipt with its wall clock (`elapsed_ms`) and a
/// per-step breakdown (`timings`: [{step, id, ms}]) — the publish-speed
/// numbers, present on ok AND failed receipts.
pub(super) async fn with_timing<F, Fut>(run: F) -> Value
where
    F: FnOnce(StepStarts) -> Fut,
    Fut: std::future::Future<Output = Value>,
{
    let t: StepStarts = Default::default();
    let started = std::time::Instant::now();
    let mut receipt = run(t.clone()).await;
    let total = started.elapsed().as_millis();
    if let Some(obj) = receipt.as_object_mut() {
        let starts = t.lock().map(|g| g.clone()).unwrap_or_default();
        let mut rows = Vec::with_capacity(starts.len());
        for (i, (pc, id, at)) in starts.iter().enumerate() {
            let end = starts.get(i + 1).map(|n| n.2).unwrap_or(total);
            rows.push(json!({ "step": pc, "id": id, "ms": end.saturating_sub(*at) as u64 }));
        }
        obj.insert("elapsed_ms".into(), json!(total as u64));
        // A person scanning a login code needs the code the page shows NOW,
        // not a screenshot that expired on the way: every receipt names the
        // self-refreshing scan page of its session.
        if let Some(sid) = obj.get("session_id").and_then(|v| v.as_str()).map(str::to_string) {
            obj.insert("live_qr".into(), json!(format!("/session/{sid}/qr/live")));
        }
        obj.insert("timings".into(), Value::Array(rows));
    }
    receipt
}
