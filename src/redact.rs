//! ダンプとログから秘匿値を落とす。
//!
//! ここが緩いと、秘匿値をキーチェーンに仕舞う努力が自分のダンプで台無しになる。
//! ダンプは「捕まえた token と Set-Cookie の平文アーカイブ」になりやすい。

/// マスク後に出る文字列。長さも漏らさないため固定。
pub const MASK: &str = "***";

/// 既定でマスクするヘッダ名(小文字)。
const DEFAULT_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
    "api-key",
    "x-auth-token",
    "x-amz-security-token",
    "x-csrf-token",
];

/// 秘匿値が入っていそうなフィールド名の断片(小文字、部分一致)。
///
/// ヘッダ名だけを見ていると、ログインの `password=...` や `token=...` のように
/// **ボディに直書きされた秘匿値**を丸ごと取りこぼす。名前で当たりを付けるのは
/// 完全ではないが、実際に漏れるのはほぼこの語彙。取りこぼしより過剰マスクを選ぶ。
/// 誤って隠れた場合は `--no-redact` で外せる。
const SENSITIVE_FIELD_PARTS: &[&str] = &[
    "password",
    "passwd",
    "pwd",
    "token",
    "secret",
    "apikey",
    "credential",
    "privatekey",
    "session",
    // ヘッダ名がそのままフィールド名として現れる形。反響する API で頻出する。
    "authorization",
    "auth",
    "bearer",
    "jwt",
    "signature",
    "otp",
    "passphrase",
];

/// フィールド名が秘匿値を持ちそうか。
///
/// 区切り文字(`-` `_` `.` 空白)を落としてから比べる。`api_key` と `api-key` と `apiKey` は
/// 同じものなのに、綴りごとに列挙すると必ず取りこぼす。
pub fn is_sensitive_field(name: &str) -> bool {
    let normalized: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    SENSITIVE_FIELD_PARTS.iter().any(|p| normalized.contains(p))
}

/// リテラル置換の下限長(文字数)。これより短い値は置換しない。
///
/// 下限がある理由は、`1` や `ok` のような値まで置換対象にするとダンプが読めなくなるから。
/// **登録されるのは秘匿らしい名前の下にあった値だけ**なので、下限は誤爆よけの最低限でよい。
///
/// 以前は 8 だった。それだと 7 文字のパスワードが登録されず、
/// サーバがリクエスト本文を**文字列としてエスケープして返す**形
/// (`{"data":"{\"password\":\"hunter2\"}"}`)で素通りした。
/// キー名によるマスクは文字列の中までは届かないので、ここで捕まえる必要がある。
///
/// 既知の限界: これより短い秘匿値は落とせない。
const MIN_LITERAL_LEN: usize = 4;

