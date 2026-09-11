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

/// `%XX` を戻す。**判定に使うためだけ**で、表示には元の綴りを残す。
///
/// 戻さないと `?api%5Fkey=<生の値>` が「秘匿らしい名前」に当たらない。
/// UTF-8 として読めないバイト列はそのまま返す（判定が甘くなるだけで、落とさない）。
fn percent_decoded(value: &str) -> String {
    let b = value.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(byte) = u8::from_str_radix(&value[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| value.to_string())
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

// ---------------------------------------------------------------------------
// 保存済み定義に直接書かれた秘匿値
//
// **「画面から落とす」と「保存を拒む」を 1 つの判定にする。** 別々に書いていた
// ときは、表示だけ落ちて保存は通る（あるいはその逆）穴が、入口を足すたびに開いた。
// TUI に編集器を足したときも、`mask_*`（表示）と `literal_secret_*`（保存の門）が
// ずれていて 3 経路が素通りした。**関数が 2 つある限り、次の入口でまたずれる。**
//
// 落としたものは `Found` で返す。閾値は 1 か所に並べて置く:
//
// - `Found::Assigned` — 名前と値の組として読み取れた。**画面から落とし、保存も拒む。**
// - `Found::Suspected` — 構造としては読めず、秘匿らしい綴りがあるだけ。
//   **画面からは落とすが、保存は拒まない**（`"send me a token"` のような
//   ただの散文まで保存できなくなると、門が通れない側で壊れる）。
// ---------------------------------------------------------------------------

/// 落とした秘匿値の**名前**。値は持たない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    /// 名前と値の組。保存も拒む。
    Assigned(String),
    /// 秘匿らしい綴りがあるだけ。表示だけ落とす。
    Suspected(String),
}

impl Found {
    pub fn name(&self) -> &str {
        match self {
            Found::Assigned(n) | Found::Suspected(n) => n,
        }
    }

    /// 保存を拒む側の閾値。
    pub fn blocks_saving(&self) -> bool {
        matches!(self, Found::Assigned(_))
    }
}

/// 画面に出せる形にした文字列と、落としたものの名前。
#[derive(Debug, Clone)]
pub struct Masked {
    pub text: String,
    pub found: Option<Found>,
}

impl Masked {
    fn clean(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            found: None,
        }
    }
}

/// 秘匿値の綴りらしい語。JSON として読めない本文と、読めない item で使う。
const SECRET_WORDS: [&str; 6] = ["password", "passwd", "token", "secret", "api_key", "apikey"];

/// 値が**テンプレート参照だけ**で出来ているか。参照は名前しか持たないので漏れない。
///
/// **「`{{` を含む」では足りない。** `Bearer {{token}}-literal-secret` は参照を
/// 1 つ含むが、後ろは生の秘匿値そのもの。`{{` を 1 つ混ぜるだけで、表示・
/// 編集器のガード・保存の門・`last.toml` の全部を素通りできていた。
///
/// 参照を取り除いた残りに、認証スキームの語（`Bearer` など）と記号以外の語が
/// 残っていたら、それは生の値。**知らない語は生の値として扱う**（通す側に倒すと
/// 漏れる）。
fn only_template(value: &str) -> bool {
    /// 参照の周りに書かれても値ではない語。
    const DECORATIONS: [&str; 6] = ["bearer", "basic", "digest", "token", "apikey", "api"];

    let mut rest = String::new();
    let mut cur = value;
    while let Some(open) = cur.find("{{") {
        rest.push_str(&cur[..open]);
        // 閉じていない `{{` は参照ではない。残りは生の文字列として見る。
        let Some(close) = cur[open..].find("}}") else {
            rest.push_str(&cur[open..]);
            cur = "";
            break;
        };
        cur = &cur[open + close + 2..];
    }
    rest.push_str(cur);

    rest.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .all(|w| DECORATIONS.contains(&w.to_ascii_lowercase().as_str()))
}

