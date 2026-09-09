//! サブコマンドの実行。

use std::collections::BTreeMap;

use anyhow::{anyhow, bail, Context, Result};
use reqwest::Method;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::args::{self, Item};
use crate::capture;
use crate::cli::{
    Command, CommonArgs, ConfigCommand, EnvCommand, LogArgs, RequestArgs, RunArgs, SaveArgs,
    SecretCommand, ShowArgs,
};
use crate::config::{Config, LastInvocation, Requests, SavedRequest, State};
use crate::config_edit;
use crate::dump::{self, Dump, Retention};
use crate::output::{self, Format, Palette};
use crate::paths;
use crate::pick;
use crate::redact::{self, Redactor};
use crate::secrets;
use crate::shape;
use crate::vars::{self, Layer, Vars};

/// プロセスの終了コード。
pub struct Outcome {
    pub code: i32,
}

const OK: Outcome = Outcome { code: 0 };
/// 「探したが無かった」。エラー(2)ではない。`git config` と同じ流儀。
const NOT_FOUND: Outcome = Outcome { code: 1 };

pub async fn run(command: Command) -> Result<Outcome> {
    if let Some((method, a)) = command.as_request() {
        return adhoc(method, a).await;
    }
    match command {
        Command::Run(a) => saved(&a).await,
        Command::Save(a) => save(&a),
        Command::New { name } => new_request(&name),
        Command::Tui(a) => crate::tui::run(a.env).await,
        Command::Ls => list_requests(),
        Command::Env(a) => match a.action {
            None => list_envs(),
            Some(EnvCommand::Use { name }) => use_env(&name),
        },
        Command::Config(c) => config_command(&c),
        Command::Secret(c) => secret(&c),
        Command::Log(a) => log(&a),
        Command::Show(a) => show(&a),
        _ => unreachable!("リクエスト系は as_request で処理済み"),
    }
}

// ------------------------------------------------------------------ リクエスト

/// 送信前に確定させたテンプレート一式。アドホックと保存済みで共通。
struct Recipe {
    method: String,
    url: String,
    items: Vec<String>,
    raw: Option<String>,
    form: bool,
    name: Option<String>,
    capture_spec: BTreeMap<String, String>,
    secret_names: Vec<String>,
}

async fn adhoc(method: &str, a: &RequestArgs) -> Result<Outcome> {
    let recipe = Recipe {
        method: method.to_string(),
        url: a.url.clone(),
        items: a.items.clone(),
        raw: a.raw.clone(),
        form: a.form,
        name: None,
        capture_spec: BTreeMap::new(),
        secret_names: Vec::new(),
    };
    // `ailo save` の材料。直書きされた秘匿値は落としてから記録する。
    record_last(&recipe)?;
    execute(recipe, &a.common).await
}

/// 定義に直接書かれた秘匿値の**名前**。値は返さない。
///
/// **判定は `redact` 側に 1 つしかない。** 表示のマスクと保存の門が別の関数だった
/// ときは、片方だけ塞がった穴が入口を足すたびに開いた（TUI の編集器で 3 経路）。
/// ここは同じ判定の「保存を拒む側の閾値」を選ぶだけにする。
pub(crate) fn literal_secrets(req: &SavedRequest) -> Vec<String> {
    literal_secrets_with(req, &Redactor::new(true))
}

fn literal_secrets_with(req: &SavedRequest, r: &Redactor) -> Vec<String> {
    let mut found: Vec<String> = req
        .items
        .iter()
        .filter_map(|raw| blocking_name(redact::mask_item(raw, r).found))
        .collect();
    if let Some(name) = req
        .raw
        .as_deref()
        .and_then(|b| blocking_name(redact::mask_body(b).found))
    {
        found.push(format!("--raw の {name}"));
    }
    if let Some(name) = blocking_name(redact::mask_url(&req.url).found) {
        found.push(name);
    }
    found
}

/// テスト用の薄い包み。1 行に対する「保存を拒む側の閾値」。
#[cfg(test)]
fn blocking_name_of(raw: &str, r: &Redactor) -> Option<String> {
    blocking_name(redact::mask_item(raw, r).found)
}

fn blocking_name(found: Option<redact::Found>) -> Option<String> {
    found
        .filter(redact::Found::blocks_saving)
        .map(|f| f.name().to_string())
}

/// 直前のリクエストを記録する。直書きの秘匿値は値を落とし、名前だけ残す。
///
/// **設定の `redact_headers` をここでも読む。** 送信側の Redactor にだけ足していた
/// ときは、`X-Tenant` のように利用者が秘匿指定したヘッダが、ダンプでは `***` なのに
/// `last.toml` には平文で残った。マスクの定義は 1 か所から両方へ配る。
fn record_last(recipe: &Recipe) -> Result<()> {
    let mut r = Redactor::new(true);
    for name in &Config::load()?.redact_headers {
        r.add_header_name(name);
    }
    let mut items = Vec::with_capacity(recipe.items.len());
    let mut redacted = Vec::new();
    for raw in &recipe.items {
        match blocking_name(redact::mask_item(raw, &r).found) {
            Some(name) => {
                // 値そのものは書かない。何が落ちたかだけ残す。
                redacted.push(name);
            }
            None => items.push(raw.clone()),
        }
    }

    // `--raw` と URL も同じ扱いにする。片方だけ守っても意味がない。
    let mut raw_body = recipe.raw.clone();
    if let Some(name) = recipe
        .raw
        .as_deref()
        .and_then(|b| blocking_name(redact::mask_body(b).found))
    {
        redacted.push(format!("--raw の {name}"));
        raw_body = None;
    }
    let mut url = recipe.url.clone();
    if let Some(name) = blocking_name(redact::mask_url(&recipe.url).found) {
        redacted.push(name);
        url = r.url(&recipe.url);
    }

    LastInvocation {
        method: recipe.method.clone(),
        url,
        items,
        raw: raw_body,
        form: recipe.form,
        redacted,
    }
    .record()
}

/// 保存済みリクエストを 1 件送り、結果を返す。表示はしない。
///
/// TUI 用の入口。`Recipe` を外へ出さずに済むよう、名前で受ける。CLI の
/// `ailo run <名前>` と**同じ recipe 組み立て**を通るので、変数展開・マスク・
/// ダンプ・capture の扱いが片方だけ古くなることがない。
pub async fn send_saved(
    name: &str,
    common: &CommonArgs,
    notes: &mut Vec<String>,
) -> Result<Performed> {
    perform(saved_recipe(name, &[])?, common, notes).await
}

