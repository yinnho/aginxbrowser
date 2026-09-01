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
//! `session` on public multi-client deployments so each MCP session only
//! sees its own rows.

use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, params_from_iter, Connection};
use serde::Serialize;
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
    if let Ok(p) = std::env::var("AGINXBROWSER_STORE_PATH") {
        if !p.is_empty() {
            return PathBuf::from(p);
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

/// Owner id for one MCP server instance (one stdio process, or one streamable
/// HTTP client session — rmcp constructs the service once per session).
pub fn session_owner() -> String {
    if scope_global() {
        "global".to_string()
    } else {
        format!("s-{}", uuid::Uuid::new_v4())
    }
}

fn norm_owner(owner: &str) -> String {
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
            let default = (parsed.scheme() == "https" && p == 443)
                || (parsed.scheme() == "http" && p == 80);
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

/// Build an FTS5 MATCH expression from free user input. CJK runs become
/// quoted per-character phrases (substring semantics); ASCII words become
/// implicit-AND terms with quote characters stripped. None when nothing
/// searchable remains.
fn fts_query(q: &str) -> Option<String> {
    let mut terms = Vec::new();
    for tok in q.split_whitespace() {
        if tok.chars().any(is_cjk) {
            let phrase = split_cjk(tok);
            if phrase.chars().any(|c| !c.is_whitespace()) {
                terms.push(format!("\"{}\"", phrase));
            }
        } else {
            let cleaned: String = tok.chars().filter(|&c| c != '"').collect();
            if !cleaned.is_empty() {
                terms.push(cleaned);
            }
        }
    }
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" "))
    }
}