/// item 1 行。**読めなかった行も素通しにしない。**
///
/// `password:=hunter2` のように JSON として壊れた item は `parse_item` が落ちる。
/// 「読めたものだけ検査する」にすると、一番危ない行だけが素通りする。
pub fn mask_item(line: &str, r: &Redactor) -> Masked {
    use crate::args::Item;

    let Ok(item) = crate::args::parse_item(line) else {
        return mask_unparsed_item(line);
    };
    let hit = |name: &str, sep: &str| Masked {
        text: format!("{name}{sep}{MASK}"),
        found: Some(Found::Assigned(name.to_string())),
    };
    match &item {
        Item::Header { name, value } if r.is_sensitive_header(name) && !only_template(value) => {
            hit(name, ": ")
        }
        Item::Field { name, value } if is_sensitive_field(name) && !only_template(value) => {
            hit(name, "=")
        }
        Item::Query { name, value } if is_sensitive_field(name) && !only_template(value) => {
            hit(name, "==")
        }
        Item::RawField { name, value }
            if is_sensitive_field(name) && !only_template(&value.to_string()) =>
        {
            hit(name, ":=")
        }
        _ => Masked::clean(line),
    }
}

/// `parse_item` が読めなかった行。名前らしき先頭だけ残して、後ろを落とす。
///
/// **区切りがあるなら、それは名前と値の組**（`password:=hunter2`）。読めなかった
/// だけで、書いてあるものは秘匿値そのものなので、保存も拒む。
fn mask_unparsed_item(line: &str) -> Masked {
    let head: String = line
        .chars()
        .take_while(|c| !matches!(c, ':' | '=' | '@'))
        .collect();
    // 区切りの後ろが値。行ごと見ると、名前の `password` が「生の語」に数えられる。
    let value = &line[head.len()..];
    let has_separator = !value.is_empty();
    if is_sensitive_field(head.trim()) && has_separator && !only_template(value) {
        return Masked {
            text: format!("{} {MASK}", head.trim()),
            found: Some(Found::Assigned(head.trim().to_string())),
        };
    }
    Masked::clean(line)
}

/// `--raw` の本文。
///
/// **item 記法の判定に通してはいけない。** `{"password":"hunter2"}` は
/// `parse_item` に**成功する**（`{"password"` という名前のヘッダと読まれる）ので、
/// item として検査すると素通りする。ログインの本文は一番秘匿値が入る場所。
pub fn mask_body(raw: &str) -> Masked {
    if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(raw) {
        let found = mask_json_in_place(&mut value);
        // **落とすものが無いなら、書いたとおりに返す。** 常に serde_json で組み直すと、
        // 定義に改行して書いた本文が 1 行に潰れて画面に出る。読むための画面で
        // 読みにくくしていた。落としたときだけ、組み直した形になる。
        if found.is_none() {
            return Masked::clean(raw);
        }
        return Masked {
            text: serde_json::to_string_pretty(&value).unwrap_or_else(|_| MASK.to_string()),
            found,
        };
    }
    // JSON として読めない本文は構造で判断できない。秘匿らしい綴りを探す。
    // **「読めなかったから素通し」にはしない。**
    //
    // **本文のどこかに `{{` があるだけで素通しにもしない。**
    // `password=hunter2&note={{anything}}` が丸ごと安全扱いになっていた。
    // 見るのは「その綴りに割り当てられている値」だけ。
    let lower = raw.to_ascii_lowercase();
    let mut assigned = false;
    let mut hit = None;
    for word in SECRET_WORDS {
        for (i, _) in lower.match_indices(word) {
            let after = lower[i + word.len()..].trim_start();
            hit = hit.or(Some(word));
            // 綴りの直後が `=` か `:` なら、名前と値の組。`password: hunter2` を
            // 「ただ単語が出てきただけ」と読むと、フォーム形式の本文が素通りする。
            let Some(value) = after.strip_prefix([':', '=']) else {
                continue;
            };
            // 値はその場の区切りまで。`&` と改行で切る。
            let value = value
                .trim_start()
                .split(['&', '\n', '\r'])
                .next()
                .unwrap_or("");
            if !only_template(value) {
                assigned = true;
                hit = Some(word);
            }
        }
    }
    let Some(word) = hit else {
        return Masked::clean(raw);
    };
    // 割り当ての形が 1 つも無く、全部が参照なら、生の値はどこにも無い。
    if !assigned && only_template(raw) {
        return Masked::clean(raw);
    }
    let name = word.to_string();
    Masked {
        text: format!("(本文は画面に出しません。`ailo show` で確かめてください) {MASK}"),
        found: Some(if assigned {
            Found::Assigned(name)
        } else {
            Found::Suspected(name)
        }),
    }
}

