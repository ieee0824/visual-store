# vstore CLI v2

```text
vstore [--store PATH] COMMAND
```

`--store` > `VSTORE_ROOT` > `$CWD/.visual-store`。上位ディレクトリは探索しない。
`--help`・`--version`以外はstdoutにUTF-8 JSON一件。正常は`{schema_version:2,ok:true,data:{...}}`、異常は`{schema_version:2,ok:false,error:{code,message,retryable}}`。

## コマンド

| コマンド | 役割・引数 |
| --- | --- |
| `init` | 未作成または空の専用ディレクトリを初期化。正常storeはIDを維持 |
| `put --file PATH` | PNG登録。`--run`、`--stream`、`--label`、`--note`、複数の`--tag`、`--captured-at RFC3339`、`--operation-id`、`--keep-source`、`--compression-level 0..9` |
| `info REF` | 寸法、メモ、タグ、ハッシュ、保存量、圧縮情報。完全性検査ではない |
| `features REF` | 保存済みhash・寸法・sizeと、同じrun/streamの直前frameとのpixel一致。画像を展開・表示しない |
| `list [--run RUN] [--limit N] [--cursor CURSOR]` | 新しい登録順。既定20件、最大100件。stdoutは最大16 KiBで、byte上限時は指定件数未満でもcursorを返す。次ページも同じrun条件を使う |
| `judgment add REF ...` | 外部判定を追加。inline JSON value、`--json FILE`、`--stdin`に対応 |
| `judgment list REF [--kind K] [--producer P]` | 画像のjudgmentを新しい順に取得 |
| `judgment search [--kind K] [--producer P] [--value JSON] [--confidence-below N]` | store全体のjudgmentを検索 |
| `get REF [--variant stored\|source] [--output PATH]` | PNGをコピーして絶対pathを返す。既定はexports配下。画像表示は行わない |
| `get-frame --run RUN [--stream default] --frame N [--variant stored\|source] [--output PATH]` | frame索引から一枚だけPNGへ復元。segmentと復号範囲を返す |
| `pack --run RUN [--stream STREAM] [--codec vp9] [--segment-frames 2..128] [--dry-run]` | 完了した列を検証済み不変segmentへ圧縮。不利ならPNGを維持。AV1は未実装で明示エラー |
| `prune --dry-run\|--apply` | 検証済みretired PNGだけを明示整理。保存やpackの副作用では実行しない |
| `verify [--report NEW_FILE]` | PNGと共有segmentの整合性検査。要約と最大20件の問題例。全問題はreportへ書く |
| `migrate --to 2 [--resume\|--restore]` | v1 storeを明示移行。中断は再開またはv1へ復元 |
| `migrate --to 3 [--resume\|--restore]` | v2 storeへjudgment schemaを追加。中断は再開またはv2へ復元 |

REFは`visual://STORE_UUID/images/IMAGE_UUID`または選択store内のIMAGE_UUID。
getとreportは出力先が既存ファイル・symlinkなら上書きせず失敗する。
sourceは登録時に`--keep-source`を付けた場合のみ取得できる。
streamはrunがある場合のみ指定でき、runあり・stream省略時は`default`。`(run, stream)`ごとに0始まりの`frame_no`を割り当てる。明示した空streamは拒否する。
get/get-frameはactive表現がPNGでもVP9でも検証済みPNGを返し、常に`displayed:false`。動画復号不能なbuildは`E_CODEC_UNAVAILABLE`を返す。
infoの`shared_representation_bytes`は複数画像で共有され得るため、画像ごとに合計しない。
Jev連携とVision escalationは[Jev adapter](jev-adapter.md)を参照。`put`や他のcore commandはJevを呼ばない。

## 再試行

同じ操作ID・同じ元バイトと登録オプションなら同じimage IDを返す。
タグは並べ替え・重複除去し、空のrun/label/noteは省略と同一視する。
時刻はUTCへ正規化し、圧縮levelは既定値を補って比較する。入力パスは比較対象外。
操作IDはstore内で一意、1〜128 UTF-8 bytes。変更した登録内容で同じIDを使うと`E_CONFLICT`。
元入力がなくなった場合、操作IDだけでは再試行できない。保存済みrefからinfo/getを使う。

## 入力上限

対応は静止PNG、非インターレース、8-bit RGB/RGBA。APNG・パレット・グレースケール・16-bitは非対応。
ファイル64 MiB、最大辺16384、総画素16777216、走査線展開128 MiB、メモリ見積もり256 MiB。
複合したメモリ見積もりで拒否するため、寸法条件だけでは受け入れを保証しない。
runとstreamは各128、label 256、note 2048 UTF-8 bytes、タグ16件・各64 bytes。さらにJSONエスケープ後のメタデータ合計を制限する。

必要な場合に限り、利用者の指定に従って有限のグローバルオプションを使う：
`--max-source-bytes`、`--max-edge`、`--max-pixels`、`--max-inflated-bytes`、`--max-memory-bytes`。
既定上限を超えて登録した画像は、get/verify時にも必要な上限を明示する。

## エラー

| 終了コード | 主なcode |
| --- | --- |
| 2 | `E_INVALID_ARGUMENT`, `E_INVALID_IMAGE`, `E_LIMIT_EXCEEDED`, `E_INVALID_CURSOR` |
| 3 | `E_NOT_FOUND`, `E_STORE_NOT_INITIALIZED`, `E_UNSUPPORTED_IMAGE`, `E_UNSUPPORTED_METADATA`, `E_SOURCE_NOT_RETAINED`, `E_CODEC_UNAVAILABLE` |
| 4 | `E_CONFLICT`, `E_BUSY`, `E_OUTPUT_EXISTS`, `E_SOURCE_CHANGED`, `E_MIGRATION_INCOMPLETE` |
| 5 | `E_INTEGRITY`, `E_SCHEMA_VERSION`, `E_STORE_MISMATCH` |
| 6 | `E_IO`, `E_PERMISSION`, `E_DISK_FULL` |

`E_BUSY`・`E_SOURCE_CHANGED`のみ`retryable:true`。無限再試行しない。
verifyで整合性問題を検出した場合は終了5、`ok:false`、errorに加えてdataに検査結果を返す。
pack/pruneの途中失敗も非ゼロ終了と、完了済み範囲を含むdataを返す。
未参照ファイルは`unreferenced_candidate`として報告するだけで、これだけなら終了0。検査中の登録かもしれないため削除しない。