fn like_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Snippet centered on the earliest occurrence of any term, bounded to
/// char boundaries.
fn snippet(text: &str, raw_query: &str) -> String {
    let hay = text.to_lowercase();
    let mut hit: Option<usize> = None;
    for term in raw_query.split_whitespace() {
        if let Some(p) = hay.find(&term.to_lowercase()) {
            if hit.is_none_or(|b| p < b) {
                hit = Some(p);
            }
        }
    }
    let bytes = text.len();
    let center = hit.unwrap_or(0);
    let start = floor_char_boundary(text, center.saturating_sub(80));
    let end = ceil_char_boundary(text, (center + 200).min(bytes));
    let mut s = String::new();
    if start > 0 {
        s.push('…');
    }
    s.push_str(text[start..end].trim());
    if end < bytes {
        s.push('…');
    }
    s
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char_boundary(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
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

    fn record_fetch(&self, owner: &str, url: &str, title: &str, content: &str, tier: &str, truncated: bool) {
        if content.is_empty() {
            return;
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
            params![owner, url, norm, title, content, tier, truncated as i64, hash,
                    prev.0, prev.1, ts, expires],
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
            }
            Err(e) => tracing::debug!("store: page upsert failed: {e}"),
        }
    }

    fn record_search(&self, owner: &str, query: &str, categories: &str, results_json: &str, n_results: usize) {
        let ts = now();
        let expires = ts + search_ttl_hours() * 3600;
        if let Err(e) = self.conn.execute(
            "INSERT INTO searches (owner, query, categories, n_results, results_json,
                                   searched_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(owner, query, categories) DO UPDATE SET
                 n_results=excluded.n_results, results_json=excluded.results_json,
                 searched_at=excluded.searched_at, expires_at=excluded.expires_at",
            params![owner, query, categories, n_results as i64, results_json, ts, expires],
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

    fn query(&self, owner: &str, q: &CacheQuery) -> Result<QueryResult, String> {
        self.purge_expired();
        let mut out = QueryResult::default();
        let want_pages = q.kind != "searches";
        let want_searches = q.kind != "pages";
        if want_pages {
            out.pages = self.query_pages(owner, q)?;
        }
        if want_searches {
            out.searches = self.query_searches(owner, q)?;
        }
        let (tp, ts) = self
            .conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM pages WHERE owner=?1),
                        (SELECT COUNT(*) FROM searches WHERE owner=?1)",
                params![owner],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
            )
            .map_err(|e| e.to_string())?;
        out.total_pages = tp;
        out.total_searches = ts;
        Ok(out)
    }

    fn query_pages(&self, owner: &str, q: &CacheQuery) -> Result<Vec<PageHit>, String> {
        let since = q.since_hours.map(|h| now() - h as i64 * 3600);
        let limit = q.limit.clamp(1, 100) as i64;
        // Filters shared by every branch; args must stay index-aligned (?1..?n).
        let mut filter_sql = String::from(" p.owner = ?1");
        let mut filter_args: Vec<rusqlite::types::Value> = vec![owner.to_string().into()];
        if let Some(u) = &q.url {
            filter_sql.push_str(&format!(
                " AND p.norm_url LIKE '%'||?{}||'%' ESCAPE '\\'",
                filter_args.len() + 1
            ));
            filter_args.push(like_escape(u).into());
        }
        if let Some(s) = since {
            filter_sql.push_str(&format!(" AND p.fetched_at >= ?{}", filter_args.len() + 1));
            filter_args.push(s.into());
        }
        let cols = "p.url, p.title, p.content, p.tier, p.truncated, p.fetched_at, p.content_hash";

        // 1) FTS path: bm25() must sit in the same (sub)query as its MATCH.
        if let Some(raw) = q.query.as_deref().filter(|s| !s.trim().is_empty()) {
            if let Some(match_expr) = fts_query(raw) {
                let mph = filter_args.len() + 1;
                let sql = format!(
                    "SELECT {cols} FROM pages p
                     JOIN (SELECT rowid AS rid, bm25(pages_fts) AS rank
                           FROM pages_fts WHERE pages_fts MATCH ?{mph}) f
                       ON f.rid = p.id
                     WHERE {filter_sql}
                     ORDER BY f.rank LIMIT {limit}"
                );
                let mut fargs = filter_args.clone();
                fargs.push(match_expr.into());
                if let Ok(hits) = self.query_page_hits(&sql, &fargs, raw) {
                    if !hits.is_empty() {
                        return Ok(hits);
                    }
                }
            }
            // 2) LIKE fallback: catches unicode61 misses and odd MATCH syntax.
            let base = filter_args.len();
            let sql = format!(
                "SELECT {cols} FROM pages p WHERE {filter_sql}
                 AND (p.title LIKE '%'||?{t}||'%' ESCAPE '\\'
                      OR p.content LIKE '%'||?{c}||'%' ESCAPE '\\'
                      OR p.norm_url LIKE '%'||?{u}||'%' ESCAPE '\\')
                 ORDER BY p.fetched_at DESC LIMIT {limit}",
                t = base + 1,
                c = base + 2,
                u = base + 3,
            );
            let esc = format!("%{}%", like_escape(raw));
            let mut largs = filter_args.clone();
            largs.push(esc.clone().into());
            largs.push(esc.clone().into());
            largs.push(esc.into());
            return self.query_page_hits(&sql, &largs, raw);
        }

        // 3) No query: recency listing (optionally URL-filtered above).
        let sql = format!(
            "SELECT {cols} FROM pages p WHERE {filter_sql}
             ORDER BY p.fetched_at DESC LIMIT {limit}"
        );
        self.query_page_hits(&sql, &filter_args, "")
    }

    fn query_page_hits(
        &self,
        sql: &str,
        args: &[rusqlite::types::Value],
        raw_query: &str,
    ) -> Result<Vec<PageHit>, String> {
        let mut stmt = self.conn.prepare(sql).map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params_from_iter(args.iter()), |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, String>(6)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        let mut hits = Vec::new();
        for row in rows {
            let (url, title, content, tier, truncated, fetched_at, content_hash) =
                row.map_err(|e| e.to_string())?;
            hits.push(PageHit {
                url,
                title,
                snippet: snippet(&content, raw_query),
                tier,
                truncated: truncated != 0,
                fetched_at,
                content_hash,
            });
        }
        Ok(hits)
    }

    fn query_searches(&self, owner: &str, q: &CacheQuery) -> Result<Vec<SearchHit>, String> {
        let mut sql = String::from(
            "SELECT query, categories, n_results, results_json, searched_at FROM searches WHERE owner=?1",
        );
        let mut args: Vec<rusqlite::types::Value> = vec![owner.to_string().into()];
        if let Some(raw) = q.query.as_deref().filter(|s| !s.trim().is_empty()) {
            sql.push_str(&format!(
                " AND query LIKE '%'||?{}||'%' ESCAPE '\\'",
                args.len() + 1
            ));
            args.push(like_escape(raw).into());
        }
        if let Some(u) = &q.url {
            sql.push_str(&format!(
                " AND results_json LIKE '%'||?{}||'%' ESCAPE '\\'",
                args.len() + 1
            ));
            args.push(like_escape(u).into());
        }
        if let Some(h) = q.since_hours {
            sql.push_str(&format!(" AND searched_at >= ?{}", args.len() + 1));
            args.push((now() - h as i64 * 3600).into());
        }
        sql.push_str(&format!(
            " ORDER BY searched_at DESC LIMIT {}",
            q.limit.clamp(1, 100)
        ));
        let mut stmt = self.conn.prepare(&sql).map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params_from_iter(args.iter()), |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        let mut hits = Vec::new();
        for row in rows {
            let (query, categories, n, json, at) = row.map_err(|e| e.to_string())?;
            let top = serde_json::from_str::<serde_json::Value>(&json)
                .ok()
                .and_then(|v| v.as_array().cloned())
                .unwrap_or_default()
                .iter()
                .take(3)
                .map(|item| {
                    let title = item
                        .get("title")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string();
                    let url = item
                        .get("url")
                        .and_then(|u| u.as_str())
                        .unwrap_or("")
                        .to_string();
                    TopResult { title, url }
                })
                .collect();
            hits.push(SearchHit {
                query,
                categories,
                n_results: n,
                searched_at: at,
                top,
            });
        }
        Ok(hits)
    }

    fn get_page(&self, owner: &str, url: &str) -> Result<Option<PageFull>, String> {
        self.conn
            .query_row(
                "SELECT url, title, content, tier, truncated, fetched_at,
                        content_hash, prev_hash, prev_fetched_at FROM pages
                 WHERE owner=?1 AND norm_url=?2",
                params![owner, normalize_url(url)],
                |r| {
                    let content_hash: String = r.get(6)?;
                    let prev_hash: String = r.get(7)?;
                    Ok(PageFull {
                        url: r.get(0)?,
                        title: r.get(1)?,
                        content: r.get(2)?,
                        tier: r.get(3)?,
                        truncated: r.get::<_, i64>(4)? != 0,
                        fetched_at: r.get(5)?,
                        changed_since_prev: !prev_hash.is_empty() && prev_hash != content_hash,
                        content_hash,
                        prev_hash,
                        prev_fetched_at: r.get(8)?,
                    })
                },
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                e => Err(e.to_string()),
            })
    }

    fn clear(&self, owner: &str, url: Option<&str>, since_hours: Option<u64>, all: bool) -> Result<(usize, usize), String> {
        if !all && url.is_none() && since_hours.is_none() {
            return Err("refusing to clear without a filter: pass url, since_hours, or all=true".into());
        }
        let mut where_pages = String::from("owner=?1");
        let mut where_searches = String::from("owner=?1");
        let mut args: Vec<rusqlite::types::Value> = vec![owner.to_string().into()];
        if let Some(u) = url {
            let ph = format!("?{}", args.len() + 1);
            where_pages.push_str(&format!(
                " AND norm_url LIKE '%'||{ph}||'%' ESCAPE '\\'"
            ));
            where_searches.push_str(&format!(
                " AND results_json LIKE '%'||{ph}||'%' ESCAPE '\\'"
            ));
            args.push(like_escape(u).into());
        }
        if let Some(h) = since_hours {
            let ph = format!("?{}", args.len() + 1);
            where_pages.push_str(&format!(" AND fetched_at >= {ph}"));
            where_searches.push_str(&format!(" AND searched_at >= {ph}"));
            args.push((now() - h as i64 * 3600).into());
        }
        self.conn
            .execute(
                &format!(
                    "DELETE FROM pages_fts WHERE rowid IN (SELECT id FROM pages WHERE {where_pages})"
                ),
                params_from_iter(args.iter()),
            )
            .map_err(|e| e.to_string())?;
        let np = self
            .conn
            .execute(&format!("DELETE FROM pages WHERE {where_pages}"), params_from_iter(args.iter()))
            .map_err(|e| e.to_string())?;
        let ns = self
            .conn
            .execute(
                &format!("DELETE FROM searches WHERE {where_searches}"),
                params_from_iter(args.iter()),
            )
            .map_err(|e| e.to_string())?;
        Ok((np, ns))
    }

    fn stats(&self, owner: &str) -> Result<Stats, String> {
        let (pages, searches, oldest) = self
            .conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM pages WHERE owner=?1),
                        (SELECT COUNT(*) FROM searches WHERE owner=?1),
                        (SELECT MIN(fetched_at) FROM pages WHERE owner=?1)",
                params![owner],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, Option<i64>>(2)?,
                    ))
                },
            )
            .map_err(|e| e.to_string())?;
        let db_bytes = std::fs::metadata(db_path()).map(|m| m.len()).unwrap_or(0);
        Ok(Stats {
            pages,
            searches,
            oldest_fetch: oldest,
            db_bytes,
        })
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
        Mutex::new(
            if enabled() {
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
            },
        )
    });
    let guard = cell.lock().map_err(|_| "store mutex poisoned")?;
    match guard.as_ref() {
        Some(s) => f(s),
        None => Err("local store disabled".into()),
    }
}

