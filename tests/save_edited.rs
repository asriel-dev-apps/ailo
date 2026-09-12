//! `run::save_edited` の門を、実際にファイルへ書いて確かめる。
//!
//! **テストは 1 本にまとめてある。** `XDG_CONFIG_HOME` はプロセス全体の値なので、
//! 同じテストバイナリの中で複数の `#[test]` が並列に走ると互いの置き場所を奪う。
//! 順に確かめるものは 1 本の中に並べる。

use std::collections::BTreeMap;

use ailo::config::{Requests, SavedRequest};

fn req(url: &str) -> SavedRequest {
    SavedRequest {
        method: "GET".into(),
        url: url.into(),
        ..SavedRequest::default()
    }
}

#[test]
fn the_gate_around_save_edited() {
    let tmp = std::env::temp_dir().join(format!("ailo-save-edited-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    // SAFETY: このバイナリは #[test] が 1 本しかない。他のスレッドは読まない。
    unsafe {
        std::env::set_var("XDG_CONFIG_HOME", &tmp);
        std::env::set_var("AILO_NO_KEYCHAIN", "1");
    }

    // 1. 新規（`None`）は登録できる。
    ailo::run::save_edited("alpha", None, req("http://x/alpha")).unwrap();
    assert_eq!(Requests::load().unwrap().names(), ["alpha"]);

    // 2. **同じ名前をもう一度 `None` で保存すると拒む。** 新規の入口が既存を
    //    黙って踏み潰さないこと。ここが緩むと、作ったつもりで消すことになる。
    let err = ailo::run::save_edited("alpha", None, req("http://x/other")).unwrap_err();
    assert!(format!("{err}").contains("もうあります"), "{err}");
    assert_eq!(
        Requests::load().unwrap().get("alpha").unwrap().url,
        "http://x/alpha",
        "拒んだのに中身が変わっている"
    );

    // 3. 既存（`Some`）は、開いたときの姿と一致していれば上書きできる。
    let base = Requests::load().unwrap().get("alpha").unwrap().clone();
    ailo::run::save_edited("alpha", Some(&base), req("http://x/alpha2")).unwrap();
    assert_eq!(
        Requests::load().unwrap().get("alpha").unwrap().url,
        "http://x/alpha2"
    );

    // 4. 開いている間に書き換えられていたら上書きしない。
    let err = ailo::run::save_edited("alpha", Some(&base), req("http://x/alpha3")).unwrap_err();
    assert!(format!("{err}").contains("書き換えられました"), "{err}");

    // 5. 秘匿値の直書きは、新規の入口でも拒む。
    let mut leaky = req("http://x/leak");
    leaky.items = vec!["Authorization: Bearer sk-live-0123456789abcdef".into()];
    let err = ailo::run::save_edited("leaky", None, leaky).unwrap_err();
    assert!(format!("{err}").contains("秘匿値"), "{err}");
    assert!(Requests::load().unwrap().get("leaky").is_none());

    // 6. `secret` に対応する `[capture]` が無ければ拒む。
    let mut dangling = req("http://x/c");
    dangling.secret = vec!["token".into()];
    dangling.capture = BTreeMap::new();
    assert!(ailo::run::save_edited("dangling", None, dangling).is_err());

    // 7. **読めない `requests.toml` を「まだ何も無い」と扱わない。**
    //    扱うと、次の新規保存が既存の定義を全部消して 1 件で置き換える。
    std::fs::write(tmp.join("ailo/requests.toml"), [0xff, 0xfe, 0x00]).unwrap();
    let err = ailo::run::save_edited("beta", None, req("http://x/beta")).unwrap_err();
    assert!(format!("{err}").contains("読めません"), "{err}");
    assert_eq!(
        std::fs::read(tmp.join("ailo/requests.toml")).unwrap(),
        [0xff, 0xfe, 0x00],
        "読めないファイルを置き換えている"
    );

    std::fs::remove_dir_all(&tmp).unwrap();
}
