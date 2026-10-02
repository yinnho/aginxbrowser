//! Durable local store: everything fetched or searched through this process
//! is persisted to a SQLite database (default `~/.aginxbrowser/cache.db`) so
//! an agent can query what it has already seen instead of re-fetching.
//!
//! Layout: `pages` (one row per fetched URL, FTS5-indexed) + `searches`
//! (whole result sets, lookup by substring — small table, no FTS needed).
//! The FTS index is contentless (`content=''`, `contentless_delete=1`) and
//! synced manually from `record_fetch`, which lets us index CJK-split text
//! (one token per character) while keeping the original text in `pages` —
//! unicode61 can't segment CJK, so a plain index would make every Chinese
//! query a single unmatchable token.
//!
//! Multi-tenant note: rows carry an `owner`. `AGINXBROWSER_STORE_SCOPE`
//! defaults to `global` (single-user instances share one pool); set to
//! `session` on multi-client deployments so each session only sees its
//! own rows.

use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};

pub const REST_OWNER: &str = "rest";

const DEFAULT_PAGE_TTL_HOURS: i64 = 24 * 30;
const DEFAULT_SEARCH_TTL_HOURS: i64 = 24 * 7;
const PURGE_INTERVAL_SECS: i64 = 600;

static STORE: OnceLock<Mutex<Option<Store>>> = OnceLock::new();
static LAST_PURGE: AtomicI64 = AtomicI64::new(0);

struct Store {
    conn: Connection,
}

// ---------------------------------------------------------------------------
// Configuration (env, read per call so operators can flip without init order)
// ---------------------------------------------------------------------------

fn enabled() -> bool {
    match std::env::var("AGINXBROWSER_STORE") {
        Ok(v) => !matches!(v.trim(), "0" | "false" | "off"),
        Err(_) => true,
    }
}

fn db_path() -> PathBuf {
    // Ephemeral is the no-disk-state contract — it outranks every path
    // knob. :memory: works because with_store holds one connection for the
    // process lifetime.
    if crate::config::ephemeral() {
        return PathBuf::from(":memory:");
    }
    if let Ok(p) = std::env::var("AGINXBROWSER_STORE_PATH") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    // An isolated instance (STORAGE_DIR / COOKIE_STORE_DIR pinned by e.g.
    // the "isolated private database" repro in #203) must not reach into
    // the shared store: session snapshots live here, and restore_all()
    // would otherwise revive foreign sessions — stale localStorage ghosts
    // included — straight into the "fresh" instance.
    if let Ok(dir) = std::env::var("AGINXBROWSER_STORAGE_DIR")
        .or_else(|_| std::env::var("AGINXBROWSER_COOKIE_STORE_DIR"))
    {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("cache.db");
        }
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_default();
    if home.is_empty() {
        PathBuf::from("cache.db")
    } else {
        PathBuf::from(home).join(".aginxbrowser").join("cache.db")
    }
}

fn scope_global() -> bool {
    std::env::var("AGINXBROWSER_STORE_SCOPE")
        .map(|v| v.trim() != "session")
        .unwrap_or(true)
}

fn page_ttl_hours() -> i64 {
    env_hours("AGINXBROWSER_STORE_TTL_HOURS", DEFAULT_PAGE_TTL_HOURS)
}

fn search_ttl_hours() -> i64 {
    env_hours(
        "AGINXBROWSER_STORE_SEARCH_TTL_HOURS",
        DEFAULT_SEARCH_TTL_HOURS,
    )
}

fn env_hours(name: &str, default: i64) -> i64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|&h| h > 0)
        .unwrap_or(default)
}

/// Canonical owner under the active scope. Pub for the account layer: its
/// live-jar registry keys must agree with the store rows, so every face
/// (e.g. the REST owner "rest") converges on one account under the
/// default global scope.
pub fn norm_owner(owner: &str) -> String {
    if scope_global() {
        "global".to_string()
    } else {
        owner.to_string()
    }
}

// ---------------------------------------------------------------------------
// URL normalization + CJK text prep
// ---------------------------------------------------------------------------

