## 4. アーキテクチャ

```
┌─────────────────────────────────────────────────┐
│  Browser (React SPA, rust-embed で同梱)          │
└───────────────┬─────────────────────────────────┘
                │ REST + SSE
┌───────────────▼─────────────────────────────────┐
│  axum HTTP server                               │
│   ├── API handlers                              │
│   ├── streaming (Range / on-the-fly transcode)  │
│   └── SSE event bus                             │
├─────────────────────────────────────────────────┤
│  Job scheduler (tokio)                          │
│   ├── queue: rip(並列1) / scan / rg /           │
│   │          transcode / tagwrite / verify /    │
│   │          normalize / thumbnail              │
│   └── recovery on startup                       │
├─────────────────────────────────────────────────┤
│  Domain                                         │
│   scanner / tagger / pathgen / rg /             │
│   cdrom / accuraterip / musicbrainz / parser    │
├─────────────────────────────────────────────────┤
│  SQLite (WAL)          外部プロセス:             │
│                        cd-paranoia / ffmpeg /   │
│                        yt-dlp                   │
└─────────────────────────────────────────────────┘
```

### 技術スタック

| 領域 | 選定 | 備考 |
|---|---|---|
| 言語 | Rust (edition 2021+) | タグ書き込み・EBU R128・デコードが揃う唯一の現実解 |
| HTTP | axum + tokio + tower-http | |
| DB | rusqlite (bundled) + FTS5 | 書き込みは単一コネクション、読み込みはプール |
| タグ | lofty | FLAC / Opus / MP4 / WAV を一貫 API |
| デコード | symphonia | FLAC/ALAC/AAC/WAV。**Opus のデコードは非対応 → ffmpeg 経由**（demux は Opus も symphonia で可。`audio_fp` はこれを使う） |
| ラウドネス | ebur128 | libebur128 の純 Rust 移植 |
| 正規表現 | fancy-regex | 後方参照・先読みが必要（ytmusic パーサ） |
| HTTP client | reqwest | MusicBrainz / CTDB |
| FFT | rustfft | 偽ハイレゾ検出（P3、任意） |
| ファイル操作 | rustix | `openat2(RESOLVE_BENEATH \| RESOLVE_NO_SYMLINKS)`、dirfd 基準の open / rename |
| フロント | React + TypeScript + TanStack Table/Virtual | |
| 外部バイナリ | cd-paranoia, cdrdao, ffmpeg, yt-dlp, flac | コンテナイメージに同梱 |

---