fn saved_recipe(name: &str, extra_items: &[String]) -> Result<Recipe> {
    let reqs = Requests::load()?;
    let saved = reqs.get(name).ok_or_else(|| {
        let known = reqs.names().join(", ");
        // どの workspace で探して見つからなかったのかを必ず添える。
        if known.is_empty() {
            anyhow!(
                "保存済みリクエストがありません（{}）。`ailo new <名前>` で定義できます",
                where_we_are()
            )
        } else {
            anyhow!(
                "`{name}` は保存されていません（{}）。あるのは: {known}",
                where_we_are()
            )
        }
    })?;

    let mut items = saved.items.clone();
    items.extend_from_slice(extra_items);

    Ok(Recipe {
        method: saved.method.clone(),
        url: saved.url.clone(),
        items,
        raw: saved.raw.clone(),
        form: saved.form,
        name: Some(name.to_string()),
        capture_spec: saved.capture.clone(),
        secret_names: saved.secret.clone(),
    })
}

async fn saved(a: &RunArgs) -> Result<Outcome> {
    execute(saved_recipe(&a.name, &a.items)?, &a.common).await
}

/// いま効いている変数の一覧。**秘匿値は伏せた形で返る。**
///
/// TUI の変数一覧から使う。CLI と同じ層の組み方(`build_vars`)を通すので、
/// 「画面で見えている値」と「送るときに使われる値」がずれない。
pub fn variables(env: Option<&str>) -> Result<Vec<(vars::VarDescription, Option<String>)>> {
    let cfg = Config::load()?;
    let env = cfg.resolve_env(env);
    let v = build_vars(&cfg, env.as_deref(), &[])?;
    let expiry = match env.as_deref() {
        Some(name) => State::load(name)?.expires_at,
        None => BTreeMap::new(),
    };
    Ok(v.describe()
        .into_iter()
        .map(|d| {
            let at = expiry.get(&d.name).cloned();
            (d, at)
        })
        .collect())
}

/// 変数の層を優先順に組む。先頭が最優先。
fn build_vars(cfg: &Config, env: Option<&str>, cli_vars: &[String]) -> Result<Vars> {
    let mut layers = Vec::new();

    let cli: BTreeMap<String, String> = cli_vars
        .iter()
        .map(|s| vars::parse_assignment(s))
        .collect::<Result<_>>()?;
    layers.push(Layer::new("cli", cli, false));
    layers.extend(vars::layer_from_process_env());

    if let Some(env) = env {
        layers.push(Layer::new("keychain", secrets::load_env(env)?, true));
        layers.push(Layer::new("state", State::load(env)?.vars, false));
    }

    layers.push(Layer::new("env-config", cfg.env_config(env).vars, false));
    layers.push(Layer::new("config", cfg.vars.clone(), false));

    Ok(Vars::from_layers(layers))
}

/// 設定の共通ヘッダと環境ヘッダを重ねる。item のヘッダが最後に勝つ。
///
/// **突き合わせは名前を小文字にしてから行う。** 設定は綴りをそのままキーにした表なので、
/// `[headers] accept` と `[env.stg.headers] Accept` を別物として持ててしまう。綴りの違いで
/// 層の優先順位が入れ替わると、環境ごとに上書きしたつもりの指定が黙って無視される。
/// 送る綴りは後から来たほう(環境側)に合わせる。
/// 同じテーブルに `Accept` と `accept` を両方書いた場合に、その名前を返す。
///
/// 送られるのは片方だけになる。しかも設定は `BTreeMap` なので反復順は綴りの
/// ソート順で、**ファイルの記述順とも一致しない**。黙って捨てると
/// 「書いたヘッダが理由なく消える」ように見えるので、名前を出して知らせる。
fn duplicate_header_spellings(headers: &BTreeMap<String, String>) -> Vec<String> {
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    let mut dupes = Vec::new();
    for name in headers.keys() {
        if let Some(first) = seen.insert(name.to_ascii_lowercase(), name.clone()) {
            dupes.push(format!("{first} と {name}"));
        }
    }
    dupes
}

fn config_headers(cfg: &Config, env: Option<&str>) -> Vec<(String, String)> {
    let mut merged: BTreeMap<String, (String, String)> = BTreeMap::new();
    for (name, value) in cfg.headers.iter().chain(cfg.env_config(env).headers.iter()) {
        merged.insert(name.to_ascii_lowercase(), (name.clone(), value.clone()));
    }
    merged.into_values().collect()
}

/// item のテンプレートを展開する。クエリに秘匿値が載ったら警告する。
fn expand_item(item: &Item, v: &Vars, warn: &mut Vec<String>) -> Result<Item> {
    Ok(match item {
        Item::Header { name, value } => Item::Header {
            name: v.expand(name)?.text,
            value: v.expand(value)?.text,
        },
        Item::Query { name, value } => {
            let expanded = v.expand(value)?;
            if expanded.used_secret {
                // URL はサーバのアクセスログ、プロキシ、Referer に残る。
                warn.push(format!("クエリ `{name}` に秘匿値を展開しました"));
            }
            Item::Query {
                name: v.expand(name)?.text,
                value: expanded.text,
            }
        }
        Item::Field { name, value } => Item::Field {
            name: v.expand(name)?.text,
            value: v.expand(value)?.text,
        },
        Item::RawField { name, value } => {
            // raw は JSON。文字列化して展開し、読み直す。
            let expanded = v.expand(&value.to_string())?.text;
            Item::RawField {
                name: v.expand(name)?.text,
                // 展開後の文字列そのものは載せない。秘匿値が入っている可能性がある。
                value: serde_json::from_str(&expanded)
                    .with_context(|| format!("`{name}:=` は変数展開後に JSON でなくなりました"))?,
            }
        }
        Item::FileField { name, path } => Item::FileField {
            name: v.expand(name)?.text,
            path: v.expand(&path.to_string_lossy())?.text.into(),
        },
    })
}

/// エラーの連鎖から秘匿値を落として組み直す。
///
/// `main` は `err.chain()` をすべて標準エラーへ出す。展開後の URL や JSON 本文が
/// context に載る経路があるので、そのままでは失敗のたびに秘匿値が表示される。
/// 連鎖の構造は保ったまま、各段の文言だけマスクを通す。
fn redact_error(r: &Redactor, err: anyhow::Error) -> anyhow::Error {
    let mut messages: Vec<String> = err.chain().map(|c| r.text(&c.to_string())).collect();
    // 一番奥から積み直す。
    let deepest = messages.pop().unwrap_or_else(|| "不明なエラー".into());
    let mut rebuilt = anyhow!(deepest);
    for message in messages.into_iter().rev() {
        rebuilt = rebuilt.context(message);
    }
    rebuilt
}

