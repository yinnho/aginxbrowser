(function () {
  if (typeof globalThis !== "undefined" && typeof globalThis.abxProbeBearer === "function") {
    return;
  }
  // Shared X bearer probe — scrape Bearer from live main.*.js when cache >12h.
  async function abxProbeBearer() {
    const at = Number(localStorage.getItem("abx_bearer_at") || 0);
    const cur = localStorage.getItem("abx_bearer") || "";
    const fresh = cur && at && Date.now() - at < 12 * 3600e3;
    if (fresh) return JSON.stringify({ ok: true, src: "cached", len: cur.length });
    const src = [...document.querySelectorAll("script[src*='main']")].map((s) => s.src)[0];
    if (!src) return JSON.stringify({ ok: !!cur, src: "kept-no-main", len: cur.length });
    const t = await (await fetch(src, { credentials: "omit" })).text();
    const m = t.match(/Bearer ([A-Za-z0-9%/+=]{80,})/);
    if (!m) return JSON.stringify({ ok: !!cur, src: "kept-no-match", len: cur.length });
    localStorage.setItem("abx_bearer", m[1]);
    localStorage.setItem("abx_bearer_at", String(Date.now()));
    return JSON.stringify({ ok: true, src: "refreshed", len: m[1].length });
  }
  if (typeof globalThis !== "undefined") globalThis.abxProbeBearer = abxProbeBearer;
})();