fn mask_json_in_place(value: &mut serde_json::Value) -> Option<Found> {
    let mut found = None;
    match value {
        serde_json::Value::Object(map) => {
            for (key, v) in map.iter_mut() {
                // 数値・真偽値は参照ではないので、秘匿名の下なら落とす。
                let safe = match v {
                    serde_json::Value::String(s) => only_template(s),
                    _ => false,
                };
                if is_sensitive_field(key) && !safe {
                    *v = serde_json::Value::String(MASK.to_string());
                    found = found.or(Some(Found::Assigned(key.clone())));
                } else {
                    // **走査は必ず最後まで回す。** `found` が埋まった時点で
                    // `or_else` が再帰を止めていたときは、最初の 1 件を見つけた
                    // あとの subtree が丸ごとマスクされずに残った。
                    let nested = mask_json_in_place(v);
                    found = found.or(nested);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for v in items.iter_mut() {
                let nested = mask_json_in_place(v);
                found = found.or(nested);
            }
        }
        _ => {}
    }
    found
}

/// URL。`?api_key=<生の値>` は画面にも保存先にも残したくない。
///
/// **`Redactor::url` に投げるだけでは足りない。** あれは `Url::parse` に失敗した
/// 入力をそのまま返すので、`{{base_url}}/x?api_key=...` という**この repo で
/// 一番普通の形**だけが落ちない。クエリは自分で分解し、そのうえで parse できる
/// ものは `Redactor::url` にも通す（userinfo のパスワードなど、クエリ以外の経路）。
pub fn mask_url(url: &str) -> Masked {
    let mut found = None;
    let masked = mask_query(url, &mut found);
    let text = if reqwest::Url::parse(&masked).is_ok() {
        let r = Redactor::new(true);
        if let Ok(parsed) = reqwest::Url::parse(&masked) {
            if parsed.password().is_some_and(|p| !only_template(p)) {
                found = found.or(Some(Found::Assigned("URL のパスワード".into())));
            }
        }
        r.url(&masked)
    } else {
        masked
    };
    Masked { text, found }
}

fn mask_query(url: &str, found: &mut Option<Found>) -> String {
    let Some((head, query)) = url.split_once('?') else {
        return url.to_string();
    };
    // fragment は落とさない。秘匿値の置き場所ではないうえ、`#` の後ろまで
    // クエリとして扱うと、素の断片まで書き換えてしまう。
    let (query, fragment) = match query.split_once('#') {
        Some((q, f)) => (q, Some(f)),
        None => (query, None),
    };
    let masked: Vec<String> = query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            // **名前は `%XX` を戻してから判定する。** 戻さないと `api%5Fkey` が
            // 「秘匿らしい名前」に当たらず、ここは素通り。一方 parse できる URL では
            // `Redactor::url` が decode した名前で伏せるので、
            // 「画面は伏せ字なのに保存の門は素通り」という**判定の分裂**が戻る。
            Some((name, value))
                if is_sensitive_field(&percent_decoded(name)) && !only_template(value) =>
            {
                *found = found.take().or(Some(Found::Assigned(name.to_string())));
                format!("{name}={MASK}")
            }
            _ => pair.to_string(),
        })
        .collect();
    let mut out = format!("{head}?{}", masked.join("&"));
    if let Some(f) = fragment {
        out.push('#');
        out.push_str(f);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // 定義に直接書かれた秘匿値 — 通す側と塞ぐ側を同数で並べる
    //
    // **片方だけ試すと、必ず片方が壊れる。** 塞ぐ側だけ見ていると、門が
    // 通れない側で壊れていても気づかない（普通の定義が保存できなくなる）。
    // 通す側だけ見ていると、漏れに気づかない。
    // ------------------------------------------------------------------

    fn found_in_item(line: &str) -> Option<Found> {
        mask_item(line, &Redactor::new(true)).found
    }

    /// 画面から落とし、**保存も拒む**もの。
    #[test]
    fn a_literally_written_secret_is_masked_and_blocks_saving() {
        let cases: Vec<(&str, Option<Found>)> = vec![
            (
                "item ヘッダ",
                found_in_item("Authorization: Bearer sk-live-0123456789"),
            ),
            (
                "item フィールド",
                found_in_item("password=hunter2-and-more"),
            ),
            ("item クエリ", found_in_item("api_key==abcdef0123456789")),
            // `parse_item` が読めない綴り。「読めたものだけ検査する」にすると
            // 一番危ない行だけが素通りする。
            ("読めない item", found_in_item("password:=hunter2")),
            // JSON でない本文。`password=` しか探していなかったときは素通りした。
            ("JSON でない本文", mask_body("password: hunter2").found),
            ("JSON の本文", mask_body(r#"{"password":"hunter2"}"#).found),
            // `Url::parse` に失敗する形。この repo で一番普通の書き方。
            (
                "テンプレート混じりの URL",
                mask_url("{{base_url}}/x?api_key=abcdef0123").found,
            ),
            (
                "素の URL",
                mask_url("https://example.com/x?token=abcdef0123").found,
            ),
        ];
        for (what, found) in cases {
            let found = found.unwrap_or_else(|| panic!("{what}: 何も落ちていない"));
            assert!(
                found.blocks_saving(),
                "{what}: 保存が拒まれない ({found:?})"
            );
        }
    }

    /// **通さないといけないもの。** ここが赤くなると、門が通れない側で壊れる。
    #[test]
    fn an_ordinary_definition_is_neither_masked_nor_blocked() {
        let pass: Vec<(&str, Masked)> = vec![
            (
                "ヘッダのテンプレート参照",
                mask_item("Authorization: Bearer {{token}}", &Redactor::new(true)),
            ),
            (
                "フィールドのテンプレート参照",
                mask_item("password={{pw}}", &Redactor::new(true)),
            ),
            (
                "秘匿でない item",
                mask_item("Content-Type: application/json", &Redactor::new(true)),
            ),
            ("秘匿でない本文", mask_body("grant_type=client_credentials")),
            (
                "本文のテンプレート参照",
                mask_body(r#"{"password":"{{pw}}"}"#),
            ),
            (
                "URL のテンプレート参照",
                mask_url("{{base_url}}/x?api_key={{k}}"),
            ),
            ("クエリの無い URL", mask_url("https://example.com/tokens")),
        ];
        for (what, m) in pass {
            assert!(m.found.is_none(), "{what}: 落ちてはいけないものが落ちた");
            assert!(
                !m.text.contains(MASK),
                "{what}: 伏せ字が入っている: {}",
                m.text
            );
        }
    }

    /// 綴りがあるだけの散文は、**画面からは落とすが保存は拒まない。**
    ///
    /// 拒むと `"send me a token"` のようなただの文章が保存できなくなる。
    /// 「画面に出す」と「ファイルに残す」の閾値をここで分けている。
    #[test]
    fn a_body_that_merely_mentions_a_secret_word_is_hidden_but_savable() {
        let m = mask_body("please send me a token when you can");
        let found = m.found.expect("画面から落ちていない");
        assert!(!found.blocks_saving(), "保存まで拒んでいる");
        assert!(m.text.contains(MASK));
    }

    /// 参照を 1 つ混ぜるだけで生の値が素通りしない。
    ///
    /// `contains("{{")` で安全扱いしていたときは、`Bearer {{token}}-literal-secret`
    /// が表示・編集器のガード・保存の門・`last.toml` の全部を通り抜けた。
    #[test]
    fn a_template_reference_glued_to_a_literal_value_does_not_slip_through() {
        let blocked: Vec<(&str, Option<Found>)> = vec![
            (
                "ヘッダ",
                found_in_item("Authorization: Bearer {{token}}-sk-live-0123"),
            ),
            (
                "JSON 本文",
                mask_body(r#"{"password":"{{pw}}-hunter2"}"#).found,
            ),
            (
                "フォーム本文",
                mask_body("password=hunter2&note={{anything}}").found,
            ),
            (
                "URL クエリ",
                mask_url("{{base_url}}/x?api_key={{k}}-sk-live-0123").found,
            ),
        ];
        for (what, found) in blocked {
            let found = found.unwrap_or_else(|| panic!("{what}: 参照混じりが素通りした"));
            assert!(found.blocks_saving(), "{what}: 保存が拒まれない");
        }

        // 認証スキームの語は参照の周りに書かれても値ではない。ここを塞ぐと
        // 一番普通のヘッダが保存できなくなる（通す側で壊れる）。
        assert!(
            found_in_item("Authorization: Bearer {{token}}").is_none(),
            "普通のヘッダまで拒んでいる"
        );
    }

    /// 最初の 1 件を見つけたあとも、走査は最後まで回る。
    ///
    /// `found.or_else(|| 再帰)` にしていたときは、`password` を見つけた時点で
    /// 再帰が止まり、`nested.token` が平文のまま画面に出ていた。
    #[test]
    fn every_secret_in_a_json_body_is_masked_not_just_the_first() {
        let m = mask_body(r#"{"password":"first-secret","nested":{"token":"second-secret"}}"#);
        assert!(!m.text.contains("first-secret"), "{}", m.text);
        assert!(!m.text.contains("second-secret"), "{}", m.text);
    }

    /// `%XX` で綴った秘匿名でも、表示と保存の判定が一致する。
    ///
    /// 名前を decode せずに判定していたときは、`Redactor::url` が表示だけ伏せ、
    /// `found` は `None` のままだった（＝編集器が開き、保存も通った）。
    #[test]
    fn a_percent_encoded_query_name_is_judged_the_same_way_on_both_sides() {
        for url in [
            "https://example.com/x?api%5Fkey=sk-live-0123456789",
            "{{base_url}}/x?api%5Fkey=sk-live-0123456789",
        ] {
            let m = mask_url(url);
            assert!(
                !m.text.contains("sk-live-0123456789"),
                "表示が素通り: {}",
                m.text
            );
            let found = m.found.unwrap_or_else(|| panic!("保存の門が素通り: {url}"));
            assert!(found.blocks_saving(), "{url}");
        }
    }

    /// 落とすものが無い本文は、書いたとおりに出す。
    ///
    /// 常に `serde_json` で組み直していたときは、定義に改行して書いた JSON が
    /// 画面で 1 行に潰れていた。読むための画面で読みにくくしていた。
    #[test]
    fn a_body_with_nothing_to_hide_is_shown_exactly_as_written() {
        let raw = "{\n  \"name\": \"taro\",\n  \"role\": \"editor\"\n}";
        let m = mask_body(raw);
        assert!(m.found.is_none());
        assert_eq!(m.text, raw, "書いたとおりに出ていない");
    }

    /// 落とすのは値。**名前は残す**（何が落ちたか分からないと直せない）。
    #[test]
    fn the_name_survives_but_the_value_does_not() {
        let m = mask_item("password=hunter2-and-more", &Redactor::new(true));
        assert!(m.text.contains("password"), "{}", m.text);
        assert!(!m.text.contains("hunter2"), "{}", m.text);
    }

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