/// 期限切れの秘匿値を使おうとしていないか、送信前に確かめる。
fn check_expiry(state: &State, referenced: &[String], now: OffsetDateTime) -> Result<()> {
    let expired: Vec<&str> = state
        .expired(now)
        .into_iter()
        .filter(|name| referenced.iter().any(|r| r == name))
        .collect();
    if expired.is_empty() {
        return Ok(());
    }
    // 黙って 401 を受けるより速く、原因も明確。
    bail!(
        "期限切れの値を使おうとしています: {}。取り直してから再実行してください (例: `ailo run login`)",
        expired.join(", ")
    );
}

/// 送信の結果。表示はしない。
///
/// TUI からも同じ経路で送るために、**送ること**と**見せること**を分けてある。
/// 片方だけを別実装にすると、マスク・ダンプ・capture のどれかが TUI 経由では
/// 効かない、という壊れ方をする。
pub struct Performed {
    /// マスク前のレスポンス。表示の直前に必ず `redactor` を通すこと。
    pub response: crate::dump::ResponseRecord,
    pub dump_path: Option<std::path::PathBuf>,
    pub redactor: Redactor,
}

async fn execute(recipe: Recipe, common: &CommonArgs) -> Result<Outcome> {
    let palette = Palette::detect();
    // **notes は呼び出し側が持つ。** `perform` の戻り値に載せていたときは、
    // 送信やダンプ書き込みが失敗した瞬間に、そこまでに出ていた警告
    // （未知の環境、重複ヘッダ、URL への秘匿値展開）が全部消えていた。
    // 警告が一番効くのは失敗したときなので、消える向きの設計にしない。
    let mut notes = Vec::new();
    let done = perform(recipe, common, &mut notes).await;
    for note in &notes {
        eprintln!("{}", palette.dim(note));
    }
    let done = done?;
    render(
        &done.response,
        done.dump_path.as_deref(),
        common,
        &done.redactor,
        &palette,
    )?;
    Ok(exit_for(done.response.status, common.fail))
}

