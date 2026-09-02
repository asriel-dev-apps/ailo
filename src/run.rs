//! サブコマンドの実行。

use anyhow::{bail, Context, Result};
use reqwest::Method;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::args::{self, Item};
use crate::cli::{Command, LogArgs, RequestArgs, ShowArgs};
use crate::dump::{self, Dump, Retention};
use crate::output::{self, Format, Palette};
use crate::paths;
use crate::pick;
use crate::redact::Redactor;
use crate::shape;

/// プロセスの終了コード。
pub struct Outcome {
    pub code: i32,
}

pub async fn run(command: Command) -> Result<Outcome> {
    if let Some((method, req)) = command.as_request() {
        return request(method, req).await;
    }
    match command {
        Command::Log(a) => log(&a),
        Command::Show(a) => show(&a),
        _ => unreachable!("リクエスト系は as_request で処理済み"),
    }
}

async fn request(method: &str, a: &RequestArgs) -> Result<Outcome> {
    let method = Method::from_bytes(method.as_bytes()).expect("固定のメソッド名");

    let items: Vec<Item> = a
        .items
        .iter()
        .map(|s| args::parse_item(s))
        .collect::<Result<_>>()?;

    let plan = crate::http::plan(method, &a.url, &items, a.form, a.raw.clone())?;
    let sent = crate::http::send(&plan, a.timeout).await?;

    let mut redactor = if a.no_redact {
        Redactor::disabled()
    } else {
        Redactor::new(true)
    };
    // 送った秘匿値を覚えさせる。API がリクエストヘッダを本文に反響して返す場合、
    // ヘッダ名を見るだけのマスクでは token が本文経由で素通りする。
    redactor.learn_from_headers(plan.headers.iter().map(|(k, v)| (k.as_str(), v.as_str())));

    let dump_path = if a.no_dump {
        None
    } else {
        let record = Dump {
            ts: OffsetDateTime::now_utc()
                .format(&Rfc3339)
                .unwrap_or_else(|_| "unknown".into()),
            name: None,
            env: None,
            redacted: redactor.is_enabled(),
            request: sent.request.clone(),
            response: sent.response.clone(),
        };
        Some(dump::write(&record, &redactor, &Retention::default())?.path)
    };

    let palette = Palette::detect();

    if let Some(expr) = &a.pick {
        // ここだけは生の本文を使う。値そのものを取りに行く操作なので、
        // マスクすると `--pick '.token'` が意味を失う。
        let body = body_as_json(&sent.response).with_context(|| {
            "--pick はレスポンスが JSON のときだけ使えます。本文はダンプで確認してください"
                .to_string()
        })?;
        let found = pick::pick(&body, expr)?;
        if found.is_empty() {
            // 空を黙って返すと「値が空文字だった」と区別がつかない。
            eprintln!("{}", palette.dim(&format!("`{expr}` に一致する値はありません")));
        } else {
            println!("{}", pick::render(&found));
        }
        return Ok(exit_for(sent.response.status, a.fail));
    }

    // 以降の表示はマスク済みの複製を使う。
    let redacted = dump::redact_response(&sent.response, &redactor);
    let res = &redacted;

    if a.shape {
        let body = body_as_json(res).with_context(|| {
            "--shape はレスポンスが JSON のときだけ使えます".to_string()
        })?;
        println!("{}", output::status_line(res, &palette));
        if let Some(path) = &dump_path {
            println!("{} {}", palette.key("dump:"), paths::tildify(path));
        }
        println!("{}", shape::of(&body).render());
        return Ok(exit_for(res.status, a.fail));
    }

    let text = match a.format.resolve() {
        Format::Pretty => output::pretty(res, dump_path.as_deref(), &palette),
        Format::Digest => output::digest(res, dump_path.as_deref(), a.head_lines(), &palette),
        Format::Json => output::machine(res, dump_path.as_deref()),
        Format::Auto => unreachable!("resolve 済み"),
    };
    println!("{text}");

    Ok(exit_for(res.status, a.fail))
}

fn exit_for(status: u16, fail: bool) -> Outcome {
    Outcome {
        code: if fail && status >= 400 { 1 } else { 0 },
    }
}

fn body_as_json(res: &crate::dump::ResponseRecord) -> Result<serde_json::Value> {
    match &res.body {
        crate::dump::BodyRecord::Json { value } => Ok(value.clone()),
        _ => bail!("レスポンス本文が JSON ではありません"),
    }
}

fn log(a: &LogArgs) -> Result<Outcome> {
    let entries = dump::read_index(a.limit)?;
    if entries.is_empty() {
        println!("ダンプはまだありません");
        return Ok(Outcome { code: 0 });
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
    Ok(Outcome { code: 0 })
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
        _ => paths::dumps_dir()?.join(&a.target),
    };
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("{} を読めません", paths::tildify(&path)))?;
    print!("{content}");
    Ok(Outcome { code: 0 })
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
}
