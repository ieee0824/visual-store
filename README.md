# Visual Store

[日本語](README.md) | [English](README.en.md)

Visual Storeは、コーディングエージェント向けのローカルPNGストレージです。画像を不透明な`visual://`参照として保存し、CLIから小さなJSONだけを返します。必要になった画像だけをローカルファイルへ取り出せます。保存と取得では画像を表示せず、画像のバイト列を標準出力へ流しません。

MVPは、非インターレースの静止8-bit RGB/RGBA PNGに対応しています。既存のフィルター済み走査線をzlib level 6で可逆再圧縮し、検証に成功した小さい方を保存します。同一の保存blobはSHA-256で重複排除し、同じ画像が複数回現れたという記録は個別の画像レコードとして保持します。

## Rustを採用した理由

このリポジトリはRust 2024のプロジェクトとして開始しました。Rustでは、パーサーの整数演算とバッファーを安全に制限でき、単一のローカルCLIバイナリとして配布できます。PNG、zlib、SQLite、SHA-256、UUID、JSON、CLI解析には保守されたライブラリを使用しています。SQLiteはソースをバンドルしてビルドします。時間方向コーデックはsystem libvpxへ直接リンクします。実行時にFFmpeg、ImageMagick、ネットワーク接続、APIキーは使用しません。

## インストール

現在の安定版Rustツールチェーン、Cコンパイラー、`pkg-config`、libvpx開発packageが必要です。

```bash
# macOS
brew install libvpx pkg-config

# Ubuntu/Debian
sudo apt-get update
sudo apt-get install -y libvpx-dev pkg-config
```

```bash
cargo install --path . --locked
vstore --version
```

開発中は、以下の例にある`vstore`を`cargo run --locked --`へ置き換えて実行できます。

## クイックスタート

```bash
vstore --store "$PWD/.visual-store" init

vstore --store "$PWD/.visual-store" put \
  --file artifacts/render.png \
  --run ui-check-20260915-a \
  --label input-border \
  --note "後で確認するため保存。画像はまだ見ていない。"

vstore --store "$PWD/.visual-store" list --run ui-check-20260915-a --limit 20
vstore --store "$PWD/.visual-store" info 'visual://STORE_ID/images/IMAGE_ID'
vstore --store "$PWD/.visual-store" get 'visual://STORE_ID/images/IMAGE_ID'
vstore --store "$PWD/.visual-store" verify
```

`--help`と`--version`以外のコマンドは、標準出力へUTF-8のJSONを一件だけ出力します。診断結果に画像本体は含まれません。`get`は絶対ローカルパスと`displayed: false`を返します。画像の内容を確認する必要がある場合だけ、そのパスを画像表示機能へ渡してください。

storeの選択順は`--store PATH`、`VSTORE_ROOT`、`$CWD/.visual-store`です。親ディレクトリは探索しません。`.visual-store/`をGit管理に含めないでください。このリポジトリの`.gitignore`では除外済みです。

入力と完全に同じバイト列を後で取得する必要がある場合は、`put --keep-source`を指定します。通常は検証済みの可逆保存版だけを保持します。再試行可能な登録には`--operation-id`を使います。同じ操作IDを異なる元画像やメタデータで再利用すると`E_CONFLICT`になります。

完全な仕様は[CLIリファレンス](docs/cli.md)、[JSON Schema](docs/cli.schema.json)、[保存形式](docs/storage-format.md)を参照してください。

## Codex Skill

リポジトリには[Visual Store Skill](skills/visual-store/SKILL.md)を同梱しています。Codexは`.agents/skills`以下のプロジェクトSkillを読み取ります。リポジトリへコピーするか、開発中はシンボリックリンクを作成します。

```bash
mkdir -p .agents/skills
ln -s ../../skills/visual-store .agents/skills/visual-store
```

複数のリポジトリで使う場合は、`skills/visual-store`を`$HOME/.agents/skills/visual-store`へコピーします。CLIとSkillは別々にインストールします。現在のCodexはSkillの変更を自動検出します。表示されない場合はCodexを再起動してください。明示的に使う場合は`$visual-store`を指定します。配置場所と呼び出し方法は[OpenAI公式のSkillドキュメント](https://developers.openai.com/codex/skills/)に基づいています。

連携手順は`codex-cli 0.154.0`を基準に作成しました。自動テストではCLIとSkillファイルを検証しています。新しいCodexセッションでの暗黙選択と、ホスト側の画像表示機能との接続は手動確認が必要です。

## バックアップと保守

すべての`vstore`プロセスを終了し、storeディレクトリ全体をコピーしてから、コピー先で`vstore --store COPY verify`を実行してください。storeの使用中に`index.sqlite3`だけをコピーしないでください。MVPは画像レコード、blob、exports、一時的な孤立候補を自動削除しません。

storeディレクトリとファイルは、POSIX環境でそれぞれ`0700`と`0600`で作成します。既存の所有者や権限は変更しません。ネットワークファイルシステムと複数ホストからの同時利用はサポート対象外です。

## 開発

```bash
cargo test --locked --features fault-injection
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo build --locked --release
cargo run --locked --example vp9_roundtrip
```

`fault-injection` featureは、永続化境界で子プロセスを終了したり`ENOSPC`を発生させたりする隔離テスト専用です。インストール用ビルドでは有効にしないでください。

性能は画像内容とハードウェアによって変わります。[性能測定手順](docs/benchmark.md)に従い、単純なGUI、文字の多いGUI、写真を含む画面、圧縮されにくいfixtureで測定してください。OS、CPU、releaseビルド、圧縮level、キャッシュ状態、並行度も結果と一緒に記録します。

## 依存ライブラリとライセンス

Visual StoreはMITライセンスです。VP9 backendはBSD 3-Clauseのsystem libvpxを、MPL-2.0の`libvpx-native-sys` 5.0.17から呼び出します。ほかの実行時直接依存は`base64`、`chrono`、`clap`、`crc32fast`、`flate2`、`libc`、`png`、`rusqlite`、`serde`、`serde_json`、`sha2`、`uuid`です。これらはMIT、Apache-2.0または互換ライセンスで提供され、`rusqlite`はMIT、バンドルされるSQLiteはパブリックドメインです。テスト専用依存は`tempfile`と`jsonschema`です。解決済みの正確なRust crate版は`Cargo.lock`へ記録しています。採用版、可逆plane配置、system libraryの再現手順は[VP9 codec ADR](docs/adr/0001-vp9-lossless-codec.md)に記録しています。

再配布前に、対象成果物の推移的依存関係とライセンス通知をすべて監査してください。このプロジェクトはサードパーティーのソースやライセンスファイルをvendorしていません。

## 対応環境

ローカルファイルシステム上のmacOSとLinuxを対象とし、それ以外の環境では意図的にコンパイルエラーになります。CIは両OSでlibvpxを有効にしてテストします。この開発環境ではmacOS、Rust 1.95.0、libvpx 1.16.0でcodec試験を実行しました。
