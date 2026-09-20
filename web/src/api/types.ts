// サーバの JSON 形（SPEC §9）。Rust 側の Serialize と 1 対 1

export type Verification =
  | 'verified_ar'
  | 'verified_ctdb'
  | 'mismatch'
  | 'unverifiable'
  | 'not_attempted'

export type Derived = { codec: string; stale_tags: boolean }
/** 系統ごとの Derived（SPEC §7.6、D-75）。opus = 配布ビュー・D バッジ、aac = Apple 向け。無い系統は null */
export type DerivedVariants = { opus: Derived | null; aac: Derived | null }
/** FLAC 健全性チェックの結果（P1-5）。stale は検査後に audio_version が進んだ */
export type FlacCheck = {
  status: 'ok' | 'md5_missing' | 'decode_error'
  checked_at: number | null
  stale: boolean
  error: string | null
}
export type HiresCheck = {
  status: 'ok' | 'upsampled' | 'padded' | 'both' | 'inconclusive' | 'decode_error'
  checked_at: number | null
  stale: boolean
  error: string | null
  /** 計測値。計測しなかった側は null（SPEC §7.10） */
  cutoff_hz: number | null
  cliff_db: number | null
  effective_bits: number | null
}
export type RgValues = { track_gain: number; track_peak: number; album_gain: number | null; album_peak: number | null }

export type TrackRow = {
  id: number
  title: string | null
  artist_display: string | null
  album: string | null
  albumartist: string | null
  track_no: number | null
  disc_no: number | null
  date: string | null
  category: string | null
  duration_ms: number | null
  codec: string
  lossless: boolean
  verification: Verification
  rg_scanned_at: number | null
  rg_written_at: number | null
  /** 解析値（-18 LUFS 基準の dB）。未解析なら null */
  rg: RgValues | null
  derived: DerivedVariants
  flac_check: FlacCheck | null
  hires_check: HiresCheck | null
  pending_batch_id: number | null
  conflict_batch_id: number | null
  duplicate_group: string | null
  hardlink: boolean
  missing_since: number | null
  rel_path: string
  /** 所属アルバム（アルバムアートの解決に使う。P1-12） */
  album_id: number | null
  /** トラック自身の埋め込み画像の SHA-256（hex）。無ければ null（D-61） */
  artwork_hash: string | null
}

/** `GET /api/tracks/:id`（セッションあり）が行に加えて返す詳細（D-58）。一覧には付かない */
export type TrackDetail = {
  /** キー（大文字）→ 値の並び（多値は idx 順）。`track_tags` 全部 */
  tags: Record<string, string[]>
  size: number
  /** epoch 秒 */
  mtime: number
  sample_rate: number | null
  bit_depth: number | null
  channels: number | null
  bitrate: number | null
  /** hex（小文字）。無ければ null */
  audio_md5: string | null
  original_codec: string | null
  added_at: number
}

export type TrackWithDetail = TrackRow & { detail: TrackDetail }

export type TrackPage = {
  items: TrackRow[]
  next_cursor: string | null
  total: number
}

export type AlbumRow = {
  id: number
  rel_dir: string
  category: string | null
  albumartist: string | null
  album: string | null
  date: string | null
  original_date: string | null
  edition: string | null
  mb_release_id: string | null
  disc_count: number | null
  artwork_id: number | null
  artwork_hash: string | null
  track_count: number
  duration_ms: number
  missing_since: number | null
  /** album gain を計算・書き出しする album か（D-74） */
  album_gain: boolean
}

export type JobState = 'queued' | 'running' | 'done' | 'failed' | 'cancelled'

export type Job = {
  id: number
  type: string
  state: JobState
  progress: number | null
  done: number | null
  total: number | null
  attempts: number
  max_attempts: number
  last_error: string | null
  run_after: number | null
  edit_batch_id: number | null
  created_at: number
  started_at: number | null
  finished_at: number | null
}

export type JobSummary = {
  running: number
  queued: number
  pending_ops: number
  failed: number
}

/** 種別ごとの件数（全件の集計） */
export type TypeCounts = {
  queued: number
  running: number
  failed: number
}

