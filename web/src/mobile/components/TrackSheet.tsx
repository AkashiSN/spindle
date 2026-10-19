// 曲の詳細シート（下から出す）。半分 = 要点と主要タグ、引き上げると全部（全タグ・ファイル情報・RG・シーク）。
// - 詳細は GET /api/tracks/:id。保存後と、batch イベント・この曲を含む library イベントで取り直す
// - タグの行をタップすると修正シート（反映待ち・競合・missing は理由を出して開かない）

import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { ApiError } from '../../api/client'
import type { TrackRow, TrackWithDetail } from '../../api/types'
import { artworkUrl } from '../../lib/artwork'
import { formatTime } from '../../lib/playback'
import { generalRows, joinValues, locationRows, metadataRows, type PropRow } from '../../lib/properties'
import { getTrack } from '../api'
import { audioSpec, editBlockReason, formatSize } from '../track'
import type { PreviewPlayer } from '../usePreviewPlayer'
import { useEventSubscription } from '../useEvents'
import { BottomSheet } from './BottomSheet'
import { TagEditSheet } from './TagEditSheet'

/** 半分の高さで出す主要タグ */
const MAIN_KEYS = ['TITLE', 'ARTIST', 'ALBUM']
/** 全体で出すファイル情報（generalRows / locationRows のキー。この順） */
const FILE_KEYS = ['codec', 'bit_depth', 'sample_rate', 'channels', 'bitrate', 'duration', 'size', 'path']
const FILE_LABEL: Record<string, string> = {
  codec: '形式',
  bit_depth: 'ビット深度',
  sample_rate: '周波数',
  channels: 'チャンネル',
  bitrate: 'ビットレート',
  duration: '長さ',
  size: 'サイズ',
  path: 'パス',
}

function titleOf(r: TrackRow): string {
  return r.title || (r.rel_path.split('/').pop() ?? r.rel_path)
}

function valueText(row: PropRow): string {
  return row.value.kind === 'text' ? row.value.text : ''
}

