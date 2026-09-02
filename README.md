# ailo

An HTTP client for AI agents — call APIs without pouring the whole response into the context window.

AI エージェントが主に使う HTTP クライアント。人間も同じ CLI から使える。

`curl` はレスポンス全文を出力する。1 MB の JSON を一度受けただけでエージェントの
コンテキスト残量が削られ、以降の推論が痩せる。ailo は本文をファイルへ落とし、
標準出力には要約・抽出・形だけを返す。

```console
$ ailo get https://api.github.com/repos/rust-lang/rust
200 OK  418ms  6.0 KB  application/json
dump: ~/.local/share/ailo/dumps/2026-09-02T14-56-25.789554Z-dc73.json
body (first 20 lines of 138):
{
  "id": 724712,
  "name": "rust",
  ...
… 残り 118 行。全文はダンプか --full で
```

12 KB のレスポンスの構造だけを知りたいなら、値を出さずに形だけ返す。

```console
$ ailo get https://api.github.com/repos/rust-lang/rust/tags --shape
200 OK  394ms  12.0 KB  application/json
[30 items] {
  name: string
  zipball_url: string
  tarball_url: string
  commit: { sha: string, url: string }
  node_id: string
}
```

必要なフィールドだけなら、本文は一度もコンテキストに入らない。

```console
$ ailo get https://api.github.com/repos/rust-lang/rust --pick '.full_name'
rust-lang/rust
```

## インストール

```bash
cargo install --git https://github.com/asriel-dev-apps/ailo
```

ソースから:

```bash
git clone https://github.com/asriel-dev-apps/ailo
cd ailo
cargo install --path .
```

Rust 1.87 以上が要る。macOS と Linux で動く。

### Claude Code から使う

同梱の skill を入れると、エージェントが `curl` ではなく ailo を選ぶようになる。

```bash
mkdir -p ~/.claude/skills
ln -s "$PWD/skill" ~/.claude/skills/ailo
```

## 使い方

```bash
ailo get https://api.example.com/users
ailo post https://api.example.com/users name=taro age:=30
ailo get https://api.example.com/users limit==50 'X-Trace: abc'
```

| 記法 | 意味 |
| --- | --- |
| `key=値` | JSON ボディの文字列フィールド |
| `key:=JSON` | JSON ボディの raw 値(数値・bool・配列・オブジェクト) |
| `key==値` | クエリパラメータ |
| `Name: 値` | ヘッダ |
| `key@パス` | multipart のファイル |

### 出力を絞る

| フラグ | 出るもの |
| --- | --- |
| `--pick '<式>'` | 式に一致した値だけ。jq 風(`.data.items[].id`)でも JSONPath でも書ける |
| `--shape` | 値を出さず、キー名・型・配列の要素数だけ |
| `--format json` | ステータス・所要時間・大きさ・ダンプの場所の 1 行 JSON |
| `--full` | 本文全部 |

端末で実行したときは整形して全文を、パイプやエージェント経由のときはダイジェストを出す。
`--format` で明示的に上書きできる。

### 後から本文を読む

送信のたびに完全なリクエスト / レスポンスが `~/.local/share/ailo/dumps/` に残り、
`index.jsonl` に索引が積まれる。索引を絞ってから該当の 1 件だけを開ける。

```bash
ailo log                     # 直近を新しい順に
ailo show 1                  # 直近のダンプ全体
grep '"status":500' ~/.local/share/ailo/dumps/index.jsonl
```

既定では新しいものから 200 件 / 7 日ぶんを保持し、超えた分は消える。

### 環境と変数

`~/.config/ailo/config.toml`

```toml
default_env = "stg"

[vars]
api_version = "v1"

[headers]
Accept = "application/json"

[env.stg.vars]
base_url = "https://stg.example.com"

[env.prd.vars]
base_url = "https://api.example.com"
```

```bash
ailo get '{{base_url}}/users' --env prd
```

変数は先に見つかったほうが勝つ。

1. `--var 名前=値`
2. 環境変数 `AILO_VAR_<名前>`
3. キーチェーンの秘匿値
4. capture で束縛した値
5. `[env.<名前>.vars]`
6. `[vars]`

未解決の `{{名前}}` が残ったら、リクエストを送らずにその名前を挙げて落ちる。
空文字で送ってしまうと、返ってきた 401 の原因が分からなくなるため。

### 秘匿値

値は OS のセキュアストアに入る。平文ファイルには書かない。

| 環境 | 保管先 |
| --- | --- |
| macOS | Keychain(Apple 署名済みの `/usr/bin/security` 経由) |
| Linux | Secret Service(libsecret を実行時 dlopen) |
| headless / CI | 環境変数 `AILO_SECRET_<ENV>_<KEY>` |

```bash
printf '<値>' | ailo secret set stg api_key    # 値は標準入力から。argv には載せない
ailo secret ls stg                             # キー名だけ。値は出さない
ailo get '{{base_url}}/me' 'Authorization: Bearer {{api_key}}' --env stg
```

### 保存とキャプチャ

一度送ったリクエストに名前を付けて残せる。ログインで得た token を次のリクエストへ
渡す流れは、スクリプトではなく宣言で書く。

```bash
ailo post '{{base_url}}/auth/login' 'password={{login_password}}' --env stg
ailo save login \
  --capture access_token=.data.token \
  --capture expires_in=.data.expiresIn \
  --secret access_token

ailo run login --env stg      # token がキーチェーンに入る
ailo run me --env stg         # Authorization: Bearer {{access_token}} が解決される
```

`expires_in` を捕まえておくと、期限切れの token を使おうとした時点で**送信前に**落ちる。
黙って 401 を受けるより速く、原因も明確。

保存されるのは展開前のテンプレートで、`{{access_token}}` のまま残る。
`Authorization: Bearer <生の値>` や `password=<生の値>` のように秘匿値が直書きされている
リクエストは、`ailo save` が拒否する。

### マスク

ダンプにも画面出力にも、認証情報はマスクして出る。

- `Authorization`、`Cookie`、`Set-Cookie`、`X-Api-Key` などのヘッダ
- キーチェーンや capture で解決した秘匿値そのもの(本文のどこに現れても)
- `password`、`token`、`secret`、`api_key` などの名前を持つフィールドに送った値

リクエストヘッダを本文に反響して返す API があるため、ヘッダ名だけでは足りない。
送った値そのものを覚えて落としている。実際の値が要るときは `--pick` を使う。
`--no-redact` はダンプに生の認証情報を残すので、理由があるときだけ。

ダンプはリポジトリの外(`XDG_DATA_HOME`)に置き、ファイルは `0600`、ディレクトリは `0700`。

## 対応していないもの

SSE / ストリーミング応答、OAuth トークンの自動リフレッシュ、GraphQL 専用サポート、
MCP サーバモード、TUI。いずれも今後の検討対象で、恒久的な対象外ではない。

プラグイン機構と `.http` / Hurl / Bruno 互換は作らない。

## ライセンス

MIT
