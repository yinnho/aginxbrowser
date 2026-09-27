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
    page(
        &s,
        "a",
        "https://docs.rs/rusqlite/latest",
        "rusqlite docs",
        "Rust bindings for SQLite. Use prepare and query_map.",
    );
    let hits = s
        .query_pages(
            "a",
            &CacheQuery {
                query: Some("rust".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].url, "https://docs.rs/rusqlite/latest");
    assert!(hits[0].snippet.contains("Rust"));
}

#[test]
fn page_hits_carry_content_hash() {
    let s = test_store();
    let body = "Rust bindings for SQLite. Use prepare and query_map.";
    page(
        &s,
        "a",
        "https://docs.rs/rusqlite/latest",
        "rusqlite docs",
        body,
    );
    let hits = s
        .query_pages(
            "a",
            &CacheQuery {
                ..Default::default()
            },
        )
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
fn fts_order_fuses_relevance_with_recency() {
    let s = test_store();
    page(&s, "a", "https://example.cn/old", "", "rust rust rust docs");
    page(
        &s,
        "a",
        "https://example.cn/mid",
        "",
        "rust middle tokio docs",
    );
    page(
        &s,
        "a",
        "https://example.cn/new",
        "",
        "rust newer far longer filler content docs",
    );
    // bm25 order: old (3 hits) > mid > new (longest doc). Overwrite fetch
    // times so recency runs the other way: new > mid > old.
    let ts = now();
    for (url, at) in [
        ("https://example.cn/old", ts - 7200),
        ("https://example.cn/mid", ts - 3600),
        ("https://example.cn/new", ts),
    ] {
        s.conn
            .execute(
                "UPDATE pages SET fetched_at=?1 WHERE url=?2",
                params![at, url],
            )
            .unwrap();
    }
    let hits = s
        .query_pages(
            "a",
            &CacheQuery {
                query: Some("rust".into()),
                limit: 10,
                ..Default::default()
            },
        )
        .unwrap();
    let urls: Vec<&str> = hits.iter().map(|h| h.url.as_str()).collect();
    // Leader+trailer tie exactly; the tiebreak hands it to the newer one,
    // and the consistent-middle doc falls last.
    assert_eq!(
        urls,
        vec![
            "https://example.cn/new",
            "https://example.cn/old",
            "https://example.cn/mid"
        ]
    );
}

#[test]
fn snippets_carry_the_section_heading() {
    let s = test_store();
    let filler = "filler line here\n".repeat(8);
    let body = format!("## Deploy runbook\n\n{filler}now use rsync to publish the site\n");
    page(&s, "a", "https://example.cn/guide", "guide", &body);
    // Hit before any heading and heading-free content stay unprefixed.
    page(
        &s,
        "a",
        "https://example.cn/plain",
        "plain",
        "plain text without markdown headings mentions rsync once\n",
    );
    let hits = s
        .query_pages(
            "a",
            &CacheQuery {
                query: Some("rsync".into()),
                limit: 10,
                ..Default::default()
            },
        )
        .unwrap();
    let guide = hits.iter().find(|h| h.url.contains("guide")).unwrap();
    assert!(
        guide.snippet.starts_with("[§ Deploy runbook] "),
        "got: {}",
        guide.snippet
    );
    let plain = hits.iter().find(|h| h.url.contains("plain")).unwrap();
    assert!(!plain.snippet.starts_with("[§ "), "got: {}", plain.snippet);
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
    page(
        &s,
        "a",
        "https://example.cn/a",
        "浏览器内核分析",
        "这个浏览器引擎渲染很快。",
    );
    // Two-char substring: would fail under plain unicode61 indexing.
    let hits = s
        .query_pages(
            "a",
            &CacheQuery {
                query: Some("浏览器".into()),
                ..Default::default()
            },
        )
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
        .query_pages(
            "alice",
            &CacheQuery {
                query: Some("notes".into()),
                ..Default::default()
            },
        )
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
fn search_roundtrip_with_top_results() {
    let s = test_store();
    let results = serde_json::json!([
        {"title": "Rust blog", "url": "https://blog.rust-lang.org"},
        {"title": "Other", "url": "https://other.test"}
    ])
    .to_string();
    s.record_search("a", "rust async runtime", "general", &results, 2);
    let hits = s
        .query_searches(
            "a",
            &CacheQuery {
                query: Some("async".into()),
                ..Default::default()
            },
        )
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
    page(
        &s,
        "a",
        "https://x.test/full",
        "The Title",
        "full body text",
    );
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
            .query_pages(
                "a",
                &CacheQuery {
                    query: Some(q.into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let _ = hits; // must not Err
    }
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
