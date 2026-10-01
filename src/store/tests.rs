use super::*;

fn test_store() -> Store {
    let path = std::env::temp_dir().join(format!("agx-store-test-{}.db", uuid::Uuid::new_v4()));
    Store::open(&path).expect("open test store")
}

fn page(store: &Store, owner: &str, url: &str, title: &str, content: &str) {
    store.record_fetch(owner, url, title, content, "http", false);
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
            .execute_batch(
                "ALTER TABLE pages DROP COLUMN prev_hash;
                            ALTER TABLE pages DROP COLUMN prev_fetched_at;",
            )
            .unwrap();
    }
    let s = Store::open(&path).unwrap();
    // Migration re-added the drift columns with their defaults.
    let (prev_hash, prev_fetched_at): (String, i64) = s
        .conn
        .query_row(
            "SELECT prev_hash, prev_fetched_at FROM pages",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(prev_hash, "");
    assert_eq!(prev_fetched_at, 0);
    // The drift upsert still works on the migrated row: the second sample
    // carries the first sample's content hash.
    page(&s, "a", "https://example.cn/x", "t", "body v2");
    let (prev_hash, content_hash): (String, String) = s
        .conn
        .query_row(
            "SELECT prev_hash, content_hash FROM pages",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(prev_hash, hex(&Sha256::digest(b"body")));
    assert_eq!(content_hash, hex(&Sha256::digest(b"body v2")));
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
    let title: String = s
        .conn
        .query_row("SELECT title FROM pages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(title, "new title");
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
    let id: i64 = s
        .conn
        .query_row("SELECT id FROM pages", [], |r| r.get(0))
        .unwrap();
    s.conn
        .execute(
            "INSERT INTO pages_fts (rowid, title, content, url) VALUES (?1,'t','body','u')",
            params![id],
        )
        .unwrap();
    LAST_PURGE.store(0, Ordering::Relaxed);
    s.purge_expired();
    let n: i64 = s
        .conn
        .query_row("SELECT COUNT(*) FROM pages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0);
    let f: i64 = s
        .conn
        .query_row(
            "SELECT COUNT(*) FROM pages_fts WHERE pages_fts MATCH 'body'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(f, 0);
}

#[test]
fn session_snapshots_roundtrip_and_purge() {
    let s = test_store();
    assert!(s.load_session_snapshot("s_1").unwrap().is_none());
    s.save_session_snapshot("s_1", "{\"url\":\"https://x.test\"}")
        .unwrap();
    let (snap, at) = s.load_session_snapshot("s_1").unwrap().unwrap();
    assert_eq!(snap, "{\"url\":\"https://x.test\"}");
    assert!(at > 0);
    // Upsert overwrites; ids are independent.
    s.save_session_snapshot("s_1", "{\"url\":\"https://y.test\"}")
        .unwrap();
    s.save_session_snapshot("s_2", "{\"url\":\"https://z.test\"}")
        .unwrap();
    assert_eq!(
        s.load_session_snapshot("s_1").unwrap().unwrap().0,
        "{\"url\":\"https://y.test\"}"
    );
    assert!(s.delete_session_snapshot("s_1").unwrap());
    assert!(!s.delete_session_snapshot("s_1").unwrap());
    assert!(s.load_session_snapshot("s_2").unwrap().is_some());
    // Age-based purge only touches rows past the cutoff.
    s.conn
        .execute(
            "UPDATE session_snapshots SET updated_at=?1 WHERE id='s_2'",
            params![now() - 100_000],
        )
        .unwrap();
    s.purge_session_snapshots(50_000).unwrap();
    assert!(s.load_session_snapshot("s_2").unwrap().is_none());
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
fn accounts_roundtrip_owner_isolation_and_listing() {
    let s = test_store();
    let record = r#"{"version":1,"cookies":["cookie2=t; Domain=.taobao.com; Path=/","sg=1; Domain=.taobao.com; Path=/"],"url":"https://www.taobao.com/"}"#;
    s.save_account("owner-a", "scraper", record).unwrap();
    // Upsert overwrites in place, keeping one row per (owner, name).
    let v2 = r#"{"version":1,"cookies":["cookie2=v2; Domain=.taobao.com; Path=/"],"url":"https://www.taobao.com/"}"#;
    s.save_account("owner-a", "scraper", v2).unwrap();
    let rows: i64 = s
        .conn
        .query_row("SELECT COUNT(*) FROM accounts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 1);

    let (loaded, _) = s.load_account("owner-a", "scraper").unwrap().unwrap();
    assert!(loaded.contains("cookie2=v2"));

    // Same name under another owner is a different account.
    assert!(s.load_account("owner-b", "scraper").unwrap().is_none());
    s.save_account("owner-b", "scraper", record).unwrap();
    assert!(s.load_account("owner-b", "scraper").unwrap().is_some());

    // Listing: metadata only — names/domains/counts, never cookie values.
    let rows = s.list_accounts("owner-a").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "scraper");
    assert_eq!(rows[0].cookie_count, 1);
    assert_eq!(rows[0].domains, vec!["taobao.com".to_string()]);
    assert!(rows[0].verify_url.is_none());
    assert!(rows[0].persona_ua.is_none());

    // The persona rides the listing as its UA; the fp_seed stays
    // server-side (callers can't act on it, and the summary is
    // metadata-only by contract).
    let with_persona =
        r#"{"version":1,"persona":{"fp_seed":42,"user_agent":"UA-X"},"cookies":[]}"#;
    s.save_account("owner-a", "publisher", with_persona)
        .unwrap();
    let rows = s.list_accounts("owner-a").unwrap();
    let publisher = rows.iter().find(|r| r.name == "publisher").unwrap();
    assert_eq!(publisher.persona_ua.as_deref(), Some("UA-X"));

    assert!(s.delete_account("owner-a", "scraper").unwrap());
    assert!(!s.delete_account("owner-a", "scraper").unwrap()); // already gone
    assert!(s.load_account("owner-a", "scraper").unwrap().is_none());
    assert!(s.load_account("owner-b", "scraper").unwrap().is_some());
}

#[test]
fn split_cjk_produces_per_char_tokens() {
    assert_eq!(split_cjk("浏览器go"), "浏 览 器 go");
    assert_eq!(split_cjk("plain"), "plain");
}
