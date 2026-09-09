# 試すための一式

認証の要らない公開 API（GitHub と httpbin.org）だけを使う。そのまま動く。

## 入れる

既定の置き場所を汚さないよう、`demo` という workspace に入れる。

```bash
mkdir -p ~/.config/ailo/workspaces/demo
cp examples/requests.toml ~/.config/ailo/workspaces/demo/
cp examples/config.toml   ~/.config/ailo/workspaces/demo/
```

消すときは `rm -rf ~/.config/ailo/workspaces/demo`。ほかの workspace には触れない。

## 使う

```bash
ailo -w demo tui          # 画面で選んで送る
ailo -w demo ls           # 一覧
ailo -w demo run gh-issues
```

`-w demo` を毎回書きたくなければ、作業ディレクトリに `.ailo` を置く。

```bash
echo demo > .ailo
ailo tui                  # このディレクトリ以下では demo になる
```

## 入っているもの

| 名前 | 何を見るか |
| --- | --- |
| `gh-issues` | 大きなレスポンス。`--shape` の効きが分かる |
| `gh-repo` | 幅の広いオブジェクト。`--shape --depth 2` で切れる |
| `echo` | 送った item がそのまま返る。`key=値` / `key:=JSON` / ヘッダの違い |
| `echo-form` | form-urlencoded で送る |
| `echo-query` | クエリパラメータ |
| `slow` | 8 秒かかる。TUI で `Enter` のあと `Esc` を押すと打ち切れる |
| `not-found` / `server-error` | 失敗の見え方。終了コードは既定で 0（`--fail` で 1） |
| `fake-login` | capture。レスポンスの値を `{{demo_token}}` に取る |
| `use-token` | 取った値をヘッダに載せて送る。httpbin が受け取った形を返す |
| `plain-text` / `an-image` | JSON でない本文の扱い |

## ひととおり触る

```bash
ailo -w demo run gh-issues --shape              # 形だけ
ailo -w demo run gh-repo --shape --depth 2      # 深さを切る
ailo -w demo run gh-issues --pick '.[].title'   # 値だけ
ailo -w demo run echo --pick '.json'            # 送った本文が返ってくる
ailo -w demo run fake-login                     # capture が走る
ailo -w demo run use-token --pick '.headers.Authorization'
ailo -w demo log                                # ここまでの送信履歴
ailo -w demo show 1                             # 直近のダンプ本体
```

`httpbin.org` は落ちていることがある。そのときは GitHub 側だけで試せる。
