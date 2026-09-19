#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleState {
    Idle,
    Loading,
    DomContentLoaded,
    Loaded,
    NetworkIdle,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitUntil {
    Load,
    DomContentLoaded,
    /// Puppeteer's `networkidle0` / `networkidle2`: navigate_single keeps a
    /// 500ms quiet-window wait with the matching in-flight threshold for
    /// these. Only reachable via `from_str` today — nothing in the service
    /// layer accepts a waitUntil string yet (firecrawl-compat is the natural
    /// future caller), hence the allow.
    NetworkIdle0,
    NetworkIdle2,
}

impl WaitUntil {
    /// Parse a puppeteer/playwright `waitUntil` string. Unrecognized values
    /// fall back to `load`, matching Playwright's tolerance.
    #[allow(dead_code)] // see the NetworkIdle0/2 doc above: one unit, one caller-to-be
    pub fn from_str(s: &str) -> Self {
        match s {
            "domcontentloaded" => WaitUntil::DomContentLoaded,
            "networkidle0" | "networkIdle" | "networkidle" => WaitUntil::NetworkIdle0,
            "networkidle2" => WaitUntil::NetworkIdle2,
            _ => WaitUntil::Load,
        }
    }
}
