---
name: ailo
description: HTTP API を叩くときに使う。curl の代わり。レスポンス全文をコンテキストに入れずに済ませるための道具で、本文はファイルへ落とし、要約・抽出・形だけを返す。API へのリクエスト、エンドポイントの動作確認、認証が要る API の呼び出し、レスポンスの構造把握、直前のリクエストの再確認に使う。
---

# ailo

**HTTP を叩くときは `curl` ではなく `ailo` を使う。**

`curl` はレスポンス全文を出力する。1 MB の JSON を一度受けただけでコンテキストの残量が削られ、
以降の推論が痩せる。`ailo` は本文をファイルへ落とし、標準出力には必要な分だけを返す。

## まず読む値の量を決める

| 知りたいこと | 使うもの |
| --- | --- |
| 特定のフィールドの値だけ | `--pick '<式>'` |
| レスポンスの構造(キー名と型) | `--shape` |
| 成否と大きさだけ | `--format json` |
| 本文をざっと見る | 既定(先頭 20 行 + ダンプの場所) |
| 本文を全部 | `--full`(**最後の手段**。まず `--pick` で足りないか考える) |

```bash
ailo get https://api.example.com/users --pick '.data.items[].id'
ailo get https://api.example.com/users --shape
ailo post https://api.example.com/users name=taro age:=30
```

引数の記法:

| 記法 | 意味 |
| --- | --- |
| `key=値` | JSON ボディの文字列フィールド |
| `key:=JSON` | JSON ボディの raw 値(数値・bool・配列・オブジェクト) |
| `key==値` | クエリパラメータ |
| `Name: 値` | ヘッダ |
| `key@パス` | multipart のファイル |

抽出式は jq 風(`.data.items[].id`)でも JSONPath(`$.data.items[*].id`)でも書ける。

## 後から本文を読む

送信のたびに完全なリクエスト / レスポンスが `~/.local/share/ailo/dumps/` に残り、
`index.jsonl` に 1 行ずつ索引が積まれる。**全文を読む前に索引で絞り込む。**

```bash
ailo log                     # 直近を新しい順に
ailo show 1                  # 直近のダンプ全体
grep '"status":500' ~/.local/share/ailo/dumps/index.jsonl
```

## 認証が要る API

秘匿値は引数に書かない。キーチェーンに預けて `{{名前}}` で参照する。

```bash
printf '<値>' | ailo secret set stg api_key     # 値は標準入力から。argv には載せない
ailo get '{{base_url}}/me' 'Authorization: Bearer {{api_key}}' --env stg
```

ログインで token を取る流れは宣言で書ける。

```bash
# 1. 一度送ってから名前を付けて保存する
ailo post '{{base_url}}/auth/login' 'password={{login_password}}' --env stg
ailo save login --capture access_token=.data.token --capture expires_in=.data.expiresIn --secret access_token

# 2. 以降 token はキーチェーンに入り、{{access_token}} で参照できる
ailo run login --env stg
ailo run me --env stg
```

期限切れの token を使おうとすると**送信前に**落ちる。その場合は `ailo run login` を先に実行する。

## 気をつけること

- **秘匿値を引数に直接書かない。** `Authorization: Bearer <生の値>` や `password=<生の値>` は
  `ailo save` に拒否される。`ailo secret set` に預けてから `{{名前}}` で参照する。
- **ダンプと画面出力では認証情報がマスクされる。** 実際の値が要るときだけ `--pick` を使う。
  `--no-redact` はダンプに生の認証情報を残すので、理由があるときだけ。
- **`--full` を反射的に使わない。** それはコンテキストにレスポンス全文を入れるということ。

## 設定

`~/.config/ailo/config.toml`

```toml
default_env = "stg"

[vars]                       # 全環境共通
api_version = "v1"

[headers]                    # 全環境共通で足すヘッダ
Accept = "application/json"

[env.stg.vars]
base_url = "https://stg.example.com"

[env.prd.vars]
base_url = "https://api.example.com"
```

変数は先に見つかったほうが勝つ: `--var` → `AILO_VAR_*` → キーチェーン →
capture した値 → `[env.<名前>.vars]` → `[vars]`。
未解決の `{{名前}}` が残ったら、送信せずにその名前を挙げて落ちる。

**ファイルを開かずに `ailo config` で書ける。** `git config` と同じく、
ファイルの構造をそのままパスで指す。エージェントは曖昧さの無いフルパスを使うとよい。

```bash
ailo config set env.stg.vars.base_url https://stg.example.com
ailo config set -e stg base_url https://stg.example.com   # 同じ場所。-e があれば env.<名前>.vars. を補う
ailo config get env.stg.vars.base_url
ailo config list -e stg                                    # 名前=値 の平らな並び
ailo config unset -e stg tenant
ailo env use stg                                           # 既定の環境を切り替える
```

秘匿らしいキー名(`token`、`api_key` など)は `config set` では弾かれる。
平文ファイルに残るため。`ailo secret set` に預けて `{{名前}}` で参照する。
`config get` / `config unset` は対象が無ければ終了コード 1(値が空文字なら 0)。

## workspace

リポジトリ直下に `.ailo`（中身は workspace 名 1 行）があれば、設定・保存済みリクエスト・
秘匿値・ダンプがその workspace のものになる。無ければ既定の置き場所。
`--workspace <名前>` で明示指定でき、そちらが勝つ。
**秘匿値は workspace をまたがない**（環境変数は `AILO_SECRET_<WS>_<ENV>_<KEY>`）。

## コマンド一覧

| | |
| --- | --- |
| `ailo <get\|post\|put\|patch\|delete\|head\|options> <URL> [item...]` | 送る |
| `ailo run <名前> [item...]` | 保存済みを実行 |
| `ailo save <名前> [--capture 名前=式] [--secret 名前]` | 直前のリクエストを保存 |
| `ailo new <名前>` | 送らずにリクエストを定義する（$EDITOR） |
| `ailo ls` / `ailo env` | 保存済み / 環境の一覧 |
| `ailo -w <名前> ...` | workspace を明示する（既定は `.ailo` を上へ辿って探す） |
| `ailo config set\|get\|unset\|list\|edit` | 設定の読み書き(`git config` 相当) |
| `ailo env use <名前>` | 既定の環境を切り替える |
| `ailo secret set\|ls\|rm <env> [キー]` | 秘匿値の出し入れ |
| `ailo log` / `ailo show <番号\|ファイル名>` | ダンプの索引 / 本体 |

終了コードは、送信できた限り 0(HTTP のステータスに関わらず)。
`--fail` を付けると 400 以上で 1 になる。引数や変数の誤りなど送信前の失敗は 2。