export type JobList = {
  /** 上限付き（実行中が先頭、待ちは取り出し順、終端は新しい順） */
  items: Job[]
  summary: JobSummary
  concurrency: Record<string, number>
  /** CPU 系（rg / transcode / flaccheck / hirescheck）が共有する並列予算（= コア数。D-73） */
  cpu_budget: number
  /** 種別ごとの queued / running / failed。`items` は上限付きなのでこちらで数える */
  by_type: Record<string, TypeCounts>
}

// SSE /api/events
export type JobEvent = {
  id: number
  state: JobState
  progress: number | null
  done: number | null
  total: number | null
}
export type BatchEvent = {
  id: number
  state: string
  applied: number
  conflict: number
  failed: number
}
export type LibraryEvent =
  | { kind: 'ids'; scan_run_id: number; track_ids: number[] }
  | { kind: 'bulk'; scan_run_id: number }
export type ResyncEvent = { skipped: number }
export type PlaylistEvent = { playlist_ids: number[] }

export type ErrorBody = { error: string; message?: string }

// 一括編集（SPEC §9、P0-10）
export type TagChange = { old: string[] | null; new: string[] | null }
export type PreviewItem = { id: number; changes: Record<string, TagChange> }
export type PreviewResponse = {
  selection_token: string
  count: number
  changed: number
  unchanged: number
  pending_excluded: number
  items: PreviewItem[]
}
export type ApplyResponse = { batch_id: number; affected: number }
export type PendingConflict = { error: 'pending'; count: number; track_ids: number[] }

// 編集履歴（SPEC §9、P0-12）
export type BatchState = 'prepared' | 'applying' | 'applied' | 'partial' | 'failed' | 'cancelled'
export type OpKind = 'tags' | 'rename' | 'delete' | 'archive' | 'md5'
export type OpResult = 'pending' | 'applied' | 'skipped_conflict' | 'failed' | 'superseded'

export type HistoryItem = {
  id: number
  created_at: number
  description: string | null
  kind: OpKind | null
  state: BatchState
  affected: number | null
  applied: number
  conflict: number
  failed: number
  reverts_batch_id: number | null
  reverted_by: number | null
  finished_at: number | null
  reverted_at: number | null
}
export type HistoryList = { items: HistoryItem[] }

export type EditView = { old: unknown; new: unknown }
export type OpView = {
  id: number
  track_id: number
  kind: OpKind
  result: OpResult
  error: string | null
  rel_path: string | null
  edits: Record<string, EditView>
  /** skipped_conflict の op だけ: 編集キーの現在値 */
  current?: Record<string, unknown>
}
export type HistoryDetail = HistoryItem & { ops: OpView[] }
export type RevertResponse = { batch_id: number; affected: number; conflict: number }

// ---------------------------------------------------------------- プレイリスト（P1-6）

export type PlaylistExport = { profile: string; out_path: string; exported_at: number | null }
export type Playlist = {
  id: number
  name: string
  kind: 'manual' | 'smart'
  /** smart のルール原文（docs/DSL.md）。manual は null */
  rule_source: string | null
  auto_export: boolean
  created_at: number
  updated_at: number
  track_count: number
  missing_count: number
  duration_ms: number
  exports: PlaylistExport[]
}
export type PlaylistList = { items: Playlist[] }
export type AppendResponse = { added: number; skipped: number }
export type ExportResponse = { out_path: string; count: number; skipped_missing: number; stale_tags: number }
/** `GET /api/playlists/:id/fb2k_query`: foobar Autoplaylist のクエリとソートパターン、変換できなかった指定 */
export type Fb2kQuery = { query: string; sort: string | null; notes: string[] }
export type ImportCandidate = { path: string; size: number }
export type ImportResponse = { playlist: Playlist; matched: number; duplicates: number; unresolved: string[] }
export type RulePreview = { count: number; ast: unknown }
export type RefreshResponse = { count: number; changed: boolean }

/** 書き出しプロファイル名（`export_profiles` の seed。CRUD は P1-8） */
export const EXPORT_PROFILES = ['internal', 'foobar', 'android'] as const
export type ExportProfileName = (typeof EXPORT_PROFILES)[number]
