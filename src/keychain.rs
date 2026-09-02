//! API token の OS セキュアストアへの保存・読込・削除。
//!
//! macOS は、アプリの再ビルドで Keychain ACL が無効にならないよう Apple 署名済みの
//! `/usr/bin/security` を仲介する。`security -w` は非 ASCII password を曖昧な hex 表示にするため、
//! macOS では token の UTF-8 bytes を常に hex 化して保存・復元する。Linux はビルド時リンクを
//! 避けて libsecret を dlopen し、Secret Service を利用する。平文ファイルへは保存しない。

use thiserror::Error;

const SERVICE: &str = "ailo";

#[allow(dead_code)] // OS 別 dispatcher のため、各 target では他 OS 用 variant が未使用になる。
#[derive(Debug, Error)]
pub enum AuthError {
    #[error(
        "この OS の永続的な認証情報ストアには対応していません。AILO_SECRET_<ENV>_<KEY> を使ってください"
    )]
    UnsupportedPlatform,
    #[error("security コマンドの起動に失敗しました: {0}")]
    SecurityIo(#[from] std::io::Error),
    #[error("security コマンドが失敗しました (exit {code}): {message}")]
    SecurityCommand { code: i32, message: String },
    #[error("認証情報に制御文字を含めることはできません ({field})")]
    InvalidInput { field: &'static str },
    #[error(
        "libsecret が見つかりません。デスクトップ環境では libsecret を導入、headless では AILO_SECRET_<ENV>_<KEY> を使ってください: {0}"
    )]
    LibsecretUnavailable(String),
    #[error(
        "libsecret の必要な機能を読み込めません。libsecret を更新するか AILO_SECRET_<ENV>_<KEY> を使ってください: {0}"
    )]
    LibsecretSymbol(String),
    #[error(
        "Secret Service へのアクセスに失敗しました。デスクトップのキーリングを確認するか、headless では AILO_SECRET_<ENV>_<KEY> を使ってください: {0}"
    )]
    SecretService(String),
    #[error("Secret Service が不正な文字列を返しました")]
    InvalidSecret,
    #[error("Keychain の保存値を読み取れません。logout して再ログインしてください")]
    InvalidStoredSecret,
}

pub fn save(account: &str, token: &str) -> Result<(), AuthError> {
    #[cfg(target_os = "macos")]
    {
        security_cli::save(SERVICE, account, token)
    }
    #[cfg(target_os = "linux")]
    {
        libsecret::save(SERVICE, account, token)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (account, token);
        Err(AuthError::UnsupportedPlatform)
    }
}

pub fn load(account: &str) -> Result<Option<String>, AuthError> {
    #[cfg(target_os = "macos")]
    {
        security_cli::load(SERVICE, account)
    }
    #[cfg(target_os = "linux")]
    {
        libsecret::load(SERVICE, account)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = account;
        Err(AuthError::UnsupportedPlatform)
    }
}

pub fn delete(account: &str) -> Result<(), AuthError> {
    #[cfg(target_os = "macos")]
    {
        security_cli::delete(SERVICE, account)
    }
    #[cfg(target_os = "linux")]
    {
        libsecret::delete(SERVICE, account)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = account;
        Err(AuthError::UnsupportedPlatform)
    }
}

mod security_cli {
    use std::io::Write;
    use std::process::{Command, Stdio};

    use super::AuthError;

    const SECURITY: &str = "/usr/bin/security";
    const NOT_FOUND: i32 = 44;

    /// 子プロセスへ引き渡す環境変数。
    ///
    /// 環境変数で秘匿値を渡す運用(CI など)があるため、子プロセスの環境をそのまま
    /// 継承させない。同一ユーザーからは他プロセスの環境が見えるため、`security` の
    /// 環境に token を載せる理由がない。`security` が要るのは keychain を探すための
    /// HOME 程度なので、そこだけ通す。
    fn inherited_env() -> Vec<(String, String)> {
        const KEEP: &[&str] = &["HOME", "USER", "LOGNAME", "TMPDIR"];
        std::env::vars()
            .filter(|(k, _)| KEEP.contains(&k.as_str()))
            .collect()
    }

