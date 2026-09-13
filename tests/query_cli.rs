//! `ailo query` の結合テスト。実際に送って履歴を作り、バイナリ越しに問い合わせる。
//!
//! 見るのは 3 つ。**テンプレートがどの問い合わせからも出てこないこと**、
//! **読む以外の操作が失敗すること**、**普通の集計は通ること**。
//! どれも「出てこない」だけでは検査が死んでいても緑になるので、同じ値が DB には
//! 確かに書いてあることをコントロールとして直接読んで示す。

mod support;

use support::{Run, Sandbox, TestServer};

/// URL にテンプレートの印を残して 3 回送った履歴を持つサンドボックス。
fn history_with_templates() -> (TestServer, Sandbox) {
    let server = TestServer::start();
    let sb = Sandbox::new();
    sb.write_config("[vars]\nmarker = \"expanded\"\n");
    let url = format!("{}?m={{{{marker}}}}", server.url("/reflect"));
    for _ in 0..3 {
        sb.run(&["get", &url]).ok();
    }
    (server, sb)
}

fn query(sb: &Sandbox, sql: &str) -> Run {
    sb.run(&["query", sql])
}

#[test]
fn templates_never_come_out_of_query_but_are_really_stored() {
    let (_server, sb) = history_with_templates();

    // コントロール: DB には展開前の姿が書いてある。
    let db = sb.data_dir().join("dumps/history.db");
    let stored: String = rusqlite::Connection::open(&db)
        .unwrap()
        .query_row("select url_template from history limit 1", [], |r| r.get(0))
        .unwrap();
    assert!(stored.contains("{{marker}}"), "{stored}");

    for sql in [
        "select url_template from history limit 1",
        "select * from history limit 1",
        "select group_concat(url_template || items_template) from history",
        "select count(*) from history where url_template like '%marker%'",
    ] {
        let run = query(&sb, sql);
        assert_eq!(run.code, 0, "{sql}: {}", run.stderr);
        assert!(
            !run.stdout.contains("{{marker}}") && !run.stderr.contains("{{marker}}"),
            "{sql} → {}",
            run.stdout
        );
    }
    assert_eq!(
        query(&sb, "select url_template from history limit 1").ok(),
        "[\"url_template\"]\n[null]"
    );
    // 盲目オラクルにならない: like が当たっていれば 3。
    assert_eq!(
        query(
            &sb,
            "select count(*) from history where url_template like '%marker%'"
        )
        .ok(),
        "[\"count(*)\"]\n[0]"
    );
}

#[test]
fn ordinary_aggregation_works() {
    let (_server, sb) = history_with_templates();
    assert_eq!(
        query(&sb, "select count(*) from history").ok(),
        "[\"count(*)\"]\n[3]"
    );
    assert_eq!(
        query(
            &sb,
            "select method, status, count(*) from history group by 1, 2"
        )
        .ok(),
        "[\"method\",\"status\",\"count(*)\"]\n[\"GET\",200,3]"
    );
    assert!(query(
        &sb,
        "select ts, url, ms, bytes from history order by id desc"
    )
    .ok()
    .contains("m=expanded"));
    let schema = sb.run(&["query", "--schema"]).ok().to_string();
    assert!(schema.contains("status") && !schema.contains("template"));
}

#[test]
fn anything_but_a_single_read_fails_without_echoing_the_input() {
    let (_server, sb) = history_with_templates();
    let other = sb.dir("elsewhere").join("other.db");
    rusqlite::Connection::open(&other)
        .unwrap()
        .execute_batch("create table t(x); insert into t values ('other-content');")
        .unwrap();
    let path = other.display().to_string();

    for sql in [
        "select 1; select 2".to_string(),
        format!("attach '{path}' as x; select * from x.t"),
        format!("attach '{path}' as x"),
        "delete from history".to_string(),
        "pragma query_only = 0".to_string(),
        "select * from sqlite_master".to_string(),
        "".to_string(),
    ] {
        let run = query(&sb, &sql);
        assert_eq!(run.code, 2, "{sql} が通った: {}", run.stdout);
        assert!(!run.stdout.contains("other-content"), "{sql}");
        assert!(
            !run.stderr.contains(&path),
            "{sql} がパスを反響した: {}",
            run.stderr
        );
    }
    // DB は無事。
    assert_eq!(
        query(&sb, "select count(*) from history").ok(),
        "[\"count(*)\"]\n[3]"
    );
}

#[test]
fn a_large_result_is_cut_and_saved_to_a_file() {
    let (_server, sb) = history_with_templates();
    // 3^5 = 243 行。標準出力の上限(100 行)を超える。
    let run = query(
        &sb,
        "select a.id from history a, history b, history c, history d, history e",
    );
    assert_eq!(run.code, 4, "{}", run.stderr);
    assert_eq!(run.stdout.lines().count(), 101, "列名 1 行 + 100 行");
    let name = run
        .stderr
        .split("`ailo show ")
        .nth(1)
        .and_then(|s| s.split('`').next())
        .expect("保存先が出ていない");
    let saved: serde_json::Value = serde_json::from_str(sb.run(&["show", name]).ok()).unwrap();
    assert_eq!(saved["rows"].as_array().unwrap().len(), 243);
}

#[test]
fn a_query_that_never_ends_is_stopped() {
    let (_server, sb) = history_with_templates();
    let started = std::time::Instant::now();
    let run = query(
        &sb,
        "with recursive n(x) as (select 1 union all select x + 1 from n) select count(*) from n",
    );
    assert_eq!(run.code, 4, "{}", run.stderr);
    assert!(run.stderr.contains("実行時間の上限"), "{}", run.stderr);
    assert!(started.elapsed() < std::time::Duration::from_secs(15));
}
