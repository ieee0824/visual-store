# ADR 0001: libvpxによる可逆VP9コーデック基盤

- 状態: 採用
- 日付: 2026-09-15
- 対象: temporal codec foundation v1

## 決定

最初の時間方向コーデックには、system libraryとしてリンクするlibvpxのVP9 encoder/decoder APIを使う。Rust側は`libvpx-native-sys` 5.0.17へ正確に固定し、通常ビルドで常に有効にする。検証基準のlibvpxは公式tag `v1.16.0`（runtime version `v1.16.0`）である。実際にリンクした版とbuild configurationは`libvpx_version()`と`libvpx_build_config()`で取得でき、encoder descriptorにもruntime versionを記録する。

macOS arm64で受け入れ試験に使ったHomebrew buildのruntime configurationは`--prefix=/opt/homebrew/Cellar/libvpx/1.16.0 --disable-dependency-tracking --disable-examples --disable-unit-tests --enable-pic --enable-runtime-cpu-detect --enable-shared --enable-vp9-highbitdepth --target=arm64-darwin25-gcc`である。CIでは各runnerのconfigurationを`vp9_roundtrip` exampleのJSONへ記録する。

FFI型、可変長control呼び出し、`unsafe`、codec contextの寿命、image stride、libvpx所有packetのコピーは`src/codec/vp9.rs`だけに閉じ込める。`vpxenc`、`vpxdec`、FFmpeg CLI、`libavcodec`、`libavformat`、`libswscale`は使用しない。

## 可逆plane layout v1

通常動画向けのRGB/YUV変換は行わない。8-bit packed RGBをVP9 profile 1のfull-resolution I444へ次のように並べ替える。

| 入力sample | VP9 color stream |
| --- | --- |
| R | plane 0 |
| G | plane 1 |
| B | plane 2 |

RGBAではRGBを同じcolor streamへ格納し、alphaは別のlossless I444 streamのplane 0へ格納する。alpha streamのplane 1と2はすべて0に固定し、decode時にも0であることを検証する。premultiply、subsampling、limited-range変換、resize、denoiseは行わないため、alpha=0の画素にある非0のRGBも保持する。

descriptor version 1は次を固定する。

- codec: `vp9`
- profile: `1`（8-bit I444）
- bit depth: `8`
- color layout: `rgb_planar_i444_direct_v1`
- alpha layout: RGBではなし、RGBAでは`alpha_in_i444_plane0_v1`
- color metadata: `srgb_full_range_no_conversion`
- lossless: `true`

これはVisual Store内部の可逆なsample配置であり、一般的な動画プレイヤーへそのまま渡して正しいRGB表示になる形式ではない。container、永続化、PNG再構築情報は後続のsegment実装が担当する。

## Encoder設定とinter-frameの検証

一つのstreamの全frameを同じencoder contextへ順に渡す。`VP9E_SET_LOSSLESS=1`、quantizer 0、lag 0、1 thread、timebase 1/30、keyframe最大間隔128を用い、先頭だけを明示的なkeyframeにする。テストはpacket flagだけに依存しない。decoder APIの`vpx_codec_peek_stream_info`で圧縮済みpacket headerがnon-keyであることを確認し、そのpacketが先行frame列と同じdecoder contextなら完全復号できる一方、新規decoderへ単独で渡すと復号できないことを要求する。これにより、設定値やpacket flagの自己申告だけでなく、実際に先行decoder stateを必要とするbitstreamであることを示す。

Rust側の`SequenceEncoder`は一枚ずつsampleを受け取り、呼び出し完了後に入力
bufferを保持しない。colorとalphaは同じframe順で二つのcontextへ逐次投入する。
取得・全体検証もdecoder出力を一枚ずつ組み立て、全segment分の展開RGB/RGBAを
同時保持しない。libvpx内部の参照面はallocator-levelのhard capを設定できないため、
寸法、stream数、有限の参照面数、作業buffer、container、補助情報から保守的に
事前拒否し、実際のpeak RSSをbenchmarkで記録する。

## ビルド方式

配布物へlibvpxをvendorしない。`pkg-config`で見つかるsystem libvpxへ動的リンクする。macOSとLinuxの通常セットアップは次のとおり。

```bash
# macOS (Homebrew)
brew install libvpx pkg-config

# Ubuntu/Debian
sudo apt-get update
sudo apt-get install -y libvpx-dev pkg-config

pkg-config --modversion vpx
cargo test --locked
```

検証基準と同じlibvpx 1.16.0をsourceから再現する場合は、公式tagをcheckoutして共有libraryを構築し、その`.pc`をRust buildへ渡す。

```bash
git clone --depth 1 --branch v1.16.0 https://chromium.googlesource.com/webm/libvpx
cd libvpx
./configure \
  --prefix="$PWD/out" \
  --enable-shared \
  --enable-pic \
  --enable-runtime-cpu-detect \
  --enable-vp9-encoder \
  --enable-vp9-decoder \
  --enable-vp9-highbitdepth \
  --disable-examples \
  --disable-tools \
  --disable-docs \
  --disable-unit-tests
make -j2
make install
PKG_CONFIG_PATH="$PWD/out/lib/pkgconfig" cargo test --manifest-path ../Cargo.toml --locked
```

macOSでsource buildを使う場合は実行時にも`DYLD_LIBRARY_PATH=$PWD/out/lib`を設定する。Linuxでは`LD_LIBRARY_PATH=$PWD/out/lib`を設定する。CIはmacOSとLinuxでsystem packageを導入し、`PATH`を空にした往復exampleと、成果物の動的依存にlibvpxが存在しlibav/FFmpegが存在しないことを検査する。

## Versionとライセンス

- libvpx: 1.16.0 baseline、BSD 3-Clause
- `libvpx-native-sys`: 5.0.17固定、MPL-2.0
- Visual Storeから追加したcodec code: リポジトリ本体と同じMIT

system libvpxのbinaryを再配布する場合は、そのbinaryに対応するlibvpxのcopyright/license noticeを同梱する。`libvpx-native-sys`やその他のRust依存を再配布する場合も`Cargo.lock`で解決された版に対応するnoticeを監査する。

## AV1

AV1 backendはこの段階では実装しない。CLIは将来の選択を曖昧にしないため
`--codec av1`を構文上は受理するが、明示的な`E_CODEC_UNAVAILABLE`として拒否し、
VP9やPNGへ読み替えない。help、CLI文書、テストもこの状態を公開する。