/// 送るところまで。表示も終了コードの決定もしない。
async fn perform(
    recipe: Recipe,
    common: &CommonArgs,
    notes: &mut Vec<String>,
) -> Result<Performed> {
    let cfg = Config::load()?;
    let env = cfg.resolve_env(common.env.as_deref());

    if let (Some(name), false) = (env.as_deref(), cfg.environments().is_empty()) {
        if !cfg.knows_env(name) {
            notes.push(format!(
                "環境 `{name}` は設定にありません。あるのは: {}",
                cfg.environments().join(", ")
            ));
        }
    }

    let v = build_vars(&cfg, env.as_deref(), &common.vars)?;

    // テンプレートが参照している名前を集め、期限切れを送信前に捕まえる。
    //
    // URL と item だけを見ていると、`--raw '{"token":"{{access_token}}"}'` や
    // 設定側の `[env.prd.headers] Authorization = "Bearer {{access_token}}"` を
    // 取りこぼし、期限切れの token をそのまま送ってしまう。参照元は全部並べる。
    if let Some(env_name) = env.as_deref() {
        let mut referenced = Vars::referenced_names(&recipe.url);
        for item in &recipe.items {
            referenced.extend(Vars::referenced_names(item));
        }
        if let Some(raw) = &recipe.raw {
            referenced.extend(Vars::referenced_names(raw));
        }
        for (name, value) in config_headers(&cfg, env.as_deref()) {
            referenced.extend(Vars::referenced_names(&name));
            referenced.extend(Vars::referenced_names(&value));
        }
        check_expiry(
            &State::load(env_name)?,
            &referenced,
            OffsetDateTime::now_utc(),
        )?;
    }

    let mut redactor = if common.no_redact {
        Redactor::disabled()
    } else {
        let mut r = Redactor::new(true);
        for name in &cfg.redact_headers {
            r.add_header_name(name);
        }
        r
    };
    // 変数として解決した秘匿値は、どの経路で本文に現れても落とす。
    for value in v.secret_values() {
        redactor.add_literal(value);
    }

    for dupe in
        duplicate_header_spellings(&cfg.headers)
            .into_iter()
            .chain(duplicate_header_spellings(
                &cfg.env_config(env.as_deref()).headers,
            ))
    {
        notes.push(format!(
            "警告: 同じ設定に大文字小文字だけが違うヘッダがあります({dupe})。送られるのは片方だけです"
        ));
    }

    // 設定のヘッダを先に、item のヘッダを後に。後勝ちで item が上書きする。
    let mut item_templates: Vec<Item> = config_headers(&cfg, env.as_deref())
        .into_iter()
        .map(|(name, value)| Item::Header { name, value })
        .collect();
    for raw in &recipe.items {
        item_templates.push(args::parse_item(raw)?);
    }

    let mut warnings = Vec::new();
    let items: Vec<Item> = item_templates
        .iter()
        .map(|i| expand_item(i, &v, &mut warnings))
        .collect::<Result<_>>()?;
    for w in &warnings {
        notes.push(format!("警告: {w}"));
    }

    let expanded_url = v.expand(&recipe.url)?;
    if expanded_url.used_secret {
        // `{{base_url}}/x?key={{api_key}}` のように URL に直接書く形が一番自然なので、
        // item のクエリだけ警告していても意味がない。URL はサーバのアクセスログ、
        // プロキシ、Referer に残る。
        notes.push("警告: URL に秘匿値を展開しました".to_string());
    }
    let url = expanded_url.text;
    let raw = recipe
        .raw
        .as_deref()
        .map(|r| v.expand(r))
        .transpose()?
        .map(|e| e.text);

    let method = Method::from_bytes(recipe.method.as_bytes())
        .with_context(|| format!("メソッドとして使えません: {}", recipe.method))?;
    // ここから先のエラー文には展開後の値が載りうる(URL、JSON 本文)。
    // main は原因の連鎖をすべて標準エラーへ出すので、その前に落とす。
    let plan = crate::http::plan(method, &url, &items, recipe.form, raw)
        .map_err(|e| redact_error(&redactor, e))?;

    redactor.learn_from_headers(plan.headers.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    // URL のクエリと userinfo。名前で URL 自体は落とせるが、値を覚えていないと
    // それを本文に反響して返す API で素通りする。
    redactor.learn_from_url(&plan.url);
    // `--raw` はフィールドに分解されないので、JSON として読めるなら中を見る。
    if let Some(body) = &plan.raw {
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(body) {
            redactor.learn_from_json(&parsed);
        }
    }
    // ボディに直書きされた秘匿値(`password=...`、`token=...`)も落とす。
    // ヘッダ名だけを見ていると、ログインの本文がまるごとダンプに残る。
    redactor.learn_from_fields(plan.fields.iter().map(|(k, v)| {
        let text = match v {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        (k.as_str(), text)
    }));

    let sent = crate::http::send(&plan, common.timeout)
        .await
        .map_err(|e| redact_error(&redactor, e))?;

    // キャプチャはマスク前の本文から取る。取った秘匿値はこの後のマスクに登録する。
    let captured = if recipe.capture_spec.is_empty() {
        None
    } else {
        // **ここから先のエラー文にはレスポンス由来の値が載る。**
        // `expires_in` が数値でなければ、その値がそのままエラーに入る。
        // 標準エラーにも TUI の画面にも出るので、必ずマスクを通す。
        let body = body_as_json(&sent.response)
            .context("capture はレスポンスが JSON のときだけ使えます")
            .map_err(|e| redact_error(&redactor, e))?;
        let got = capture::capture(
            &body,
            &recipe.capture_spec,
            &recipe.secret_names,
            OffsetDateTime::now_utc(),
        )
        .map_err(|e| redact_error(&redactor, e))?;
        for value in got.secret_values() {
            redactor.add_literal(value);
        }
        Some(got)
    };

    let dump_path = if common.no_dump {
        None
    } else {
        let record = Dump {
            ts: OffsetDateTime::now_utc()
                .format(&Rfc3339)
                .unwrap_or_else(|_| "unknown".into()),
            name: recipe.name.clone(),
            env: env.clone(),
            redacted: redactor.is_enabled(),
            request: sent.request.clone(),
            response: sent.response.clone(),
        };
        Some(
            dump::write(
                &record,
                &redactor,
                &Retention {
                    keep_count: cfg.retention.keep_count,
                    keep_days: cfg.retention.keep_days,
                },
            )
            .map_err(|e| redact_error(&redactor, e))?
            .path,
        )
    };

    if let Some(got) = captured {
        let env_name = env.as_deref().ok_or_else(|| {
            anyhow!("capture には環境が要ります。`--env <名前>` を指定するか config に default_env を書いてください")
        })?;
        persist_capture(env_name, &got, notes).map_err(|e| redact_error(&redactor, e))?;
    }

    Ok(Performed {
        response: sent.response,
        dump_path,
        redactor,
    })
}

fn persist_capture(env: &str, got: &capture::Captured, notes: &mut Vec<String>) -> Result<()> {
    for (key, value) in &got.secrets {
        secrets::set(env, key, value)?;
    }
    let mut state = State::load(env)?;
    state.vars.extend(got.vars.clone());
    // 取り直した値の古い期限は必ず捨てる。`extend` だけだと、期限を返さないログインで
    // 過去の `expires_at` が残り続け、再ログインしても「期限切れ」と言われ続ける。
    // 自律で動くエージェントはそこで無限に往復する。
    for name in got.secrets.keys().chain(got.vars.keys()) {
        state.expires_at.remove(name);
    }
    state.expires_at.extend(got.expires_at.clone());
    state.save(env)?;

    let mut names: Vec<&str> = got.secrets.keys().map(String::as_str).collect();
    names.extend(got.vars.keys().map(String::as_str));
    if !names.is_empty() {
        // 値は出さない。名前だけで「入った」ことは分かる。
        notes.push(format!("captured({env}): {}", names.join(", ")));
    }
    Ok(())
}

fn render(
    res: &crate::dump::ResponseRecord,
    dump_path: Option<&std::path::Path>,
    common: &CommonArgs,
    redactor: &Redactor,
    palette: &Palette,
) -> Result<()> {
    if let Some(expr) = &common.pick {
        let body = body_as_json(res).context("--pick はレスポンスが JSON のときだけ使えます")?;
        let found = pick::pick(&body, expr)?;
        // マスクを外すのは**単一の値**を取りに行ったときだけ。`--pick '.token'` を
        // 成り立たせるための例外であって、`--pick '$'` で本文まるごとを無マスクで
        // 引き出すための穴ではない。オブジェクトや配列が返ってきたら通常どおり落とす。
        let found: Vec<serde_json::Value> = if found.iter().all(|v| {
            !matches!(
                v,
                serde_json::Value::Object(_) | serde_json::Value::Array(_)
            )
        }) {
            found
        } else {
            found.iter().map(|v| redactor.json(v)).collect()
        };
        if found.is_empty() {
            // 空を黙って返すと「値が空文字だった」と区別がつかない。
            eprintln!(
                "{}",
                palette.dim(&format!("`{expr}` に一致する値はありません"))
            );
        } else {
            println!("{}", pick::render(&found));
        }
        return Ok(());
    }

    // 以降の表示はマスク済みの複製を使う。
    let redacted = dump::redact_response(res, redactor);
    let res = &redacted;

    if common.shape {
        let body = body_as_json(res).context("--shape はレスポンスが JSON のときだけ使えます")?;
        println!("{}", output::status_line(res, palette));
        if let Some(path) = dump_path {
            println!("{} {}", palette.key("dump:"), paths::tildify(path));
        }
        let depth = common.depth.unwrap_or(shape::DEFAULT_MAX_DEPTH);
        println!("{}", shape::of_with_depth(&body, depth).render());
        return Ok(());
    }

    let text = match common.format.resolve() {
        Format::Pretty => output::pretty(res, dump_path, palette),
        Format::Digest => output::digest(res, dump_path, common.head_lines(), palette),
        Format::Json => output::machine(res, dump_path),
        Format::Auto => unreachable!("resolve 済み"),
    };
    println!("{text}");
    Ok(())
}

fn exit_for(status: u16, fail: bool) -> Outcome {
    Outcome {
        code: if fail && status >= 400 { 1 } else { 0 },
    }
}

fn body_as_json(res: &crate::dump::ResponseRecord) -> Result<serde_json::Value> {
    match &res.body {
        crate::dump::BodyRecord::Json { value } => Ok(value.clone()),
        _ => bail!("レスポンス本文が JSON ではありません。本文はダンプで確認してください"),
    }
}

// -------------------------------------------------------------------- 保存系

fn save(a: &SaveArgs) -> Result<Outcome> {
    let last = LastInvocation::load()?
        .ok_or_else(|| anyhow!("保存できるリクエストがありません。先に 1 回送信してください"))?;

    if !last.redacted.is_empty() {
        // 直書きの値を保存すると、平文の秘匿値が設定ファイルに残る。
        bail!(
            "{} に値が直書きされているため保存しません。`ailo secret set <env> <名前>` で預けたうえで `{{{{名前}}}}` を使う形に書き換えてから、もう一度送信して保存してください",
            last.redacted.join(", ")
        );
    }

    let capture: BTreeMap<String, String> = a
        .captures
        .iter()
        .map(|s| vars::parse_assignment(s))
        .collect::<Result<_>>()?;

    for name in &a.secrets {
        if !capture.contains_key(name) {
            bail!("`--secret {name}` に対応する `--capture {name}=<式>` がありません");
        }
    }

    // `ailo new` と同じ排他に入れる。片方だけロックしても、もう片方との
    // 同時実行で書き込みが消える。
    let _lock = crate::config::lock_config()?;
    let mut reqs = Requests::load()?;
    reqs.put(
        &a.name,
        SavedRequest {
            method: last.method,
            url: last.url,
            items: last.items,
            raw: last.raw,
            form: last.form,
            capture,
            secret: a.secrets.clone(),
        },
    );
    reqs.save()?;
    println!("保存しました: {}", a.name);
    Ok(OK)
}

fn list_requests() -> Result<Outcome> {
    let reqs = Requests::load()?;
    if reqs.requests.is_empty() {
        // どの workspace を見て空なのかが出ていないと、`.ailo` を置いた瞬間に
        // 一覧が空になった理由が分からない。
        println!("保存済みリクエストはありません（{}）", where_we_are());
        return Ok(OK);
    }
    let p = Palette::detect();
    for (name, r) in &reqs.requests {
        let captured = if r.capture.is_empty() {
            String::new()
        } else {
            let names: Vec<&str> = r.capture.keys().map(String::as_str).collect();
            format!("  {}", p.dim(&format!("capture: {}", names.join(", "))))
        };
        println!("{:<20} {:<6} {}{}", name, r.method, r.url, captured);
    }
    Ok(OK)
}

/// 使える環境の名前。設定にあるものと、秘匿値の索引にあるものを合わせる。
pub fn environment_names() -> Result<Vec<String>> {
    let cfg = Config::load()?;
    let mut names: Vec<String> = cfg.environments().iter().map(|s| s.to_string()).collect();
    for e in secrets::Index::load()?.environments() {
        if !names.iter().any(|n| n == e) {
            names.push(e.to_string());
        }
    }
    names.sort();
    Ok(names)
}

fn list_envs() -> Result<Outcome> {
    let cfg = Config::load()?;
    let p = Palette::detect();
    let default = cfg.default_env.clone().unwrap_or_default();
    let mut names: Vec<String> = cfg.environments().iter().map(|s| s.to_string()).collect();
    for e in secrets::Index::load()?.environments() {
        if !names.iter().any(|n| n == e) {
            names.push(e.to_string());
        }
    }
    if names.is_empty() {
        // 手でファイルを開かせない。それを無くすために `config` を足した。
        println!("環境はまだありません。`ailo config set -e <名前> base_url <URL>` で作れます");
        return Ok(OK);
    }
    names.sort();
    for name in names {
        let mark = if name == default { "*" } else { " " };
        let base = cfg
            .env_config(Some(&name))
            .vars
            .get("base_url")
            .cloned()
            .unwrap_or_default();
        println!("{mark} {:<12} {}", name, p.dim(&base));
    }
    Ok(OK)
}

/// 既定の環境を切り替える。
///
/// **存在しない名前は弾く。** `prd` を `prod` と打ち間違えて通ると、以降の
/// リクエストが全部 `base_url` 未解決で落ちる。原因が切り替えにあるとは気づきにくい。
fn use_env(name: &str) -> Result<Outcome> {
    ensure_env_exists(name)?;
    edit_config(|doc| config_edit::set(doc, &["default_env".to_string()], name))?;
    // 切り替え後の既定を必ず表示する。書き換えたことが目で見えないと確認のために
    // もう 1 コマンド叩くことになる。
    println!("既定の環境: {name}");
    Ok(OK)
}

/// 既定にしてよい環境かどうか。
///
/// **`env use` と `config set default_env` の両方から通す。** 片方だけに置くと、
/// フルパスを勧めているエージェントのほうが検証の無い経路を通ることになる。
/// 打ち間違いをそのまま通すと、以降のリクエストが全部未解決の変数で落ちる。
fn ensure_env_exists(name: &str) -> Result<()> {
    crate::config::validate_env_name(name)?;
    let cfg = Config::load()?;
    let mut known: Vec<String> = cfg
        .environments()
        .iter()
        .map(|s| s.to_string())
        .chain(
            secrets::Index::load()?
                .environments()
                .iter()
                .map(|s| s.to_string()),
        )
        .collect();
    if known.iter().any(|k| k == name) {
        return Ok(());
    }
    if known.is_empty() {
        bail!(
            "環境 `{name}` はありません。まず `ailo config set -e {name} base_url <URL>` で作ってください"
        );
    }
    known.sort();
    known.dedup();
    bail!("環境 `{name}` はありません。あるのは: {}", known.join(", "))
}

/// 設定ファイルを読み、渡された変更を適用し、`Config` として読めることを確かめてから書く。
///
/// 検証を通さずに書くと、`config set` の 1 回で以降すべてのリクエストが
/// 「設定を読めません」で落ちる状態になりうる。
fn edit_config(change: impl FnOnce(&mut toml_edit::DocumentMut) -> Result<()>) -> Result<()> {
    // 「読む → 変える → 書き戻す」全体を排他する。取らないと、同時に走った
    // `config set` が互いの結果を捨て合い、しかも全部が成功として終わる。
    let _lock = crate::config::lock_config()?;
    let path = Config::path()?;
    let text = Config::read_text()?;
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .with_context(|| format!("{} が TOML として読めません", paths::tildify(&path)))?;
    change(&mut doc)?;
    let updated = doc.to_string();
    config_edit::check(&updated)?;

    // **ロックだけに頼らない。** ロックは置き去り対策として古いものを奪うので、
    // I/O が詰まって処理が長引いたプロセスから奪ってしまう可能性が残る。奪われた側が
    // 黙って上書きしないよう、書く直前に「読んだときから変わっていないこと」を見る。
    if Config::read_text()? != text {
        bail!(
            "書き込む直前に {} が別のプロセスに書き換えられました。何も変更していません。やり直してください",
            paths::tildify(&path)
        );
    }
    Config::write_text(&updated)
}

fn config_command(c: &ConfigCommand) -> Result<Outcome> {
    match c {
        ConfigCommand::Edit => edit_in_editor(),
        ConfigCommand::Set { env, key, value } => {
            let path = config_edit::resolve_path(env.as_deref(), key)?;
            if path == ["default_env"] {
                ensure_env_exists(value)?;
            }
            edit_config(|doc| config_edit::set(doc, &path, value))?;
            println!("{} = {}", path.join("."), value);
            Ok(OK)
        }
        ConfigCommand::Get { env, key } => {
            let path = config_edit::resolve_path(env.as_deref(), key)?;
            let doc = load_document()?;
            let found = config_edit::flatten(&doc, &path);
            match found.as_slice() {
                [] => {
                    // **空文字が入っていた場合と区別できる形で終える。** stderr だけに
                    // 書いても、`$(ailo config get k)` と終了コードしか見ない
                    // エージェントには届かない。`git config` と同じく 1 で終える。
                    eprintln!(
                        "{}",
                        Palette::detect().dim(&format!("`{}` はありません", path.join(".")))
                    );
                    return Ok(NOT_FOUND);
                }
                values => {
                    for (_, v) in values {
                        println!("{v}");
                    }
                }
            }
            Ok(OK)
        }
        ConfigCommand::Unset { env, key } => {
            let path = config_edit::resolve_path(env.as_deref(), key)?;
            let mut removed = false;
            edit_config(|doc| {
                removed = config_edit::unset(doc, &path)?;
                Ok(())
            })?;
            if !removed {
                eprintln!(
                    "{}",
                    Palette::detect().dim(&format!("`{}` はありません", path.join(".")))
                );
                return Ok(NOT_FOUND);
            }
            println!("消しました: {}", path.join("."));
            Ok(OK)
        }
        ConfigCommand::List { env } => {
            let prefix: Vec<String> = match env {
                Some(e) => {
                    crate::config::validate_env_name(e)?;
                    vec!["env".to_string(), e.to_string()]
                }
                None => Vec::new(),
            };
            let doc = load_document()?;
            for (key, value) in config_edit::flatten(&doc, &prefix) {
                println!("{key}={value}");
            }
            Ok(OK)
        }
    }
}

fn load_document() -> Result<toml_edit::DocumentMut> {
    let path = Config::path()?;
    Config::read_text()?
        .parse()
        .with_context(|| format!("{} が TOML として読めません", paths::tildify(&path)))
}

/// `$EDITOR`(無ければ `$VISUAL`、それも無ければ `vi`)で 1 ファイルを開く。
///
/// 空文字は「設定されていない」と同じに扱う。素通しにすると一時ファイル自体を
/// 実行しようとして "Permission denied" になり、原因が分からない。
fn run_editor(path: &std::path::Path) -> Result<std::process::ExitStatus> {
    let editor = ["VISUAL", "EDITOR"]
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|v| !v.trim().is_empty()))
        .unwrap_or_else(|| "vi".to_string());

    std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sh")
        .arg(path)
        .status()
        .with_context(|| format!("エディタを起動できません: {editor}"))
}

