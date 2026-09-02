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

/// リテラル置換の下限長。これより短い値は置換しない。
///
/// 短い秘匿値をそのまま置換対象にすると、ボディ中の無関係な `1` や `ok` まで `***` になり、
/// ダンプが読めなくなる。短い秘匿値はヘッダ名側で捕まえる。
const MIN_LITERAL_LEN: usize = 8;

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
    pub fn add_literal(&mut self, value: &str) {
        if value.len() >= MIN_LITERAL_LEN && !self.literals.iter().any(|l| l == value) {
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

    /// 任意のテキストから、登録済みの秘匿値そのものを落とす。
    pub fn text(&self, s: &str) -> String {
        if !self.enabled || self.literals.is_empty() {
            return s.to_string();
        }
        let mut out = s.to_string();
        for lit in &self.literals {
            if out.contains(lit.as_str()) {
                out = out.replace(lit.as_str(), MASK);
            }
        }
        out
    }

    /// JSON の値を再帰的に走査し、文字列に含まれる秘匿値を落とす。
    pub fn json(&self, v: &serde_json::Value) -> serde_json::Value {
        use serde_json::Value;
        if !self.enabled || self.literals.is_empty() {
            return v.clone();
        }
        match v {
            Value::String(s) => Value::String(self.text(s)),
            Value::Array(a) => Value::Array(a.iter().map(|x| self.json(x)).collect()),
            Value::Object(o) => {
                Value::Object(o.iter().map(|(k, x)| (k.clone(), self.json(x))).collect())
            }
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
    fn added_header_names_are_honoured() {
        let mut r = Redactor::new(true);
        r.add_header_name("X-Internal-Secret");
        assert_eq!(r.header_value("x-internal-secret", "value"), MASK);
    }
}