/// `%XX` エンコードした変形を作る。
///
/// 秘匿値がクエリ文字列やフォーム本文に載ると、`/` が `%2F` に、`+` が `%2B` になる。
/// 元の綴りしか登録していないと、変形した側が素通りする。
fn percent_encoded(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 大文字小文字を無視して `needle` を `MASK` に置き換える。
///
/// ASCII の範囲でだけ畳む。`to_ascii_lowercase` はバイト長を変えないので、
/// 小文字化した文字列で見つけた位置をそのまま元の文字列に使える。
fn replace_ascii_case_insensitive(haystack: &str, needle: &str) -> String {
    let hay_lower = haystack.to_ascii_lowercase();
    let need_lower = needle.to_ascii_lowercase();
    let mut out = String::with_capacity(haystack.len());
    let mut cursor = 0usize;
    while let Some(found) = hay_lower[cursor..].find(&need_lower) {
        let start = cursor + found;
        out.push_str(&haystack[cursor..start]);
        out.push_str(MASK);
        cursor = start + needle.len();
    }
    out.push_str(&haystack[cursor..]);
    out
}

#[derive(Debug, Clone)]
pub struct Redactor {
    enabled: bool,
    header_names: Vec<String>,
    literals: Vec<String>,
}

impl Redactor {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            header_names: DEFAULT_HEADERS.iter().map(|s| s.to_string()).collect(),
            literals: Vec::new(),
        }
    }

    /// マスクを完全に外した Redactor(`--no-redact`)。
    pub fn disabled() -> Self {
        Self::new(false)
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// 追加のヘッダ名を登録する(設定ファイル由来)。
    pub fn add_header_name(&mut self, name: &str) {
        let lower = name.trim().to_ascii_lowercase();
        if !lower.is_empty() && !self.header_names.contains(&lower) {
            self.header_names.push(lower);
        }
    }

    /// 実際の秘匿値そのものを登録する。
    ///
    /// キーチェーンから展開した値やキャプチャ対象の値を渡す。ヘッダ名では捕まらない
    /// 「ボディに token がそのまま入っている」経路を塞ぐのがここ。
    /// 変形した綴りも一緒に登録する。完全一致しか見ないので、
    /// クエリやフォームに載って `%XX` になった形は別途登録しないと素通りする。
    pub fn add_literal(&mut self, value: &str) {
        self.push_literal(value);
        let encoded = percent_encoded(value);
        if encoded != value {
            self.push_literal(&encoded);
        }
    }

    fn push_literal(&mut self, value: &str) {
        if value.chars().count() >= MIN_LITERAL_LEN && !self.literals.iter().any(|l| l == value) {
            self.literals.push(value.to_string());
        }
    }

    /// 送信するヘッダから秘匿値そのものを学習する。
    ///
    /// ヘッダ名を見るだけでは、**サーバがリクエストヘッダを本文に反響して返す** API
    /// (httpbin など、デバッグ用エンドポイントは珍しくない)で token が素通りする。
    /// 送った値を literal として登録しておけば、それが本文のどこに現れても落ちる。
    ///
    /// `Bearer <token>` のようなスキーム付きの値は、token 部分も個別に登録する。
    /// サーバが token だけを返す形もあるため。
    pub fn learn_from_headers<'a, I>(&mut self, headers: I)
    where
        I: IntoIterator<Item = (&'a str, &'a str)>,
    {
        for (name, value) in headers {
            if !self.is_sensitive_header(name) {
                continue;
            }
            self.add_literal(value);
            if let Some((_scheme, rest)) = value.split_once(' ') {
                self.add_literal(rest.trim());
            }
        }
    }

    /// 送信するボディフィールドから秘匿値を学習する。
    ///
    /// `password=...` をそのまま送ると、ダンプの request 側に平文で残る。
    /// サーバが受け取った値を反響する API なら response 側にも残る。
    pub fn learn_from_fields<'a, I>(&mut self, fields: I)
    where
        I: IntoIterator<Item = (&'a str, String)>,
    {
        for (name, value) in fields {
            if is_sensitive_field(name) {
                self.add_literal(&value);
            }
        }
    }

    /// URL から秘匿値を学習する。
    ///
    /// `?api_key=<値>` は変数でもヘッダでもボディでもないので、他の学習経路に引っかからない。
    /// URL 自体は [`Redactor::url`] が名前で落とせるが、**サーバがその値を本文に反響して
    /// 返す**場合(`httpbin` の `args`、多くのデバッグ用エンドポイント)は落ちない。
    /// 値そのものを覚えておく必要がある。
    pub fn learn_from_url(&mut self, url: &reqwest::Url) {
        if let Some(password) = url.password() {
            self.add_literal(password);
        }
        let pairs: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(k, _)| is_sensitive_field(k))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        for (_, value) in pairs {
            self.add_literal(&value);
        }
    }

    /// JSON の本文から、秘匿らしいキーの値を学習する。
    ///
    /// `--raw` で渡された本文はフィールドに分解されないので、`learn_from_fields` では
    /// 拾えない。ここを見ていなかったとき、`--raw '{"password":"..."}'` の値が
    /// ダンプに平文で残った。
    pub fn learn_from_json(&mut self, v: &serde_json::Value) {
        use serde_json::Value;
        match v {
            Value::Object(o) => {
                for (k, val) in o {
                    if is_sensitive_field(k) {
                        if let Value::String(s) = val {
                            self.add_literal(s);
                        }
                    }
                    self.learn_from_json(val);
                }
            }
            Value::Array(a) => {
                for x in a {
                    self.learn_from_json(x);
                }
            }
            _ => {}
        }
    }

    pub fn is_sensitive_header(&self, name: &str) -> bool {
        let lower = name.to_ascii_lowercase();
        self.header_names.iter().any(|h| h == &lower)
    }

    /// ヘッダ 1 本の値を、必要ならマスクして返す。
    pub fn header_value(&self, name: &str, value: &str) -> String {
        if self.enabled && self.is_sensitive_header(name) {
            MASK.to_string()
        } else {
            self.text(value)
        }
    }

    /// URL を記録・表示できる形にする。
    ///
    /// literal 置換だけでは足りない。`?api_key=<生の値>` のように**その場で打ち込まれた**
    /// 秘匿値は変数でもヘッダでもないので、redactor は値を知らない。名前で判断して落とす。
    /// URL はダンプの索引に残り、`ailo log` がそのまま表示するため、ここが緩いと
    /// 索引が認証情報の一覧になる。
    ///
    /// `userinfo`(`https://user:pass@host/`)のパスワードも落とす。
    /// パス自体に埋め込まれた秘匿値は名前が無いので判別できない。これは残る。
    pub fn url(&self, raw: &str) -> String {
        let scrubbed = self.text(raw);
        if !self.enabled {
            return scrubbed;
        }
        let Ok(mut parsed) = reqwest::Url::parse(&scrubbed) else {
            return scrubbed;
        };
        if parsed.password().is_some() {
            let _ = parsed.set_password(Some(MASK));
        }
        if let Some(query) = parsed.query() {
            if !query.is_empty() {
                let rewritten: Vec<(String, String)> = parsed
                    .query_pairs()
                    .map(|(k, v)| {
                        let value = if is_sensitive_field(&k) {
                            MASK.to_string()
                        } else {
                            v.to_string()
                        };
                        (k.to_string(), value)
                    })
                    .collect();
                parsed.query_pairs_mut().clear().extend_pairs(rewritten);
            }
        }
        parsed.to_string()
    }

    /// 任意のテキストから、登録済みの秘匿値そのものを落とす。
    pub fn text(&self, s: &str) -> String {
        if !self.enabled || self.literals.is_empty() {
            return s.to_string();
        }
        let mut out = s.to_string();
        for lit in &self.literals {
            // 大文字小文字を無視する。サーバが token を正規化して返すことがある。
            out = replace_ascii_case_insensitive(&out, lit);
        }
        out
    }

    /// JSON を再帰的に走査してマスクする。
    ///
    /// 2 通りで落とす。**値そのもの**(登録済みの literal)と、**キーの名前**。
    /// 名前で落とす側が要るのは、8 文字未満の秘匿値が literal として登録されないため。
    /// `{"password":"abc123"}` は literal では捕まらないが、キー名では捕まる。
    pub fn json(&self, v: &serde_json::Value) -> serde_json::Value {
        if !self.enabled {
            return v.clone();
        }
        self.json_inner(v, false)
    }

    /// `under_sensitive_key` は、この値が秘匿らしい名前のキーの下にいるかどうか。
    fn json_inner(&self, v: &serde_json::Value, under_sensitive_key: bool) -> serde_json::Value {
        use serde_json::Value;
        match v {
            Value::String(s) => {
                if under_sensitive_key {
                    Value::String(MASK.to_string())
                } else {
                    Value::String(self.text(s))
                }
            }
            // 数値・真偽値は、秘匿らしいキーの下でも落とさない。`expires_in: 3600` まで
            // 隠すと、キャプチャの設定を書くときに何も見えなくなる。
            Value::Array(a) => Value::Array(
                a.iter()
                    .map(|x| self.json_inner(x, under_sensitive_key))
                    .collect(),
            ),
            Value::Object(o) => Value::Object(
                o.iter()
                    .map(|(k, x)| (k.clone(), self.json_inner(x, is_sensitive_field(k))))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_default_sensitive_headers() {
        let r = Redactor::new(true);
        assert_eq!(r.header_value("Authorization", "Bearer abc123xyz"), MASK);
        assert_eq!(r.header_value("Set-Cookie", "sid=deadbeef"), MASK);
        // 大文字小文字は問わない。
        assert_eq!(r.header_value("AUTHORIZATION", "x"), MASK);
    }

    #[test]
    fn leaves_ordinary_headers_alone() {
        let r = Redactor::new(true);
        assert_eq!(
            r.header_value("Content-Type", "application/json"),
            "application/json"
        );
    }

    #[test]
    fn no_redact_passes_everything_through() {
        let r = Redactor::disabled();
        assert_eq!(
            r.header_value("Authorization", "Bearer abc123xyz"),
            "Bearer abc123xyz"
        );
    }

    #[test]
    fn literal_values_are_scrubbed_from_body_text() {
        let mut r = Redactor::new(true);
        r.add_literal("s3cr3t-token-value");
        let body = r#"{"token":"s3cr3t-token-value","user":"taro"}"#;
        let out = r.text(body);
        assert!(!out.contains("s3cr3t-token-value"));
        assert!(out.contains("taro"));
    }

    #[test]
    fn short_literals_are_not_registered() {
        // "ok" を置換対象にすると、無関係な語まで潰れてダンプが読めなくなる。
        let mut r = Redactor::new(true);
        r.add_literal("ok");
        assert_eq!(r.text("status is ok"), "status is ok");
    }

    #[test]
    fn literals_are_scrubbed_inside_nested_json() {
        let mut r = Redactor::new(true);
        r.add_literal("s3cr3t-token-value");
        let v: serde_json::Value =
            serde_json::from_str(r#"{"a":{"b":["s3cr3t-token-value","keep"]}}"#).unwrap();
        let out = r.json(&v).to_string();
        assert!(!out.contains("s3cr3t-token-value"));
        assert!(out.contains("keep"));
    }

    #[test]
    fn secrets_echoed_back_in_the_response_body_are_scrubbed() {
        // httpbin のようにリクエストヘッダを本文へ反響する API が実在する。
        // ヘッダ名を見るだけのマスクはここで素通りする。
        let mut r = Redactor::new(true);
        r.learn_from_headers([("Authorization", "Bearer s3cr3t-token-value")]);
        let echoed = r#"{"headers":{"Authorization":"Bearer s3cr3t-token-value"}}"#;
        let out = r.text(echoed);
        assert!(!out.contains("s3cr3t-token-value"), "{out}");
    }

    #[test]
    fn the_token_part_alone_is_also_scrubbed() {
        // サーバが scheme を落として token だけ返す形もある。
        let mut r = Redactor::new(true);
        r.learn_from_headers([("Authorization", "Bearer s3cr3t-token-value")]);
        assert!(!r.text(r#"{"t":"s3cr3t-token-value"}"#).contains("s3cr3t"));
    }

    #[test]
    fn ordinary_headers_are_not_learned_as_literals() {
        // Content-Type の値まで literal にすると、本文中の同じ語まで潰れる。
        let mut r = Redactor::new(true);
        r.learn_from_headers([("Content-Type", "application/json")]);
        assert_eq!(r.text("application/json"), "application/json");
    }

    #[test]
    fn secret_looking_field_names_are_recognised() {
        for name in [
            "password",
            "Password",
            "access_token",
            "clientSecret",
            "api_key",
            "sessionId",
        ] {
            assert!(is_sensitive_field(name), "見逃した: {name}");
        }
        for name in ["name", "email", "age", "limit", "items"] {
            assert!(!is_sensitive_field(name), "過剰に拾った: {name}");
        }
    }

    #[test]
    fn separator_spelling_does_not_change_the_verdict() {
        // 綴りごとに列挙すると必ず取りこぼす。区切りを落としてから比べる。
        for name in ["api_key", "api-key", "apiKey", "API KEY", "x-api-key"] {
            assert!(is_sensitive_field(name), "見逃した: {name}");
        }
    }

    #[test]
    fn header_like_field_names_are_recognised() {
        // 反響する API はヘッダ名をそのままフィールド名にして返してくる。
        for name in ["authorization", "Authorization", "jwt", "signature", "otp"] {
            assert!(is_sensitive_field(name), "見逃した: {name}");
        }
    }

    #[test]
    fn a_password_sent_in_the_body_is_scrubbed_from_the_dump() {
        // ヘッダ名だけを見ていると、ログインの本文に書いた秘匿値を丸ごと取りこぼす。
        let mut r = Redactor::new(true);
        r.learn_from_fields([("password", "hunter2-and-more".to_string())]);
        let out = r.text(r#"{"password":"hunter2-and-more","email":"a@example.com"}"#);
        assert!(!out.contains("hunter2-and-more"), "{out}");
        assert!(out.contains("a@example.com"), "{out}");
    }

    #[test]
    fn ordinary_field_names_are_not_learned() {
        let mut r = Redactor::new(true);
        r.learn_from_fields([("name", "taro-yamada-san".to_string())]);
        assert!(r.text("taro-yamada-san").contains("taro"));
    }

    #[test]
    fn a_short_secret_is_caught_by_its_key_name_even_though_no_literal_is_registered() {
        // 8 文字未満は literal に登録されない。名前側で捕まえないと素通りする。
        let mut r = Redactor::new(true);
        r.learn_from_fields([("password", "abc123".to_string())]);
        let v: serde_json::Value =
            serde_json::from_str(r#"{"password":"abc123","user":"taro"}"#).unwrap();
        let out = r.json(&v);
        assert_eq!(out["password"], MASK);
        assert_eq!(out["user"], "taro");
    }

    #[test]
    fn numbers_under_a_sensitive_key_are_left_readable() {
        // `expires_in: 3600` まで隠すと、キャプチャの設定を書くときに何も見えなくなる。
        let r = Redactor::new(true);
        let v: serde_json::Value = serde_json::from_str(r#"{"token_expires_in":3600}"#).unwrap();
        assert_eq!(r.json(&v)["token_expires_in"], 3600);
    }

    #[test]
    fn a_secret_inside_an_escaped_json_string_is_scrubbed() {
        // サーバがリクエスト本文を文字列として反響する形。キー名によるマスクは
        // 文字列の中まで届かないので、値そのものの登録で捕まえる。
        let mut r = Redactor::new(true);
        r.learn_from_fields([("password", "hunter2".to_string())]);
        let v: serde_json::Value =
            serde_json::from_str(r#"{"data":"{\"password\":\"hunter2\"}"}"#).unwrap();
        let out = r.json(&v).to_string();
        assert!(!out.contains("hunter2"), "{out}");
    }

    #[test]
    fn a_percent_encoded_secret_is_also_scrubbed() {
        // クエリやフォーム本文に載ると `/` は `%2F` になる。元の綴りだけでは足りない。
        let mut r = Redactor::new(true);
        r.add_literal("ab/cd+ef==gh");
        let out = r.text("q=ab%2Fcd%2Bef%3D%3Dgh");
        assert!(!out.contains("ab%2Fcd"), "{out}");
    }

    #[test]
    fn case_differences_do_not_let_a_secret_through() {
        let mut r = Redactor::new(true);
        r.add_literal("AbCdEfGhIj");
        assert!(!r.text("value=abcdefghij").contains("abcdefghij"));
    }

    #[test]
    fn case_insensitive_replacement_keeps_the_surrounding_text_intact() {
        let mut r = Redactor::new(true);
        r.add_literal("SecretValue1");
        assert_eq!(r.text("a-secretvalue1-b"), format!("a-{MASK}-b"));
    }

    #[test]
    fn a_secret_typed_straight_into_the_query_string_is_masked_by_its_name() {
        // 索引に残り `ailo log` がそのまま表示するので、ここが緩いと
        // 索引が認証情報の一覧になる。redactor はこの値を知らないため名前で判断する。
        let r = Redactor::new(true);
        let out = r.url("https://api.example.com/u?api_key=abc123&limit=50");
        assert!(!out.contains("abc123"), "{out}");
        assert!(out.contains("limit=50"), "{out}");
    }

    #[test]
    fn a_password_in_the_url_userinfo_is_masked() {
        let r = Redactor::new(true);
        let out = r.url("https://user:hunter2@api.example.com/u");
        assert!(!out.contains("hunter2"), "{out}");
        assert!(out.contains("user"), "{out}");
    }

    #[test]
    fn an_ordinary_url_is_left_usable() {
        let r = Redactor::new(true);
        assert_eq!(
            r.url("https://api.example.com/users?limit=50"),
            "https://api.example.com/users?limit=50"
        );
        // クエリの無い URL に `?` を足さないこと。
        assert_eq!(
            r.url("https://api.example.com/u"),
            "https://api.example.com/u"
        );
    }

    #[test]
    fn no_redact_leaves_the_url_untouched() {
        let r = Redactor::disabled();
        let raw = "https://api.example.com/u?api_key=abc123";
        assert_eq!(r.url(raw), raw);
    }

    #[test]
    fn added_header_names_are_honoured() {
        let mut r = Redactor::new(true);
        r.add_header_name("X-Internal-Secret");
        assert_eq!(r.header_value("x-internal-secret", "value"), MASK);
    }
}