/// 編集用の一時ファイルを 0600 で作る。
///
/// 中身は設定や定義の複製なので、本体を 0600 で書いておきながらここが 0644 では、
/// 編集している間だけ同じ内容が誰にでも読める状態になる。
fn write_scratch(path: &std::path::Path, text: &str) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("{} を作れません", paths::tildify(path)))?;
    f.write_all(text.as_bytes())?;
    Ok(())
}

/// 保存済みリクエストの名前として使える文字。
///
/// 一時ファイル名の一部になるので、素通しにすると置き場所の外を指せる。
/// いまは接頭辞のおかげで実害が出ていないが、**たまたま守られている**状態を残さない。
fn validate_request_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(())
    } else {
        bail!("名前 `{name}` は使えません。英数字と `-` `_` `.` だけで指定してください")
    }
}

/// 定義として成り立っているか。**`ailo save` と同じ秘匿値のガードを通す。**
///
/// `save` は直書きされた `Authorization:` や `password=` を見つけると保存を拒む。
/// `new` だけが通してしまうと、既存の安全境界を新しい入口が迂回することになる。
fn check_definition(req: &SavedRequest) -> Result<()> {
    if req.url.trim().is_empty() {
        bail!("`url` が空です");
    }
    for secret in &req.secret {
        if !req.capture.contains_key(secret) {
            bail!("`secret` の `{secret}` に対応する `[capture]` がありません");
        }
    }

    let found = literal_secrets(req);
    if !found.is_empty() {
        bail!(
            "秘匿値が直接書かれています({})。平文のファイルには残せません。\n`ailo secret set <環境> <キー>` に預けて `{{{{<キー>}}}}` で参照してください",
            found.join(", ")
        );
    }
    Ok(())
}

