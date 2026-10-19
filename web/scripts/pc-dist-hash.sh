#!/bin/sh
# PC 版のビルド出力（dist 以下の m/ 以外）のハッシュを相対パス順に出す。web/ で実行する。
# スマホ版を足しても PC の出力が変わらないことを、変更前後の出力の diff で確かめる
find dist -path dist/m -prune -o -type f -print | LC_ALL=C sort | xargs sha256sum