export function TrackSheet({
  trackId,
  initial,
  editKey,
  player,
  onClose,
  onEdit,
  onEditClose,
}: {
  trackId: number
  /** 一覧で選んだ行（詳細が届くまでの表示に使う） */
  initial: TrackRow | null
  /** 修正シートを開いているタグのキー（history.state.edit） */
  editKey: string | null
  player: PreviewPlayer
  onClose: () => void
  onEdit: (key: string) => void
  onEditClose: () => void
}) {
  const [track, setTrack] = useState<TrackWithDetail | null>(null)
  const [loadError, setLoadError] = useState<string | null>(null)
  const [notice, setNotice] = useState<string | null>(null)
  const [expanded, setExpanded] = useState(false)

  const reload = useCallback(() => {
    getTrack(trackId)
      .then((t) => {
        setTrack(t)
        setLoadError(null)
        if (editBlockReason(t) == null) setNotice(null)
      })
      .catch((e: unknown) =>
        setLoadError(e instanceof ApiError ? `読み込みに失敗しました（${e.code}）` : '読み込みに失敗しました'),
      )
  }, [trackId])
  useEffect(() => reload(), [reload])
  useEventSubscription({
    // 保存直後は反映待ち。バッチが進んだら取り直して値と反映待ちの表示を最新にする
    onBatch: () => reload(),
    onLibrary: (e) => {
      if (e.kind === 'bulk' || e.track_ids.includes(trackId)) reload()
    },
    onOpen: (reconnect) => {
      if (reconnect) reload()
    },
  })

  const row: TrackRow | null = track ?? (initial?.id === trackId ? initial : null)
  const tags = useMemo(() => (track ? metadataRows([track.detail]) : []), [track])
  const files = useMemo(() => {
    if (!track) return []
    const details = new Map([[track.id, track.detail]])
    const all = [...generalRows([track], details), ...locationRows([track], details)]
    return FILE_KEYS.flatMap((k) => {
      const r = all.find((x) => x.key === k)
      return r ? [{ ...r, label: FILE_LABEL[k] ?? r.label }] : []
    })
  }, [track])
  const rgText = useMemo(() => {
    if (!track) return ''
    const r = generalRows([track], new Map([[track.id, track.detail]])).find((x) => x.key === 'rg')
    return r ? valueText(r) : ''
  }, [track])

  const isCurrent = player.current?.id === trackId
  const playing = isCurrent && player.playing

  const tapTag = (key: string) => {
    if (!track) return
    const reason = editBlockReason(track)
    if (reason) {
      setNotice(reason)
      return
    }
    setNotice(null)
    onEdit(key)
  }

  const togglePlay = () => {
    if (!row) return
    if (isCurrent) player.toggle()
    else player.play(row)
  }

  const editing = editKey != null && track ? tags.find((t) => t.key === editKey) : undefined
  const shownTags = expanded ? tags : MAIN_KEYS.flatMap((k) => tags.filter((t) => t.key === k))
  const spec = track ? audioSpec(track.detail) : null
  const sub = row ? [row.artist_display, row.album].filter((v): v is string => !!v).join(' · ') : ''

  return (
    <>
      <BottomSheet label="曲の詳細" expanded={expanded} onExpandedChange={setExpanded} onClose={onClose}>
        {row == null ? (
          <p className={loadError ? 'm-error m-sheet-pad' : 'm-placeholder'}>{loadError ?? '読み込み中…'}</p>
        ) : (
          <div className="m-track-sheet">
            <div className="m-track-head">
              {row.artwork_hash ? (
                <img className="m-track-art" src={artworkUrl(row.artwork_hash, 256)} alt="" decoding="async" />
              ) : (
                <span className="m-track-art m-art-empty" aria-hidden />
              )}
              <div className="m-track-titles">
                <div className="m-track-title">{titleOf(row)}</div>
                <div className="m-row-sub">{sub}</div>
              </div>
              <button
                type="button"
                className="m-play"
                aria-label={playing ? '一時停止' : '再生'}
                onClick={togglePlay}
                disabled={row.missing_since != null}
              >
                {playing ? '⏸' : '▶'}
              </button>
            </div>

            <div className="m-badges">
              <span className="m-badge">{row.codec.toUpperCase()}</span>
              {spec && <span className="m-badge">{spec}</span>}
              {row.rg != null && <span className="m-badge m-badge-rg">RG</span>}
              {row.pending_batch_id != null && <span className="m-badge m-badge-warn">反映待ち</span>}
              {row.conflict_batch_id != null && <span className="m-badge m-badge-warn">競合</span>}
              {row.missing_since != null && <span className="m-badge m-badge-warn">見つからない</span>}
            </div>

            {isCurrent && player.error != null && <p className="m-error m-sheet-pad">{player.error}</p>}
            {notice != null && <p className="m-notice">{notice}</p>}
            {loadError != null && track != null && <p className="m-error m-sheet-pad">{loadError}</p>}

            {expanded && <h2 className="m-sheet-h">タグ</h2>}
            <div className="m-props">
              {shownTags.map((t) => (
                <button key={t.key} type="button" className="m-prop m-prop-edit" onClick={() => tapTag(t.key)}>
                  <span className="m-prop-key">{t.key}</span>
                  <span className="m-prop-val">{valueText(t) || <span className="m-empty">（空）</span>}</span>
                </button>
              ))}
            </div>

            {!expanded && track && (
              <p className="m-track-format">
                {row.codec.toUpperCase()} · {formatSize(track.detail.size)}
              </p>
            )}

            {expanded && track && (
              <>
                <h2 className="m-sheet-h">
                  再生
                  {isCurrent && player.codec != null && <span className="m-codec">{player.codec} で再生中</span>}
                </h2>
                <SeekBar player={player} active={isCurrent} fallbackDuration={(row.duration_ms ?? 0) / 1000} />

                <h2 className="m-sheet-h">ファイル</h2>
                <dl className="m-props">
                  {files.map((f) => (
                    <div key={f.key} className="m-prop">
                      <dt className="m-prop-key">{f.label}</dt>
                      <dd className="m-prop-val">{valueText(f)}</dd>
                    </div>
                  ))}
                </dl>

                <h2 className="m-sheet-h">ReplayGain</h2>
                <p className="m-sheet-pad m-rg">{rgText || '未計測'}</p>
              </>
            )}

            <div className="m-sheet-actions">
              {/* Task 8 で普通のプレイリストを選んで末尾に足す */}
              <button type="button" disabled>
                プレイリストに追加
              </button>
            </div>
          </div>
        )}
      </BottomSheet>
      {editing && track && (
        <TagEditSheet
          key={`${track.id}:${editing.key}`}
          trackId={track.id}
          tagKey={editing.key}
          label={editing.key}
          initial={joinValues(track.detail.tags[editing.key] ?? [])}
          onClose={onEditClose}
          onSaved={() => {
            onEditClose()
            reload()
          }}
        />
      )}
    </>
  )
}

/** シーク付きの再生バー。ドラッグ中は位置を手元に持ち、離したときに 1 回だけシークする（変換ストリームは読み直しになるため） */
function SeekBar({
  player,
  active,
  fallbackDuration,
}: {
  player: PreviewPlayer
  active: boolean
  fallbackDuration: number
}) {
  const [scrub, setScrub] = useState<number | null>(null)
  // pointerup と touchend の両方が来ても 1 回だけシークする
  const scrubRef = useRef<number | null>(null)
  const duration = active && player.duration > 0 ? player.duration : fallbackDuration
  const position = active ? player.position : 0
  const value = scrub ?? Math.min(position, duration)
  const change = (v: number) => {
    scrubRef.current = v
    setScrub(v)
  }
  const commit = () => {
    const v = scrubRef.current
    scrubRef.current = null
    setScrub(null)
    if (v != null && active) player.seek(v)
  }
  return (
    <div className="m-seek">
      <input
        type="range"
        min={0}
        max={Math.max(duration, 1)}
        step={1}
        value={value}
        disabled={!active}
        aria-label="再生位置"
        onChange={(e) => change(Number(e.target.value))}
        onPointerUp={commit}
        onTouchEnd={commit}
        onKeyUp={commit}
        onBlur={commit}
      />
      <span className="m-seek-time">
        {formatTime(value)} / {formatTime(duration)}
      </span>
    </div>
  )
}