/// `ailo new` の雛形。**コメントで書き方を示す。** ヘルプを読み直させない。
const TEMPLATE: &str = r#"# 送らずに登録するリクエスト。保存すると `ailo run <名前>` で実行できる。
method = "GET"
url = "{{base_url}}/path"

# 足す item。`Name: 値`(ヘッダ) / `key=値`(フィールド) / `key==値`(クエリ)
items = []

# ボディを文字列で直接送るとき
# raw = '{"name": "taro"}'

# capture のうちキーチェーンへ入れるもの(名前だけ)
secret = []

# レスポンスから変数へ束縛する式
[capture]
# access_token = ".data.token"
"#;

/// 送らずにリクエストを定義する。
///
/// `ailo save` は**直前に送ったリクエスト**しか保存できない。先に定義してから送りたい
/// (Postman がやっていること)ので、雛形を `$EDITOR` で開いて登録できるようにする。
/// 既にある名前なら、その定義を開いて直す。
fn new_request(name: &str) -> Result<Outcome> {
    edit_saved(name)?;
    Ok(OK)
}

/// `$EDITOR` で保存済みリクエストの定義を開き、読めたら書き戻す。
///
/// `ailo new` と TUI の `e` が**同じ実装を通る**。TUI 側に編集器を作らないのは、
/// 二重実装になった瞬間、秘匿値のガードや編集中の衝突検出が片方だけ古くなるため。
pub fn edit_saved(name: &str) -> Result<()> {
    validate_request_name(name)?;

    let existing = Requests::load()?.get(name).cloned();
    let initial = match &existing {
        Some(req) => toml::to_string_pretty(req)?,
        None => TEMPLATE.to_string(),
    };

    let dir = paths::config_dir()?;
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!("new.{name}.{}.toml", std::process::id()));
    write_scratch(&tmp, &initial)?;

    let status = run_editor(&tmp)?;
    let edited = std::fs::read_to_string(&tmp)?;
    if !status.success() {
        if edited == initial {
            let _ = std::fs::remove_file(&tmp);
            bail!("エディタが異常終了しました。何も登録していません");
        }
        bail!(
            "エディタが異常終了しました。何も登録していません。\n書いたものは {} に残してあります",
            paths::tildify(&tmp)
        );
    }

    let req: SavedRequest = toml::from_str(&edited).map_err(|e| {
        anyhow!(
            "{e}\n定義として読めませんでした。書いたものは {} に残してあります",
            paths::tildify(&tmp)
        )
    })?;
    check_definition(&req).map_err(|e| {
        anyhow!(
            "{e}\n書いたものは {} に残してあります",
            paths::tildify(&tmp)
        )
    })?;

    {
        // 保存済みリクエストも「読む → 変える → 書き戻す」なので、設定と同じ排他に入れる。
        let _lock = crate::config::lock_config()?;
        let mut reqs = Requests::load()?;
        // **開いてから保存するまでの間に同じ名前が変わっていたら上書きしない。**
        // ロックは書き込み 1 回を守るだけで、編集中の変更は防げない。
        if reqs.get(name).cloned() != existing {
            bail!(
                "編集している間に `{name}` が書き換えられました。上書きしていません。\n書いたものは {} に残してあります",
                paths::tildify(&tmp)
            );
        }
        reqs.put(name, req);
        reqs.save()?;
    }
    let _ = std::fs::remove_file(&tmp);

    let what = if existing.is_some() {
        "直しました"
    } else {
        "登録しました"
    };
    println!("{what}: {name} ({})", crate::workspace::current().label());
    Ok(())
}