    pub(super) fn save(service: &str, account: &str, token: &str) -> Result<(), AuthError> {
        validate_security_interactive_args(service, account)?;
        let stored_token = hex_encode(token.as_bytes());
        let _ = Command::new(SECURITY)
            .env_clear()
            .envs(inherited_env())
            .args(["delete-generic-password", "-s", service, "-a", account])
            .output();

        let mut child = Command::new(SECURITY)
            .env_clear()
            .envs(inherited_env())
            .arg("-i")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        let command = format!(
            "add-generic-password -U -s {} -a {} -w {}\n",
            quote_security_arg(service),
            quote_security_arg(account),
            quote_security_arg(&stored_token)
        );
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(command.as_bytes())?;
        }
        let output = child.wait_with_output()?;
        map_unit_result(
            output.status.success(),
            output.status.code(),
            &output.stderr,
        )
    }

    pub(super) fn load(service: &str, account: &str) -> Result<Option<String>, AuthError> {
        let output = Command::new(SECURITY)
            .env_clear()
            .envs(inherited_env())
            .args(["find-generic-password", "-s", service, "-a", account, "-w"])
            .output()?;
        map_load_result(
            output.status.success(),
            output.status.code(),
            &output.stdout,
            &output.stderr,
        )
    }

    pub(super) fn delete(service: &str, account: &str) -> Result<(), AuthError> {
        let output = Command::new(SECURITY)
            .env_clear()
            .envs(inherited_env())
            .args(["delete-generic-password", "-s", service, "-a", account])
            .output()?;
        map_delete_result(
            output.status.success(),
            output.status.code(),
            &output.stderr,
        )
    }

    fn quote_security_arg(value: &str) -> String {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }

    fn validate_security_interactive_args(service: &str, account: &str) -> Result<(), AuthError> {
        for (field, value) in [("service", service), ("account", account)] {
            if value.chars().any(|character| character <= '\u{001f}') {
                return Err(AuthError::InvalidInput { field });
            }
        }
        Ok(())
    }

    fn hex_encode(bytes: &[u8]) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut encoded = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        encoded
    }

    fn hex_decode(value: &str) -> Result<String, AuthError> {
        if !value.len().is_multiple_of(2) {
            return Err(AuthError::InvalidStoredSecret);
        }
        let mut decoded = Vec::with_capacity(value.len() / 2);
        for pair in value.as_bytes().chunks_exact(2) {
            let high = hex_digit(pair[0]).ok_or(AuthError::InvalidStoredSecret)?;
            let low = hex_digit(pair[1]).ok_or(AuthError::InvalidStoredSecret)?;
            decoded.push((high << 4) | low);
        }
        String::from_utf8(decoded).map_err(|_| AuthError::InvalidStoredSecret)
    }

    fn hex_digit(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }

    fn command_error(code: Option<i32>, stderr: &[u8]) -> AuthError {
        AuthError::SecurityCommand {
            code: code.unwrap_or(-1),
            message: String::from_utf8_lossy(stderr).trim().to_string(),
        }
    }

    fn map_unit_result(success: bool, code: Option<i32>, stderr: &[u8]) -> Result<(), AuthError> {
        if success {
            Ok(())
        } else {
            Err(command_error(code, stderr))
        }
    }

    fn map_load_result(
        success: bool,
        code: Option<i32>,
        stdout: &[u8],
        stderr: &[u8],
    ) -> Result<Option<String>, AuthError> {
        if success {
            let value = String::from_utf8_lossy(stdout)
                .trim_end_matches(['\r', '\n'])
                .to_string();
            hex_decode(&value).map(Some)
        } else if code == Some(NOT_FOUND) {
            Ok(None)
        } else {
            Err(command_error(code, stderr))
        }
    }

    fn map_delete_result(success: bool, code: Option<i32>, stderr: &[u8]) -> Result<(), AuthError> {
        if success || code == Some(NOT_FOUND) {
            Ok(())
        } else {
            Err(command_error(code, stderr))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn quotes_security_interactive_arguments() {
            assert_eq!(quote_security_arg(""), "\"\"");
            assert_eq!(quote_security_arg("a\"b"), "\"a\\\"b\"");
            assert_eq!(quote_security_arg("a\\b"), "\"a\\\\b\"");
            assert_eq!(quote_security_arg("a b"), "\"a b\"");
            assert_eq!(quote_security_arg("日本語"), "\"日本語\"");
            assert_eq!(quote_security_arg("me@example.com"), "\"me@example.com\"");
        }

        #[test]
        fn interactive_identifiers_reject_c0_control_characters() {
            for (field, service, account) in [
                ("service", "bad\nservice", "me@example.com"),
                ("service", "bad\tservice", "me@example.com"),
                ("account", "ailo", "bad\remail"),
                ("account", "ailo", "bad\0email"),
            ] {
                assert!(matches!(
                    validate_security_interactive_args(service, account),
                    Err(AuthError::InvalidInput { field: actual }) if actual == field
                ));
            }
        }

        #[test]
        fn interactive_arguments_accept_normal_values() {
            assert!(validate_security_interactive_args("ailo", "me@example.com").is_ok());
        }

        #[test]
        fn hex_encode_handles_empty_ascii_and_japanese() {
            assert_eq!(hex_encode(b""), "");
            assert_eq!(hex_encode(b"token"), "746f6b656e");
            assert_eq!(hex_encode("日本語".as_bytes()), "e697a5e69cace8aa9e");
        }

        #[test]
        fn hex_roundtrip_handles_unicode_and_control_characters() {
            for token in ["", "token", "日本語", "line1\nline2\t\0"] {
                assert_eq!(
                    hex_decode(&hex_encode(token.as_bytes())).expect("decode"),
                    token
                );
            }
        }

        #[test]
        fn hex_decode_rejects_odd_length_and_non_hex_input() {
            assert!(matches!(
                hex_decode("0"),
                Err(AuthError::InvalidStoredSecret)
            ));
            assert!(matches!(
                hex_decode("gg"),
                Err(AuthError::InvalidStoredSecret)
            ));
        }

        #[test]
        fn load_rejects_invalid_stored_hex_with_recovery_guidance() {
            let error = map_load_result(true, Some(0), b"not-hex\n", b"")
                .expect_err("invalid stored value");
            assert!(matches!(error, AuthError::InvalidStoredSecret));
            assert!(error.to_string().contains("logout して再ログイン"));
        }

        #[test]
        fn load_maps_success_and_trims_only_line_endings() {
            assert_eq!(
                map_load_result(true, Some(0), b"2073656372657420\r\n", b"").expect("map"),
                Some(" secret ".to_string())
            );
        }

        #[test]
        fn load_maps_not_found_to_none() {
            assert_eq!(
                map_load_result(false, Some(NOT_FOUND), b"", b"missing").expect("map"),
                None
            );
        }

        #[test]
        fn delete_maps_not_found_to_success() {
            assert!(map_delete_result(false, Some(NOT_FOUND), b"missing").is_ok());
        }

        #[test]
        fn failures_include_exit_code_and_stderr() {
            let error = map_load_result(false, Some(1), b"", b"denied").expect_err("failure");
            assert!(
                matches!(error, AuthError::SecurityCommand { code: 1, ref message } if message == "denied")
            );
        }

        #[test]
        #[ignore = "実 macOS Keychain に読み書きするため実機で手動実行"]
        fn real_keychain_roundtrip() {
            struct Cleanup<'a> {
                service: &'a str,
                account: &'a str,
            }
            impl Drop for Cleanup<'_> {
                fn drop(&mut self) {
                    let _ = delete(self.service, self.account);
                }
            }

            let service = "ailo-test";
            let account = "stg/access_token";
            let token = "quote=\" slash=\\ space 日本語";
            let _ = delete(service, account);
            let _cleanup = Cleanup { service, account };
            save(service, account, token).expect("save");
            assert_eq!(load(service, account).expect("load").as_deref(), Some(token));
            delete(service, account).expect("delete");
            assert_eq!(load(service, account).expect("load after delete"), None);
        }
    }
}

