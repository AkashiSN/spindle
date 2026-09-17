// サーバの JSON 形（SPEC §9）。Rust 側の Serialize と 1 対 1

export type Verification =
  | 'verified_ar'
  | 'verified_ctdb'
  | 'mismatch'
  | 'unverifiable'
  | 'not_attempted'

export type Derived = { codec: string; stale_tags: boolean }

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
  derived: Derived | null
  pending_batch_id: number | null
  conflict_batch_id: number | null
  duplicate_group: string | null
  hardlink: boolean
  missing_since: number | null
  rel_path: string
}

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

export type JobList = { items: Job[]; summary: JobSummary }

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
export type OpKind = 'tags' | 'rename' | 'delete' | 'archive'
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