/// Canonical form used for dedup: lowercase scheme/host, drop default port
/// and fragment, strip trailing empty query. Path/query kept verbatim.
fn normalize_url(raw: &str) -> String {
    let parsed = match url::Url::parse(raw) {
        Ok(u) => u,
        Err(_) => return raw.trim().to_string(),
    };
    let mut out = format!("{}://", parsed.scheme());
    match (parsed.host_str(), parsed.port()) {
        (Some(h), Some(p)) => {
            let default =
                (parsed.scheme() == "https" && p == 443) || (parsed.scheme() == "http" && p == 80);
            if default {
                out.push_str(&h.to_lowercase());
            } else {
                out.push_str(&format!("{}:{}", h.to_lowercase(), p));
            }
        }
        (Some(h), None) => out.push_str(&h.to_lowercase()),
        _ => {}
    }
    out.push_str(parsed.path());
    if let Some(q) = parsed.query() {
        if !q.is_empty() {
            out.push('?');
            out.push_str(q);
        }
    }
    out
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x30FF   // kana
        | 0x3400..=0x4DBF // CJK ext A
        | 0x4E00..=0x9FFF // CJK unified
        | 0xAC00..=0xD7AF // hangul
        | 0xF900..=0xFAFF // CJK compat
    )
}

/// One token per CJK character (spaces inserted) so the unicode61 tokenizer
/// can match Chinese substrings as FTS phrases.
fn split_cjk(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len() * 2);
    for (i, &c) in chars.iter().enumerate() {
        let cjk = is_cjk(c);
        if cjk {
            if i > 0 {
                out.push(' ');
            }
        } else if i > 0 && is_cjk(chars[i - 1]) {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

// ---------------------------------------------------------------------------
// Store core (unit-testable without touching process-global state)
// ---------------------------------------------------------------------------

impl Store {
    fn open(path: &std::path::Path) -> Result<Store, String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
        }
        let conn = Connection::open(path).map_err(|e| e.to_string())?;
        let _: String = conn
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        conn.execute_batch(
            "PRAGMA synchronous=NORMAL;
             PRAGMA busy_timeout=5000;
             CREATE TABLE IF NOT EXISTS pages (
                 id INTEGER PRIMARY KEY,
                 owner TEXT NOT NULL,
                 url TEXT NOT NULL,
                 norm_url TEXT NOT NULL,
                 title TEXT NOT NULL DEFAULT '',
                 content TEXT NOT NULL DEFAULT '',
                 tier TEXT NOT NULL DEFAULT '',
                 truncated INTEGER NOT NULL DEFAULT 0,
                 content_hash TEXT NOT NULL DEFAULT '',
                 fetched_at INTEGER NOT NULL,
                 expires_at INTEGER NOT NULL,
                 UNIQUE(owner, norm_url)
             );
             CREATE INDEX IF NOT EXISTS idx_pages_expires ON pages(expires_at);
             CREATE INDEX IF NOT EXISTS idx_pages_owner_time ON pages(owner, fetched_at);
             CREATE TABLE IF NOT EXISTS searches (
                 id INTEGER PRIMARY KEY,
                 owner TEXT NOT NULL,
                 query TEXT NOT NULL,
                 categories TEXT NOT NULL DEFAULT '',
                 n_results INTEGER NOT NULL DEFAULT 0,
                 results_json TEXT NOT NULL DEFAULT '[]',
                 searched_at INTEGER NOT NULL,
                 expires_at INTEGER NOT NULL,
                 UNIQUE(owner, query, categories)
             );
             CREATE INDEX IF NOT EXISTS idx_searches_expires ON searches(expires_at);
             CREATE VIRTUAL TABLE IF NOT EXISTS pages_fts USING fts5(
                 title, content, url, content='', contentless_delete=1
             );
             CREATE TABLE IF NOT EXISTS session_snapshots (
                 id TEXT PRIMARY KEY,
                 snapshot TEXT NOT NULL,
                 updated_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS accounts (
                 owner TEXT NOT NULL,
                 name TEXT NOT NULL,
                 record TEXT NOT NULL,
                 updated_at INTEGER NOT NULL,
                 PRIMARY KEY (owner, name)
             );",
        )
        .map_err(|e| e.to_string())?;
        // Migration for stores created before drift tracking: consecutive-sample
        // hashes (prev_hash/prev_fetched_at) power changed_since_prev.
        for (col, ddl) in [
            (
                "prev_hash",
                "ALTER TABLE pages ADD COLUMN prev_hash TEXT NOT NULL DEFAULT ''",
            ),
            (
                "prev_fetched_at",
                "ALTER TABLE pages ADD COLUMN prev_fetched_at INTEGER NOT NULL DEFAULT 0",
            ),
        ] {
            let known: bool = conn
                .query_row(
                    "SELECT COUNT(*) FROM pragma_table_info('pages') WHERE name=?1",
                    params![col],
                    |r| r.get::<_, i64>(0),
                )
                .map(|n| n > 0)
                .unwrap_or(true);
            if !known {
                let _ = conn.execute(ddl, []);
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
            for suffix in ["-wal", "-shm"] {
                let mut p = path.as_os_str().to_os_string();
                p.push(suffix);
                let _ = std::fs::set_permissions(
                    std::path::PathBuf::from(p),
                    std::fs::Permissions::from_mode(0o600),
                );
            }
        }
        Ok(Store { conn })
    }

    /// Upsert one page sample; returns `(content_hash, changed_since_prev)`
    /// so the fetch response can carry the drift receipt. `("", None)` when
    /// there is nothing to record.
    fn record_fetch(
        &self,
        owner: &str,
        url: &str,
        title: &str,
        content: &str,
        tier: &str,
        truncated: bool,
    ) -> (String, Option<bool>) {
        if content.is_empty() {
            return (String::new(), None);
        }
        let norm = normalize_url(url);
        let hash = hex(&Sha256::digest(content.as_bytes()));
        let ts = now();
        let expires = ts + page_ttl_hours() * 3600;
        // Keep the previous sample's hash so repeated fetches of the same
        // source expose drift (a rate-limited origin serving frozen 200s
        // shows up as changed_since_prev=false across samples).
        let prev: (String, i64) = self
            .conn
            .query_row(
                "SELECT content_hash, fetched_at FROM pages WHERE owner=?1 AND norm_url=?2",
                params![owner, norm],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap_or_default();
        let changed = (!prev.0.is_empty()).then(|| prev.0 != hash);
        let res = self.conn.query_row(
            "INSERT INTO pages (owner, url, norm_url, title, content, tier, truncated,
                                content_hash, prev_hash, prev_fetched_at, fetched_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(owner, norm_url) DO UPDATE SET
                 url=excluded.url, title=excluded.title, content=excluded.content,
                 tier=excluded.tier, truncated=excluded.truncated,
                 content_hash=excluded.content_hash,
                 prev_hash=excluded.prev_hash, prev_fetched_at=excluded.prev_fetched_at,
                 fetched_at=excluded.fetched_at, expires_at=excluded.expires_at
             RETURNING id",
            params![
                owner,
                url,
                norm,
                title,
                content,
                tier,
                truncated as i64,
                hash,
                prev.0,
                prev.1,
                ts,
                expires
            ],
            |r| r.get::<_, i64>(0),
        );
        match res {
            Ok(id) => {
                let _ = self
                    .conn
                    .execute("DELETE FROM pages_fts WHERE rowid = ?1", params![id]);
                let _ = self.conn.execute(
                    "INSERT INTO pages_fts (rowid, title, content, url) VALUES (?1, ?2, ?3, ?4)",
                    params![id, split_cjk(title), split_cjk(content), norm],
                );
                (hash, changed)
            }
            Err(e) => {
                tracing::debug!("store: page upsert failed: {e}");
                (String::new(), None)
            }
        }
    }

    fn record_search(
        &self,
        owner: &str,
        query: &str,
        categories: &str,
        results_json: &str,
        n_results: usize,
    ) {
        let ts = now();
        let expires = ts + search_ttl_hours() * 3600;
        if let Err(e) = self.conn.execute(
            "INSERT INTO searches (owner, query, categories, n_results, results_json,
                                   searched_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(owner, query, categories) DO UPDATE SET
                 n_results=excluded.n_results, results_json=excluded.results_json,
                 searched_at=excluded.searched_at, expires_at=excluded.expires_at",
            params![
                owner,
                query,
                categories,
                n_results as i64,
                results_json,
                ts,
                expires
            ],
        ) {
            tracing::debug!("store: search upsert failed: {e}");
        }
    }

    /// Drop expired rows (FTS first — contentless index has no row of its own
    /// to cascade). Throttled so the write path pays this at most every 10 min.
    fn purge_expired(&self) {
        let ts = now();
        let last = LAST_PURGE.load(Ordering::Relaxed);
        if ts - last < PURGE_INTERVAL_SECS {
            return;
        }
        LAST_PURGE.store(ts, Ordering::Relaxed);
        if let Err(e) = self.conn.execute(
            "DELETE FROM pages_fts WHERE rowid IN
                 (SELECT id FROM pages WHERE expires_at < ?1)",
            params![ts],
        ) {
            tracing::debug!("store: fts purge failed: {e}");
        }
        let _ = self
            .conn
            .execute("DELETE FROM pages WHERE expires_at < ?1", params![ts]);
        let _ = self
            .conn
            .execute("DELETE FROM searches WHERE expires_at < ?1", params![ts]);
    }

    // Persistent-session snapshots (feedback ③). Keyed by bare session id —
    // session ids are process-global, so an
    // owner column would add nothing. Snapshots hold login cookies and live
    // next to fetched page content under the same 0600 db.

    fn save_session_snapshot(&self, id: &str, snapshot: &str) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT INTO session_snapshots (id, snapshot, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET snapshot=excluded.snapshot, updated_at=excluded.updated_at",
                params![id, snapshot, now()],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    fn load_session_snapshot(&self, id: &str) -> Result<Option<(String, i64)>, String> {
        self.conn
            .query_row(
                "SELECT snapshot, updated_at FROM session_snapshots WHERE id=?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                e => Err(e.to_string()),
            })
    }

    fn delete_session_snapshot(&self, id: &str) -> Result<bool, String> {
        self.conn
            .execute("DELETE FROM session_snapshots WHERE id=?1", params![id])
            .map(|n| n > 0)
            .map_err(|e| e.to_string())
    }

    fn purge_session_snapshots(&self, max_age_secs: i64) -> Result<usize, String> {
        self.conn
            .execute(
                "DELETE FROM session_snapshots WHERE updated_at < ?1",
                params![now() - max_age_secs],
            )
            .map_err(|e| e.to_string())
    }

    /// Every snapshot id, most recently saved first — startup restore (#88)
    /// walks this list to bring the flushed fleet back under its own ids.
    fn list_session_snapshot_ids(&self) -> Result<Vec<String>, String> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM session_snapshots ORDER BY updated_at DESC")
            .map_err(|e| e.to_string())?;
        let ids = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        Ok(ids)
    }

    // Named login identities (account.rs). Keyed (owner, name) from day one —
    // hosted multi-caller deployments keep callers' accounts separate the
    // same way the cache rows are scoped. Records hold login cookies and live
    // under the same 0600 db as the session snapshots.

    fn save_account(&self, owner: &str, name: &str, record: &str) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT INTO accounts (owner, name, record, updated_at) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(owner, name) DO UPDATE SET record=excluded.record, updated_at=excluded.updated_at",
                params![owner, name, record, now()],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    fn load_account(&self, owner: &str, name: &str) -> Result<Option<(String, i64)>, String> {
        self.conn
            .query_row(
                "SELECT record, updated_at FROM accounts WHERE owner=?1 AND name=?2",
                params![owner, name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                e => Err(e.to_string()),
            })
    }

    fn delete_account(&self, owner: &str, name: &str) -> Result<bool, String> {
        self.conn
            .execute(
                "DELETE FROM accounts WHERE owner=?1 AND name=?2",
                params![owner, name],
            )
            .map(|n| n > 0)
            .map_err(|e| e.to_string())
    }

    /// Metadata-only rows for the account listing — cookie values are
    /// credentials and never leave the store; name, domains and counts are
    /// enough to tell identities apart.
    fn list_accounts(&self, owner: &str) -> Result<Vec<crate::account::AccountSummary>, String> {
        let mut stmt = self
            .conn
            .prepare("SELECT name, record, updated_at FROM accounts WHERE owner=?1 ORDER BY name")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![owner], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for row in rows {
            let (name, record, updated_at) = row.map_err(|e| e.to_string())?;
            let v: serde_json::Value =
                serde_json::from_str(&record).unwrap_or(serde_json::Value::Null);
            let cookies: Vec<String> = v["cookies"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|c| c.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            // Distinct registrable-looking domains from the Domain= attributes.
            let mut domains: Vec<String> = cookies
                .iter()
                .filter_map(|c| {
                    c.split(';')
                        .skip(1)
                        .filter_map(|a| a.trim().split_once('='))
                        .find(|(k, _)| k.trim().eq_ignore_ascii_case("domain"))
                })
                .map(|(_, d)| d.trim().trim_start_matches('.').to_string())
                .collect();
            domains.sort();
            domains.dedup();
            out.push(crate::account::AccountSummary {
                name,
                domains,
                cookie_count: cookies.len(),
                updated_at,
                verify_url: v["verify"]["url"].as_str().map(str::to_string),
                verify_predicate: v["verify"]["predicate"].as_str().map(str::to_string),
                verify_last: v["verify"]["last_result"]
                    .as_object()
                    .cloned()
                    .map(serde_json::Value::Object),
                persona_ua: v["persona"]["user_agent"].as_str().map(str::to_string),
            });
        }
        Ok(out)
    }

}

fn hex(d: &[u8]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// Public API over the process-global store
// ---------------------------------------------------------------------------

fn with_store<T>(f: impl FnOnce(&Store) -> Result<T, String>) -> Result<T, String> {
    let cell = STORE.get_or_init(|| {
        Mutex::new(if enabled() {
            match Store::open(&db_path()) {
                Ok(s) => {
                    tracing::info!("local store open at {}", db_path().display());
                    Some(s)
                }
                Err(e) => {
                    tracing::warn!("local store disabled, open failed: {e}");
                    None
                }
            }
        } else {
            None
        })
    });
    let guard = cell.lock().map_err(|_| "store mutex poisoned")?;
    match guard.as_ref() {
        Some(s) => f(s),
        None => Err("local store disabled".into()),
    }
}

/// Record a successful page fetch and return the drift receipt
/// `(content_hash, changed_since_prev)` for stamping onto the response.
/// Best-effort: failures are logged, never propagated — caching must not
/// break the fetch that produced the data.
pub fn record_fetch(owner: &str, resp: &crate::FetchResponse) -> Option<(String, Option<bool>)> {
    with_store(|st| {
        let receipt = st.record_fetch(
            &norm_owner(owner),
            &resp.url,
            resp.title.as_deref().unwrap_or(""),
            &resp.content,
            resp.tier.unwrap_or(""),
            resp.truncated,
        );
        st.purge_expired();
        Ok(receipt)
    })
    .ok()
}

/// Record a successful search (whole result set). Best-effort, same policy.
pub fn record_search(owner: &str, query: &str, categories: &str, resp: &crate::SearchResponse) {
    let results = serde_json::to_string(&resp.results).unwrap_or_else(|_| "[]".into());
    let _ = with_store(|st| {
        st.record_search(
            &norm_owner(owner),
            query,
            categories,
            &results,
            resp.results.len(),
        );
        Ok(())
    });
}

/// Best-effort snapshot persistence — a failed write degrades the session to
/// in-memory-only, never breaks the command that produced the state.
pub fn save_session_snapshot(id: &str, snapshot: &str) -> Result<(), String> {
    with_store(|st| st.save_session_snapshot(id, snapshot))
}

/// `None` when the store is disabled, the id has no snapshot, or the read
/// failed (a revive then behaves as if the session never existed).
pub fn load_session_snapshot(id: &str) -> Option<(String, i64)> {
    with_store(|st| st.load_session_snapshot(id)).ok().flatten()
}

pub fn delete_session_snapshot(id: &str) -> bool {
    with_store(|st| st.delete_session_snapshot(id)).unwrap_or(false)
}

pub fn purge_session_snapshots(max_age_secs: i64) {
    let _ = with_store(|st| st.purge_session_snapshots(max_age_secs));
}

/// Snapshot ids, most recently saved first; empty when the store is
/// disabled (startup restore then revives nothing, the pre-#88 behavior).
pub fn list_session_snapshot_ids() -> Vec<String> {
    with_store(|st| st.list_session_snapshot_ids()).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Accounts (named login identities — see account.rs)
// ---------------------------------------------------------------------------

pub fn save_account(owner: &str, name: &str, record: &str) -> Result<(), String> {
    with_store(|st| st.save_account(&norm_owner(owner), name, record))
}

pub fn load_account(owner: &str, name: &str) -> Option<(String, i64)> {
    with_store(|st| st.load_account(&norm_owner(owner), name))
        .ok()
        .flatten()
}

pub fn delete_account(owner: &str, name: &str) -> bool {
    with_store(|st| st.delete_account(&norm_owner(owner), name)).unwrap_or(false)
}

pub fn list_accounts(owner: &str) -> Vec<crate::account::AccountSummary> {
    with_store(|st| st.list_accounts(&norm_owner(owner))).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