#[allow(dead_code)] // 全 OS で型検査するが、呼び出すのは Linux dispatcher だけ。
mod libsecret {
    use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_void};
    use std::ptr;

    use libloading::{Library, Symbol};

    use super::AuthError;

    const LIBSECRET: &str = "libsecret-1.so.0";
    const ATTRIBUTE_SLOTS: usize = 32;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct SecretSchemaAttribute {
        name: *const c_char,
        attribute_type: c_int,
    }

    #[repr(C)]
    struct SecretSchema {
        name: *const c_char,
        flags: c_int,
        attributes: [SecretSchemaAttribute; ATTRIBUTE_SLOTS],
        reserved: c_int,
        reserved_ptrs: [*mut c_void; 7],
    }

    #[repr(C)]
    struct GError {
        domain: c_uint,
        code: c_int,
        message: *mut c_char,
    }

    type StoreFn = unsafe extern "C" fn(
        *const SecretSchema,
        *const c_char,
        *const c_char,
        *const c_char,
        *mut c_void,
        *mut *mut GError,
        ...
    ) -> c_int;
    type LookupFn = unsafe extern "C" fn(
        *const SecretSchema,
        *mut c_void,
        *mut *mut GError,
        ...
    ) -> *mut c_char;
    type ClearFn =
        unsafe extern "C" fn(*const SecretSchema, *mut c_void, *mut *mut GError, ...) -> c_int;
    type PasswordFreeFn = unsafe extern "C" fn(*mut c_char);
    type ErrorFreeFn = unsafe extern "C" fn(*mut GError);

    struct Api {
        _library: Library,
        store: StoreFn,
        lookup: LookupFn,
        clear: ClearFn,
        password_free: PasswordFreeFn,
        error_free: ErrorFreeFn,
    }

    impl Api {
        fn load() -> Result<Self, AuthError> {
            // SAFETY: The fixed soname is loaded only for the duration held by `Api`.
            let library = unsafe { Library::new(LIBSECRET) }
                .map_err(|error| AuthError::LibsecretUnavailable(error.to_string()))?;
            // SAFETY: Symbol names and signatures match libsecret/glib's stable C API. Function
            // pointers are copied while `library` remains owned by the returned `Api`.
            unsafe {
                Ok(Self {
                    store: symbol(&library, b"secret_password_store_sync\0")?,
                    lookup: symbol(&library, b"secret_password_lookup_sync\0")?,
                    clear: symbol(&library, b"secret_password_clear_sync\0")?,
                    password_free: symbol(&library, b"secret_password_free\0")?,
                    error_free: symbol(&library, b"g_error_free\0")?,
                    _library: library,
                })
            }
        }
    }

    unsafe fn symbol<T: Copy>(library: &Library, name: &[u8]) -> Result<T, AuthError> {
        // SAFETY: The caller supplies the C API's exact symbol type and keeps the library loaded.
        let value: Symbol<'_, T> = unsafe { library.get(name) }
            .map_err(|error| AuthError::LibsecretSymbol(error.to_string()))?;
        Ok(*value)
    }

    struct Inputs {
        schema_name: CString,
        service_name: CString,
        user_name: CString,
        service: CString,
        account: CString,
    }

    impl Inputs {
        fn new(service: &str, account: &str) -> Result<Self, AuthError> {
            Ok(Self {
                schema_name: cstring(service, "schema name")?,
                service_name: cstring("service", "attribute name")?,
                user_name: cstring("user", "attribute name")?,
                service: cstring(service, "service")?,
                account: cstring(account, "account")?,
            })
        }

        fn schema(&self) -> SecretSchema {
            let empty = SecretSchemaAttribute {
                name: ptr::null(),
                attribute_type: 0,
            };
            let mut attributes = [empty; ATTRIBUTE_SLOTS];
            attributes[0].name = self.service_name.as_ptr();
            attributes[1].name = self.user_name.as_ptr();
            SecretSchema {
                name: self.schema_name.as_ptr(),
                flags: 0,
                attributes,
                reserved: 0,
                reserved_ptrs: [ptr::null_mut(); 7],
            }
        }
    }

    fn cstring(value: &str, field: &'static str) -> Result<CString, AuthError> {
        CString::new(value).map_err(|_| AuthError::InvalidInput { field })
    }

    fn finish_error(api: &Api, error: *mut GError) -> AuthError {
        if error.is_null() {
            return AuthError::SecretService("詳細情報がありません".to_string());
        }
        // SAFETY: libsecret returned a valid GError. Its message is read before g_error_free.
        let message = unsafe {
            let pointer = (*error).message;
            let message = if pointer.is_null() {
                "詳細情報がありません".to_string()
            } else {
                CStr::from_ptr(pointer).to_string_lossy().into_owned()
            };
            (api.error_free)(error);
            message
        };
        AuthError::SecretService(message)
    }

    pub(super) fn save(service: &str, account: &str, token: &str) -> Result<(), AuthError> {
        let api = Api::load()?;
        let inputs = Inputs::new(service, account)?;
        let schema = inputs.schema();
        let label = cstring(&format!("{service} ({account})"), "label")?;
        let password = cstring(token, "token")?;
        let mut error = ptr::null_mut();
        // SAFETY: All pointers refer to live CStrings/schema for the call; varargs are two
        // name/value string pairs terminated by NULL, as required by libsecret.
        let stored = unsafe {
            (api.store)(
                &schema,
                ptr::null(),
                label.as_ptr(),
                password.as_ptr(),
                ptr::null_mut(),
                &mut error,
                inputs.service_name.as_ptr(),
                inputs.service.as_ptr(),
                inputs.user_name.as_ptr(),
                inputs.account.as_ptr(),
                ptr::null::<c_char>(),
            )
        };
        if !error.is_null() {
            Err(finish_error(&api, error))
        } else if stored == 0 {
            Err(AuthError::SecretService("保存に失敗しました".to_string()))
        } else {
            Ok(())
        }
    }

    pub(super) fn load(service: &str, account: &str) -> Result<Option<String>, AuthError> {
        let api = Api::load()?;
        let inputs = Inputs::new(service, account)?;
        let schema = inputs.schema();
        let mut error = ptr::null_mut();
        // SAFETY: The schema and attribute CStrings live through the call; varargs end in NULL.
        let password = unsafe {
            (api.lookup)(
                &schema,
                ptr::null_mut(),
                &mut error,
                inputs.service_name.as_ptr(),
                inputs.service.as_ptr(),
                inputs.user_name.as_ptr(),
                inputs.account.as_ptr(),
                ptr::null::<c_char>(),
            )
        };
        if !error.is_null() {
            return Err(finish_error(&api, error));
        }
        if password.is_null() {
            return Ok(None);
        }
        // SAFETY: libsecret returns a NUL-terminated allocated password, freed exactly once after
        // copying. Invalid UTF-8 is reported without exposing its bytes.
        let result = unsafe {
            let parsed = CStr::from_ptr(password)
                .to_str()
                .map(str::to_owned)
                .map_err(|_| AuthError::InvalidSecret);
            (api.password_free)(password);
            parsed
        };
        result.map(Some)
    }

    pub(super) fn delete(service: &str, account: &str) -> Result<(), AuthError> {
        let api = Api::load()?;
        let inputs = Inputs::new(service, account)?;
        let schema = inputs.schema();
        let mut error = ptr::null_mut();
        // SAFETY: The schema and attribute CStrings live through the call; varargs end in NULL.
        unsafe {
            (api.clear)(
                &schema,
                ptr::null_mut(),
                &mut error,
                inputs.service_name.as_ptr(),
                inputs.service.as_ptr(),
                inputs.user_name.as_ptr(),
                inputs.account.as_ptr(),
                ptr::null::<c_char>(),
            );
        }
        if error.is_null() {
            Ok(())
        } else {
            Err(finish_error(&api, error))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn schema_has_service_and_user_attributes() {
            let inputs = Inputs::new("ailo", "me@example.com").expect("inputs");
            let schema = inputs.schema();
            // SAFETY: attribute pointers refer to CStrings owned by `inputs`.
            let names = unsafe {
                [
                    CStr::from_ptr(schema.attributes[0].name)
                        .to_str()
                        .expect("service"),
                    CStr::from_ptr(schema.attributes[1].name)
                        .to_str()
                        .expect("user"),
                ]
            };
            assert_eq!(names, ["service", "user"]);
            assert!(schema.attributes[2].name.is_null());
        }

        #[test]
        fn input_nul_is_rejected_without_loading_libsecret() {
            assert!(matches!(
                Inputs::new("ailo", "bad\0email"),
                Err(AuthError::InvalidInput { field: "account" })
            ));
        }

        #[test]
        fn ffi_layout_has_expected_attribute_capacity() {
            assert_eq!(ATTRIBUTE_SLOTS, 32);
            assert_eq!(
                std::mem::size_of::<SecretSchemaAttribute>(),
                std::mem::size_of::<(*const c_char, c_int)>()
            );
        }
    }
}