/// Record a successful page fetch. Best-effort: failures are logged, never
/// propagated — caching must not break the fetch that produced the data.
pub fn record_fetch(owner: &str, resp: &crate::FetchResponse) {
    let _ = with_store(|st| {
        st.record_fetch(
            &norm_owner(owner),
            &resp.url,
            resp.title.as_deref().unwrap_or(""),
            &resp.content,
            resp.tier.unwrap_or(""),
            resp.truncated,
        );
        st.purge_expired();
        Ok(())
    });
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

#[derive(Debug, Default, Serialize)]
pub struct CacheQuery {
    pub query: Option<String>,
    pub url: Option<String>,
    /// "auto" (default), "pages", or "searches"
    pub kind: String,
    pub since_hours: Option<u64>,
    pub limit: usize,
}

#[derive(Debug, Serialize)]
pub struct TopResult {
    pub title: String,
    pub url: String,
}

#[derive(Debug, Serialize)]
pub struct PageHit {
    pub url: String,
    pub title: String,
    pub snippet: String,
    pub tier: String,
    pub truncated: bool,
    pub fetched_at: i64,
    /// SHA-256 of the cached content — diff consecutive samples of the same
    /// URL to catch a source that serves frozen bodies while claiming 200.
    pub content_hash: String,
}

#[derive(Debug, Serialize)]
pub struct SearchHit {
    pub query: String,
    pub categories: String,
    pub n_results: i64,
    pub searched_at: i64,
    /// First 3 results of the cached set (titles + URLs) for quick triage.
    pub top: Vec<TopResult>,
}

#[derive(Debug, Default, Serialize)]
pub struct QueryResult {
    pub pages: Vec<PageHit>,
    pub searches: Vec<SearchHit>,
    pub total_pages: i64,
    pub total_searches: i64,
}

#[derive(Debug, Serialize)]
pub struct PageFull {
    pub url: String,
    pub title: String,
    pub content: String,
    pub tier: String,
    pub truncated: bool,
    pub fetched_at: i64,
    /// True when this fetch's content differs from the previous sample of the
    /// same URL. Frozen-body lies (rate-limited origins serving stale 200s)
    /// show up as false across consecutive samples.
    pub changed_since_prev: bool,
    pub content_hash: String,
    pub prev_hash: String,
    pub prev_fetched_at: i64,
}

#[derive(Debug, Serialize)]
pub struct Stats {
    pub pages: i64,
    pub searches: i64,
    pub oldest_fetch: Option<i64>,
    pub db_bytes: u64,
}

pub fn query(owner: &str, q: &CacheQuery) -> Result<QueryResult, String> {
    with_store(|st| st.query(&norm_owner(owner), q))
}

pub fn get_page(owner: &str, url: &str) -> Result<Option<PageFull>, String> {
    with_store(|st| st.get_page(&norm_owner(owner), url))
}

pub fn clear(
    owner: &str,
    url: Option<&str>,
    since_hours: Option<u64>,
    all: bool,
) -> Result<(usize, usize), String> {
    with_store(|st| st.clear(&norm_owner(owner), url, since_hours, all))
}

pub fn stats(owner: &str) -> Result<Stats, String> {
    with_store(|st| st.stats(&norm_owner(owner)))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> Store {
        let path = std::env::temp_dir().join(format!("agx-store-test-{}.db", uuid::Uuid::new_v4()));
        Store::open(&path).expect("open test store")
    }

    fn page(store: &Store, owner: &str, url: &str, title: &str, content: &str) {
        store.record_fetch(owner, url, title, content, "http", false);
    }

    #[test]
    fn fetch_roundtrip_and_fts_match() {
        let s = test_store();
        page(&s, "a", "https://docs.rs/rusqlite/latest", "rusqlite docs", "Rust bindings for SQLite. Use prepare and query_map.");
        let hits = s
            .query_pages("a", &CacheQuery { query: Some("rust".into()), ..Default::default() })
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "https://docs.rs/rusqlite/latest");
        assert!(hits[0].snippet.contains("Rust"));
    }

    #[test]
    fn page_hits_carry_content_hash() {
        let s = test_store();
        let body = "Rust bindings for SQLite. Use prepare and query_map.";
        page(&s, "a", "https://docs.rs/rusqlite/latest", "rusqlite docs", body);
        let hits = s
            .query_pages("a", &CacheQuery { ..Default::default() })
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].content_hash, hex(&Sha256::digest(body.as_bytes())));
    }

    #[test]
    fn consecutive_samples_expose_drift() {
        let s = test_store();
        page(&s, "a", "https://example.cn/feed", "feed", "version one");
        let first = s.get_page("a", "https://example.cn/feed").unwrap().unwrap();
        assert!(!first.changed_since_prev);
        assert!(first.prev_hash.is_empty());

        page(&s, "a", "https://example.cn/feed", "feed", "version two");
        let second = s.get_page("a", "https://example.cn/feed").unwrap().unwrap();
        assert!(second.changed_since_prev);
        assert_eq!(second.prev_hash, first.content_hash);
        assert_eq!(second.prev_fetched_at, first.fetched_at);

        // The frozen-body lie: same bytes again in a row reads as unchanged.
        page(&s, "a", "https://example.cn/feed", "feed", "version two");
        let third = s.get_page("a", "https://example.cn/feed").unwrap().unwrap();
        assert!(!third.changed_since_prev);
        assert_eq!(third.content_hash, second.content_hash);
    }

    #[test]
    fn reopen_migrates_pre_drift_schema() {
        let path = std::env::temp_dir().join(format!("agx-store-mig-{}.db", uuid::Uuid::new_v4()));
        {
            let s = Store::open(&path).unwrap();
            page(&s, "a", "https://example.cn/x", "t", "body");
            // Roll the schema back to the pre-drift shape so the reopen below
            // exercises the ALTER TABLE migration branch for real.
            s.conn
                .execute_batch("ALTER TABLE pages DROP COLUMN prev_hash;
                                ALTER TABLE pages DROP COLUMN prev_fetched_at;")
                .unwrap();
        }
        let s = Store::open(&path).unwrap();
        let p = s.get_page("a", "https://example.cn/x").unwrap().unwrap();
        assert!(!p.changed_since_prev);
        assert_eq!(p.prev_fetched_at, 0);
        page(&s, "a", "https://example.cn/x", "t", "body v2");
        let p = s.get_page("a", "https://example.cn/x").unwrap().unwrap();
        assert!(p.changed_since_prev);
    }

    #[test]
    fn cjk_substring_matches_via_split_phrase() {
        let s = test_store();
        page(&s, "a", "https://example.cn/a", "浏览器内核分析", "这个浏览器引擎渲染很快。");
        // Two-char substring: would fail under plain unicode61 indexing.
        let hits = s
            .query_pages("a", &CacheQuery { query: Some("浏览器".into()), ..Default::default() })
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].snippet.contains("浏览器"));
    }

    #[test]
    fn owner_isolation() {
        let s = test_store();
        page(&s, "alice", "https://x.test/1", "t", "secret project notes");
        page(&s, "bob", "https://x.test/2", "t", "other notes");
        let hits = s
            .query_pages("alice", &CacheQuery { query: Some("notes".into()), ..Default::default() })
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].url.ends_with("/1"));
    }

    #[test]
    fn same_url_upserts_instead_of_duplicating() {
        let s = test_store();
        page(&s, "a", "https://x.test/p", "old title", "old content");
        page(&s, "a", "https://x.test/p#frag", "new title", "new content");
        let n: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM pages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        let full = s.get_page("a", "https://x.test/p").unwrap().unwrap();
        assert_eq!(full.title, "new title");
    }

    #[test]
    fn purge_drops_expired_rows_and_fts() {
        let s = test_store();
        let ts = now();
        s.conn
            .execute(
                "INSERT INTO pages (owner,url,norm_url,title,content,tier,truncated,content_hash,fetched_at,expires_at)
                 VALUES ('a','https://x.test/old','https://x.test/old','t','body','','0','',?1,?2)",
                params![ts - 100, ts - 50],
            )
            .unwrap();
        let id: i64 = s.conn.query_row("SELECT id FROM pages", [], |r| r.get(0)).unwrap();
        s.conn
            .execute(
                "INSERT INTO pages_fts (rowid, title, content, url) VALUES (?1,'t','body','u')",
                params![id],
            )
            .unwrap();
        LAST_PURGE.store(0, Ordering::Relaxed);
        s.purge_expired();
        let n: i64 = s.conn.query_row("SELECT COUNT(*) FROM pages", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
        let f: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM pages_fts WHERE pages_fts MATCH 'body'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(f, 0);
    }

    #[test]
    fn search_roundtrip_with_top_results() {
        let s = test_store();
        let results = serde_json::json!([
            {"title": "Rust blog", "url": "https://blog.rust-lang.org"},
            {"title": "Other", "url": "https://other.test"}
        ])
        .to_string();
        s.record_search("a", "rust async runtime", "general", &results, 2);
        let hits = s
            .query_searches("a", &CacheQuery { query: Some("async".into()), ..Default::default() })
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].n_results, 2);
        assert_eq!(hits[0].top[0].url, "https://blog.rust-lang.org");
    }

    #[test]
    fn clear_requires_a_filter() {
        let s = test_store();
        assert!(s.clear("a", None, None, false).is_err());
    }

    #[test]
    fn clear_by_url_substring() {
        let s = test_store();
        page(&s, "a", "https://x.test/keep", "t", "c");
        page(&s, "a", "https://y.test/drop", "t", "c");
        let (np, _) = s.clear("a", Some("y.test"), None, false).unwrap();
        assert_eq!(np, 1);
        assert!(s.get_page("a", "https://x.test/keep").unwrap().is_some());
    }

    #[test]
    fn get_page_full_content() {
        let s = test_store();
        page(&s, "a", "https://x.test/full", "The Title", "full body text");
        let full = s.get_page("a", "https://x.test/full").unwrap().unwrap();
        assert_eq!(full.content, "full body text");
        assert_eq!(full.tier, "http");
        assert!(s.get_page("b", "https://x.test/full").unwrap().is_none());
    }

    #[test]
    fn hostile_query_syntax_does_not_error() {
        let s = test_store();
        page(&s, "a", "https://x.test/1", "t", "harmless body");
        for q in ["\"(weird)*", "a OR b AND NOT (", "NEAR(", "--", "'"] {
            let hits = s
                .query_pages("a", &CacheQuery { query: Some(q.into()), ..Default::default() })
                .unwrap();
            let _ = hits; // must not Err
        }
    }

    #[test]
    fn normalize_url_strips_fragment_default_port_and_case() {
        assert_eq!(
            normalize_url("HTTPS://Example.COM:443/Path?q=1#frag"),
            "https://example.com/Path?q=1"
        );
        assert_eq!(
            normalize_url("http://example.com:8080/a"),
            "http://example.com:8080/a"
        );
    }

    #[test]
    fn snippet_bounds_and_ellipses() {
        let long = format!("{}needle{}", "x".repeat(500), "y".repeat(500));
        let snip = snippet(&long, "needle");
        assert!(snip.starts_with('…') && snip.ends_with('…'));
        assert!(snip.contains("needle"));
        assert!(snippet("short text", "missing").starts_with("short"));
    }

    #[test]
    fn split_cjk_produces_per_char_tokens() {
        assert_eq!(split_cjk("浏览器go"), "浏 览 器 go");
        assert_eq!(split_cjk("plain"), "plain");
    }
}