/// 編集した定義を保存する。**TUI の編集器から使う。**
///
/// `ailo new` と同じ検証（`check_definition`＝秘匿値の直書きガードを含む）と、
/// 同じ衝突検出（開いてから保存するまでに書き換わっていたら上書きしない）を通す。
/// TUI 側に書き込みを持たせると、この 2 つが片方だけ古くなる。
pub fn save_edited(name: &str, opened_from: &SavedRequest, edited: SavedRequest) -> Result<()> {
    validate_request_name(name)?;
    check_definition(&edited)?;

    // 保存済みリクエストも「読む → 変える → 書き戻す」なので、設定と同じ排他に入れる。
    let _lock = crate::config::lock_config()?;
    let mut reqs = Requests::load()?;
    if reqs.get(name) != Some(opened_from) {
        bail!("編集している間に `{name}` が書き換えられました。上書きしていません");
    }
    reqs.put(name, edited);
    reqs.save()
}

/// いまの workspace を添える 1 行。
///
/// **自動で切り替わる以上、いまどこを見ているかが出ていないと動機と逆になる。**
/// `.ailo` を置いた瞬間に `ailo ls` が空になっても、理由がどこにも出ない。
fn where_we_are() -> String {
    format!("workspace: {}", crate::workspace::current().label())
}

/// `$EDITOR` で設定を開く。
///
/// **編集は一時ファイルで行い、読めることを確かめてから本体に書く。** 直接開かせると、
/// 保存した瞬間に壊れた設定が正本になる。壊れていたら一時ファイルの場所を伝えて、
/// 書いたものを捨てさせない。
fn edit_in_editor() -> Result<Outcome> {
    let dir = paths::config_dir()?;
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!("config.edit.{}.toml", std::process::id()));
    write_scratch(&tmp, &Config::read_text()?)?;

    let before = Config::read_text()?;
    let status = run_editor(&tmp)?;
    let edited = std::fs::read_to_string(&tmp)?;

    if !status.success() {
        // **書いたものは、書き換えられていれば残す。** `vim` の `:cq`、エディタの
        // クラッシュ、接続断はどれも「保存済み・非ゼロ終了」になる。ここで消すと、
        // 唯一「人の手作業が消える」経路になる。
        if edited == before {
            let _ = std::fs::remove_file(&tmp);
            bail!("エディタが異常終了しました。設定は変更していません");
        }
        bail!(
            "エディタが異常終了しました。設定は変更していません。\n編集内容は {} に残してあります",
            paths::tildify(&tmp)
        );
    }

    if let Err(e) = edited
        .parse::<toml_edit::DocumentMut>()
        .map_err(anyhow::Error::from)
        .and_then(|_| config_edit::check(&edited))
    {
        // 書いたものは消さない。直して `ailo config edit` をやり直せる。
        bail!(
            "{e}\n編集したものは {} に残してあります",
            paths::tildify(&tmp)
        );
    }

    // 書く直前に排他を取り、読んだときから変わっていないことを確かめる。
    // エディタは何分も開きっぱなしになるので、その間の `config set` を
    // 黙って巻き戻さない。
    let _lock = crate::config::lock_config()?;
    if Config::read_text()? != before {
        bail!(
            "編集している間に設定が書き換えられました。上書きしていません。\n編集内容は {} に残してあります",
            paths::tildify(&tmp)
        );
    }

    Config::write_text(&edited)?;
    let _ = std::fs::remove_file(&tmp);
    println!("保存しました: {}", paths::tildify(&Config::path()?));
    Ok(OK)
}

fn secret(c: &SecretCommand) -> Result<Outcome> {
    match c {
        SecretCommand::Set { env, key } => {
            // 値は引数で受けない。argv は同一ユーザーの他プロセスから見える。
            let value = read_secret_from_stdin()?;
            if value.is_empty() {
                bail!("値が空です");
            }
            secrets::set(env, key, &value)?;
            println!("保存しました: {env}/{key}");
        }
        SecretCommand::Ls { env } => {
            let index = secrets::Index::load()?;
            let envs: Vec<String> = match env {
                Some(e) => vec![e.clone()],
                None => index.environments().iter().map(|s| s.to_string()).collect(),
            };
            if envs.is_empty() {
                println!("保存された秘匿値はありません");
            }
            for e in envs {
                // 値は絶対に出さない。名前だけで足りる。
                println!("{e}: {}", index.keys(&e).join(", "));
            }
        }
        SecretCommand::Rm { env, key } => {
            secrets::remove(env, key)?;
            println!("削除しました: {env}/{key}");
        }
    }
    Ok(OK)
}

fn read_secret_from_stdin() -> Result<String> {
    use std::io::{BufRead, IsTerminal, Read};
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        // 端末では 1 行だけ読む。EOF まで待つと Enter を押しても返らず、
        // 「入力して Enter」という案内が嘘になる。
        eprint!("値を入力して Enter (入力は画面に表示されます): ");
        let mut line = String::new();
        stdin
            .lock()
            .read_line(&mut line)
            .context("標準入力から値を読めません")?;
        return Ok(line.trim_end_matches(['\n', '\r']).to_string());
    }
    // パイプ経由(`printf ... | ailo secret set`)では、末尾改行だけを落として全部使う。
    let mut buf = String::new();
    stdin
        .lock()
        .read_to_string(&mut buf)
        .context("標準入力から値を読めません")?;
    Ok(buf.trim_end_matches(['\n', '\r']).to_string())
}

// ---------------------------------------------------------------------- 参照系

fn log(a: &LogArgs) -> Result<Outcome> {
    let entries = dump::read_index(a.limit)?;
    if entries.is_empty() {
        println!("ダンプはまだありません");
        return Ok(OK);
    }
    let p = Palette::detect();
    for (i, e) in entries.iter().enumerate() {
        let name = e.name.as_deref().unwrap_or("-");
        println!(
            "{:>3}  {}  {:>6}  {:<6} {}  {}  {}",
            i + 1,
            p.status(e.status, &e.status.to_string()),
            p.dim(&format!("{}ms", e.ms)),
            e.method,
            e.url,
            p.dim(name),
            p.dim(&e.dump),
        );
    }
    Ok(OK)
}

fn show(a: &ShowArgs) -> Result<Outcome> {
    let path = match a.target.parse::<usize>() {
        Ok(n) if n >= 1 => {
            let entries = dump::read_index(n)?;
            let entry = entries
                .get(n - 1)
                .with_context(|| format!("{n} 件目のダンプはありません"))?;
            dump::dump_path(entry)?
        }
        _ => {
            // ダンプ置き場の外へ出さない。このツールの主利用者はエージェントで、
            // レスポンスの中身に誘導されてパスを組み立てうる。任意のファイルを
            // 標準出力に流せる口を残さない。
            let dir = paths::dumps_dir()?;
            let name = std::path::Path::new(&a.target);
            let unsafe_component = name
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)));
            if unsafe_component {
                bail!(
                    "`{}` は指定できません。`ailo log` に出るファイル名か、新しいものからの番号で指定してください",
                    a.target
                );
            }
            dir.join(name)
        }
    };
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("{} を読めません", paths::tildify(&path)))?;
    print!("{content}");
    Ok(OK)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fail_flag_turns_error_statuses_into_a_non_zero_exit() {
        assert_eq!(exit_for(500, true).code, 1);
        assert_eq!(exit_for(404, true).code, 1);
        assert_eq!(exit_for(200, true).code, 0);
    }

    #[test]
    fn without_the_flag_an_error_status_still_exits_zero() {
        // curl と同じ既定。本文を読ませたい場面で落とさない。
        assert_eq!(exit_for(500, false).code, 0);
    }

    #[test]
    fn non_json_bodies_are_rejected_for_pick_and_shape() {
        let res = crate::dump::ResponseRecord {
            status: 200,
            status_text: "OK".into(),
            headers: Default::default(),
            body: crate::dump::BodyRecord::Text { text: "hi".into() },
            bytes: 2,
            ms: 1,
        };
        assert!(body_as_json(&res).is_err());
    }

    #[test]
    fn a_literal_token_in_a_sensitive_header_is_caught() {
        let r = Redactor::new(true);
        assert_eq!(
            blocking_name_of("Authorization: Bearer s3cr3t-token-value", &r).as_deref(),
            Some("Authorization")
        );
    }

    #[test]
    fn a_literal_secret_in_a_body_field_is_caught() {
        // ヘッダだけを見ていたときは、これが requests.toml に平文で残った。
        let r = Redactor::new(true);
        assert_eq!(
            blocking_name_of("password=hunter2-and-more", &r).as_deref(),
            Some("password")
        );
        assert_eq!(
            blocking_name_of("token=stg-token-abcdefgh", &r).as_deref(),
            Some("token")
        );
    }

    #[test]
    fn a_templated_secret_is_fine_to_save() {
        let r = Redactor::new(true);
        assert!(blocking_name_of("Authorization: Bearer {{access_token}}", &r).is_none());
        assert!(blocking_name_of("password={{password}}", &r).is_none());
    }

    #[test]
    fn ordinary_items_never_block_saving() {
        let r = Redactor::new(true);
        for raw in ["X-Trace: abc", "name=taro", "limit==50", "age:=30"] {
            assert!(blocking_name_of(raw, &r).is_none(), "{raw}");
        }
    }

    #[test]
    fn recording_drops_the_value_and_keeps_only_the_name() {
        let recipe = Recipe {
            method: "POST".into(),
            url: "https://example.com/login".into(),
            items: vec![
                "password=hunter2-and-more".into(),
                "email=a@example.com".into(),
            ],
            raw: None,
            form: false,
            name: None,
            capture_spec: BTreeMap::new(),
            secret_names: Vec::new(),
        };
        // record_last はファイルに書くので、ここでは同じ選別ロジックだけを確かめる。
        let r = Redactor::new(true);
        let kept: Vec<&String> = recipe
            .items
            .iter()
            .filter(|i| blocking_name_of(i, &r).is_none())
            .collect();
        assert_eq!(kept, vec!["email=a@example.com"]);
    }

    #[test]
    fn an_expired_value_stops_the_request_before_it_is_sent() {
        let now = OffsetDateTime::now_utc();
        let state = State {
            vars: Default::default(),
            expires_at: BTreeMap::from([("access_token".into(), "2020-01-01T00:00:00Z".into())]),
        };
        let err = check_expiry(&state, &["access_token".into()], now)
            .unwrap_err()
            .to_string();
        assert!(err.contains("access_token"), "{err}");
        assert!(err.contains("login"), "次にやることを示していない: {err}");
    }

    #[test]
    fn an_expired_value_that_is_not_referenced_does_not_block() {
        let now = OffsetDateTime::now_utc();
        let state = State {
            vars: Default::default(),
            expires_at: BTreeMap::from([("other".into(), "2020-01-01T00:00:00Z".into())]),
        };
        assert!(check_expiry(&state, &["access_token".into()], now).is_ok());
    }

    #[test]
    fn expanding_a_secret_into_a_query_produces_a_warning() {
        // URL はサーバのログ、プロキシ、Referer に残る。
        let v = Vars::from_layers(vec![Layer::new(
            "keychain",
            vars::map([("token".into(), "s3cr3t-token-value".into())]),
            true,
        )]);
        let mut warn = Vec::new();
        let item = args::parse_item("t=={{token}}").unwrap();
        expand_item(&item, &v, &mut warn).unwrap();
        assert_eq!(warn.len(), 1, "{warn:?}");
        assert!(warn[0].contains('t'), "{warn:?}");
    }

    #[test]
    fn expanding_a_secret_into_a_header_produces_no_warning() {
        // ヘッダは通常ログに残らない。ここで警告すると、正しい使い方が騒がしくなる。
        let v = Vars::from_layers(vec![Layer::new(
            "keychain",
            vars::map([("token".into(), "s3cr3t-token-value".into())]),
            true,
        )]);
        let mut warn = Vec::new();
        let item = args::parse_item("Authorization: Bearer {{token}}").unwrap();
        let expanded = expand_item(&item, &v, &mut warn).unwrap();
        assert!(warn.is_empty(), "{warn:?}");
        assert_eq!(
            expanded,
            Item::Header {
                name: "Authorization".into(),
                value: "Bearer s3cr3t-token-value".into()
            }
        );
    }

    #[test]
    fn raw_json_fields_survive_variable_expansion() {
        let v = Vars::from_layers(vec![Layer::new(
            "cli",
            vars::map([("n".into(), "42".into())]),
            false,
        )]);
        let mut warn = Vec::new();
        let item = args::parse_item(r#"ids:=["{{n}}"]"#).unwrap();
        let expanded = expand_item(&item, &v, &mut warn).unwrap();
        assert_eq!(
            expanded,
            Item::RawField {
                name: "ids".into(),
                value: serde_json::json!(["42"])
            }
        );
    }
}
