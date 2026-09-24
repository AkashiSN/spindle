// Inbox の承認画面の「③ トラック」（D-86）。ライブラリの表のように全項目を列で並べ、セルをクリックで選び、
// ダブルクリック（Enter / F2）で入力欄にする。Enter で確定、Esc で取り消し、↑↓←→ で直せるセルを移る。
//
// 列の効き方（lib/inbox.ts の inboxColumns）:
// - disc / # / タイトル / アーティスト: その行だけ
// - 画像: その曲だけ画像を差し替える（ファイルを選ぶかドロップ）
// - アルバム / アルバムアーティスト / 日付: アルバム単位（どの行で直しても全行と ② に反映）
// - ファイルのタグ: その行だけ。空にするとそのタグを消す。「タグを追加」で新しいキーの列を足す
// - 長さ / codec / ファイル / 判定 / category と 🔒 のタグ: 直せない（🔒 は同一性の判定に使うタグ）
//
// disc / # / 画像 / タイトルは横スクロールしても左に残す（stickyColumns）。列は「列」メニューで隠せ、
// 隠した列は localStorage に覚える

import { useMemo, useRef, useState, type DragEvent, type KeyboardEvent, type MouseEvent, type ReactNode } from 'react'
import type { ArtworkUploadState } from '../hooks/useArtworkUpload'
import { editSession } from '../lib/editSession'
import { formatDuration } from '../lib/format'
import {
  ARTIST_JOIN,
  applyPicture,
  artistValues,
  effectiveTag,
  extraTagKeys,
  inboxColumns,
  newTagKeyProblem,
  omitsDisc,
  parseHiddenColumns,
  sameTitleLabel,
  setTrackTag,
  stickyColumns,
  tagChanged,
  trackPictureUrl,
  verdictLabel,
  type DraftTrack,
  type InboxColumn,
  type InboxDraft,
  type InboxFile,
  type InboxItem,
  type InboxSource,
} from '../lib/inbox'

type Cell = { row: number; col: string }

const GROUP_TITLES: Record<InboxColumn['group'], string> = {
  edit: 'ダブルクリックで編集（この行だけ）',
  album: 'ダブルクリックで編集（アルバム単位。全行と ② に反映）',
  file: 'ファイルから（表示のみ）',
  tag: 'ファイルのタグ。ダブルクリックで編集、空にすると消す',
}
const LOCK_TITLE =
  '曲・盤の同一性の判定に使うタグ（YouTube の二重取り込みの判定・CD の盤の識別）。書き換えると判定が壊れるので直せない'

const HIDDEN_KEY = 'inbox.columns.hidden'

/** 隠した列（利用者ごとの見た目の好みなので localStorage。読めない環境では毎回すべて出す） */
function useHiddenColumns(): [string[], (v: string[]) => void] {
  const [hidden, setHiddenState] = useState<string[]>(() => {
    try {
      return parseHiddenColumns(window.localStorage.getItem(HIDDEN_KEY))
    } catch {
      return []
    }
  })
  const setHidden = (v: string[]) => {
    setHiddenState(v)
    try {
      window.localStorage.setItem(HIDDEN_KEY, JSON.stringify(v))
    } catch {
      // 保存できなくても表示は切り替わる
    }
  }
  return [hidden, setHidden]
}

function fileName(relPath: string): string {
  const i = relPath.lastIndexOf('/')
  return i < 0 ? relPath : relPath.slice(i + 1)
}

/** 表示の文字列（編集の初期値にも使う） */
function cellText(c: InboxColumn, t: DraftTrack, d: InboxDraft, f: InboxFile | undefined): string {
  switch (c.id) {
    case 'disc':
      return String(t.disc_no)
    case 'no':
      return String(t.track_no)
    case 'title':
      return t.title
    case 'artist':
      return t.artist
    case 'album':
      return d.album
    case 'albumartist':
      return d.albumartist
    case 'date':
      return d.date ?? ''
    case 'category':
      return d.category ?? '_Unsorted'
    case 'duration':
      return f != null ? formatDuration(f.duration_ms) : ''
    case 'codec':
      return f != null ? `${f.codec}${f.lossless ? '' : '（非可逆）'}` : ''
    case 'file':
      return fileName(t.rel_path)
    default:
      return c.id.startsWith('tag:') ? effectiveTag(f, t, c.label) : ''
  }
}

export function InboxTrackGrid({
  item,
  draft,
  files,
  editable,
  artwork,
  update,
}: {
  item: InboxItem
  draft: InboxDraft
  files: ReadonlyMap<string, InboxFile>
  editable: boolean
  artwork: ArtworkUploadState
  update: (f: (d: InboxDraft) => InboxDraft) => void
}) {
  const [added, setAdded] = useState<string[]>([])
  const hasPicture = draft.tracks.some((t) => trackPictureUrl(item.id, files.get(t.rel_path), t) != null) || editable
  const hasSource = item.tracks.some((f) => f.source != null)
  const noDisc = omitsDisc(item, draft)
  const tagKeys = useMemo(
    () => [...new Set([...extraTagKeys(item.tracks, draft.tracks), ...added])].sort(),
    [item.tracks, draft.tracks, added],
  )
  const columns = useMemo(() => inboxColumns({ hasPicture, hasSource, tagKeys }), [hasPicture, hasSource, tagKeys])
  const [hidden, setHidden] = useHiddenColumns()
  const shown = columns.filter((c) => !c.hideable || !hidden.includes(c.id))
  const sticky = stickyColumns(shown)
  const editCols = editable ? shown.filter((c) => c.editable).map((c) => c.id) : []

  const [cursor, setCursor] = useState<Cell | null>(null)
  const [editing, setEditing] = useState<(Cell & { text: string }) | null>(null)
  const gridRef = useRef<HTMLDivElement>(null)
  // 1 回の編集で確定 / 取り消しは 1 回だけ（Esc の後の blur で確定しない）
  const session = useRef(editSession())
  const fileInput = useRef<HTMLInputElement>(null)
  const [picRow, setPicRow] = useState<number | null>(null)
  const refocus = () => gridRef.current?.focus()

  const colOf = (id: string) => columns.find((c) => c.id === id)
  // 「変更」の基準: ファイルのタグから作った提案（トラックは rel_path で引く）
  const proposalByPath = useMemo(() => new Map(item.proposal.tracks.map((t) => [t.rel_path, t])), [item.proposal])
  /** 提案から変わったセルか（タグ以外。タグは tagChanged）と、元の値 */
  const fieldChange = (c: InboxColumn, t: DraftTrack): string | null => {
    const o = proposalByPath.get(t.rel_path)
    if (o == null || !c.editable) return null
    const before = cellText(c, o, item.proposal, undefined)
    const now = cellText(c, t, draft, undefined)
    if (c.id === 'artist' && t.keep_artists === true) return null
    return before !== now ? before : null
  }
  const updTrack = (i: number, f: (t: DraftTrack) => DraftTrack) =>
    update((d) => ({ ...d, tracks: d.tracks.map((t, j) => (j === i ? f(t) : t)) }))

  const takePicture = async (file: File | undefined, row: number) => {
    if (file == null) return
    const value = await artwork.upload(file)
    if (value != null) update((d) => applyPicture(d, files, value, row))
  }
  const start = (cell: Cell) => {
    const c = colOf(cell.col)
    if (!editable || c == null || !c.editable) return
    setCursor(cell)
    if (c.id === 'thumb') {
      setPicRow(cell.row)
      fileInput.current?.click()
      return
    }
    session.current.start()
    setEditing({ ...cell, text: cellText(c, draft.tracks[cell.row], draft, files.get(draft.tracks[cell.row].rel_path)) })
  }
  const commit = () => {
    if (editing == null) return
    const { row, col, text } = editing
    const c = colOf(col)
    setEditing(null)
    refocus()
    if (c == null) return
    const num = () => {
      const v = Number.parseInt(text, 10)
      return Number.isFinite(v) ? v : 0
    }
    switch (col) {
      case 'disc':
        updTrack(row, (t) => ({ ...t, disc_no: num() }))
        return
      case 'no':
        updTrack(row, (t) => ({ ...t, track_no: num() }))
        return
      case 'title':
        updTrack(row, (t) => ({ ...t, title: text }))
        return
      case 'artist':
        // 直したら 1 値で書く（多値を保つのをやめる。D-70）
        updTrack(row, (t) => ({ ...t, artist: text, keep_artists: false }))
        return
      case 'album':
      case 'albumartist':
        update((d) => ({ ...d, [col]: text }))
        return
      case 'date':
        update((d) => ({ ...d, date: text.trim() === '' ? null : text.trim() }))
        return
      default:
        if (col.startsWith('tag:')) updTrack(row, (t) => setTrackTag(t, files.get(t.rel_path), c.label, text))
    }
  }
  const cancel = () => {
    setEditing(null)
    refocus()
  }
  const onKey = (e: KeyboardEvent) => {
    if (editing != null || editCols.length === 0) return
    const cur = cursor ?? { row: 0, col: editCols[0] }
    let { row, col } = cur
    const ci = Math.max(0, editCols.indexOf(col))
    if (e.key === 'ArrowDown') row = Math.min(draft.tracks.length - 1, row + 1)
    else if (e.key === 'ArrowUp') row = Math.max(0, row - 1)
    else if (e.key === 'ArrowRight') col = editCols[Math.min(editCols.length - 1, ci + 1)]
    else if (e.key === 'ArrowLeft') col = editCols[Math.max(0, ci - 1)]
    else if ((e.key === 'Enter' || e.key === 'F2') && cursor != null) {
      e.preventDefault()
      start(cursor)
      return
    } else if (e.key === 'Escape') {
      setCursor(null)
      return
    } else return
    e.preventDefault()
    setCursor({ row, col })
  }

  const [newKey, setNewKey] = useState('')
  const keyProblem = newKey.trim() === '' ? null : newTagKeyProblem(newKey, tagKeys)
  const addTag = () => {
    const k = newKey.trim().toUpperCase()
    if (k === '' || newTagKeyProblem(k, tagKeys) != null) return
    setAdded((a) => [...a, k])
    setHidden(hidden.filter((h) => h !== `tag:${k}`))
    setNewKey('')
    if (draft.tracks.length > 0) {
      session.current.start()
      setEditing({ row: 0, col: `tag:${k}`, text: '' })
    }
  }

  const place = (id: string) => {
    const s = sticky.get(id)
    if (s == null) return {}
    return {
      stickyClass: `inbox-sticky${s.last ? ' inbox-sticky-last' : ''}`,
      style: { left: s.left, width: s.width, minWidth: s.width, maxWidth: s.width },
    }
  }

  const cell = (c: InboxColumn, t: DraftTrack, i: number): ReactNode => {
    const f = files.get(t.rel_path)
    const p = place(c.id)
    const isCur = cursor?.row === i && cursor.col === c.id
    const cls = (extra: string) =>
      [extra, p.stickyClass, isCur ? 'inbox-cursor' : '', c.group === 'album' && c.editable ? 'inbox-album-col' : '']
        .filter(Boolean)
        .join(' ')
    const handlers = c.editable && editable
      ? {
          onClick: (e: MouseEvent) => {
            if (editing != null) return
            if (e.detail >= 2) start({ row: i, col: c.id })
            else setCursor({ row: i, col: c.id })
          },
        }
      : {}
    if (editing?.row === i && editing.col === c.id) {
      const num = c.id === 'disc' || c.id === 'no'
      return (
        <td key={c.id} className={cls('inbox-editing')} style={p.style}>
          <input
            type={num ? 'number' : 'text'}
            min={num ? 1 : undefined}
            aria-label={`${t.rel_path} の${c.label}`}
            autoFocus
            value={editing.text}
            onChange={(e) => setEditing({ ...editing, text: e.target.value })}
            onKeyDown={(e) => {
              const a = session.current.key(e.key)
              if (a != null) e.preventDefault()
              if (a === 'commit') commit()
              else if (a === 'cancel') cancel()
            }}
            onBlur={() => {
              if (session.current.blur() === 'commit') commit()
            }}
          />
        </td>
      )
    }
    switch (c.id) {
      case 'thumb': {
        const url = trackPictureUrl(item.id, f, t)
        const changed = t.picture != null
        return (
          <td
            key={c.id}
            className={cls(`inbox-thumb-cell${changed ? ' inbox-cell-changed' : ''}`)}
            style={p.style}
            title={editable ? 'ダブルクリック（かドロップ）でこの曲の画像を差し替える' : undefined}
            onDragOver={(e: DragEvent) => editable && e.preventDefault()}
            onDrop={(e: DragEvent) => {
              e.preventDefault()
              if (editable) void takePicture(e.dataTransfer.files[0], i)
            }}
            {...handlers}
          >
            {url != null ? <img className="inbox-thumb" src={url} alt="" /> : <span className="inbox-nopic small">なし</span>}
          </td>
        )
      }
      case 'title': {
        const same = f == null ? null : sameTitleLabel(f)
        const before = fieldChange(c, t)
        return (
          <td
            key={c.id}
            className={cls(`inbox-title-cell${before != null ? ' inbox-cell-changed' : ''}`)}
            style={p.style}
            title={before != null ? `ファイルの値: ${before || '（空）'}` : undefined}
            {...handlers}
          >
            {t.title.trim() === '' ? <span className="muted">（空）</span> : t.title}
            {same != null && (
              <div
                className="inbox-same-title small"
                title="同じ曲を二重に取り込もうとしている可能性がある（Cover / Live ver. は別曲。承認は止めない）"
              >
                ⚠ {same}
              </div>
            )}
          </td>
        )
      }
      case 'artist': {
        const values = f == null ? [] : artistValues(f)
        const multi = values.length > 1
        const keep = multi && t.keep_artists === true
        const before = fieldChange(c, t)
        return (
          <td
            key={c.id}
            className={cls(`inbox-artist-cell${before != null ? ' inbox-cell-changed' : ''}`)}
            title={before != null ? `ファイルの値: ${before || '（空）'}` : undefined}
            {...handlers}
          >
            {keep ? values.join(ARTIST_JOIN) : t.artist.trim() === '' ? <span className="muted">{draft.albumartist}</span> : t.artist}
            {multi && (
              <label className="inbox-keep small" onClick={(e) => e.stopPropagation()}>
                <input
                  type="checkbox"
                  checked={keep}
                  disabled={!editable}
                  onChange={(e) =>
                    updTrack(i, (x) => ({ ...x, keep_artists: e.target.checked, artist: values.join(ARTIST_JOIN) }))
                  }
                />{' '}
                多値を保つ
              </label>
            )}
          </td>
        )
      }
      case 'verdict':
        return (
          <td key={c.id} className="inbox-verdict">
            {f?.source != null && <VerdictCell source={f.source} />}
          </td>
        )
      default: {
        // 宛先に合わせてディスク番号を書かない件は、disc 列を空で見せる（値は下書きに残る）
        const text = c.id === 'disc' && noDisc ? '' : cellText(c, t, draft, f)
        const tagKey = c.id.startsWith('tag:') ? c.label : null
        const before = tagKey == null ? fieldChange(c, t) : null
        const changed = tagKey != null ? tagChanged(f, t, tagKey) : before != null
        const removed = changed && text === ''
        return (
          <td
            key={c.id}
            className={cls(
              [
                c.editable ? '' : 'muted',
                c.id === 'duration' || c.id === 'disc' || c.id === 'no' ? 'num' : '',
                c.group === 'tag' || c.id === 'file' ? 'inbox-tag-cell' : '',
                changed ? 'inbox-cell-changed' : '',
              ]
                .filter(Boolean)
                .join(' '),
            )}
            style={p.style}
            title={
              c.locked
                ? LOCK_TITLE
                : changed
                  ? `ファイルの値: ${tagKey != null ? (f == null ? '' : effectiveTag(f, {}, tagKey)) || '（無し）' : before || '（空）'}`
                  : c.id === 'file'
                    ? t.rel_path
                    : text || undefined
            }
            {...handlers}
          >
            {removed ? <span className="muted">（消す）</span> : text}
          </td>
        )
      }
    }
  }

  return (
    <>
      <div className="inbox-tracks-head inbox-grid-tools">
        <span className="spacer" />
        <ColumnMenu columns={columns} hidden={hidden} onChange={setHidden} />
      </div>
      <div
        className="inbox-tracks-wrap"
        ref={gridRef}
        tabIndex={0}
        onKeyDown={onKey}
        role="grid"
        aria-label="トラック（セルをダブルクリックで編集）"
      >
        <table className="cd-tracks inbox-tracks">
          <thead>
            <tr>
              {shown.map((c) => {
                const p = place(c.id)
                return (
                  <th
                    key={c.id}
                    className={[p.stickyClass, `inbox-col-${c.group}`, c.group === 'album' && c.editable ? 'inbox-album-col' : '']
                      .filter(Boolean)
                      .join(' ')}
                    style={p.style}
                    title={c.locked ? LOCK_TITLE : c.editable ? GROUP_TITLES[c.group] : GROUP_TITLES.file}
                  >
                    {c.locked ? '🔒 ' : ''}
                    {c.label}
                  </th>
                )
              })}
            </tr>
          </thead>
          <tbody>
            {draft.tracks.map((t, i) => {
              const f = files.get(t.rel_path)
              return (
                <tr key={t.rel_path} className={f?.source != null && f.source.verdict !== 'ok' ? 'inbox-unmatched' : ''}>
                  {shown.map((c) => cell(c, t, i))}
                </tr>
              )
            })}
          </tbody>
        </table>
      </div>
      <div className="inbox-legend small muted">
        <span>
          <i className="inbox-sw" />
          この行だけ
        </span>
        <span>
          <i className="inbox-sw inbox-sw-album" />
          アルバム単位（全行）
        </span>
        <span>
          <i className="inbox-sw inbox-sw-changed" />
          変更したセル
        </span>
        <span>灰色は直せない（🔒 は同一性に使うタグ）</span>
      </div>
      {editable && (
        <div className="op-row inbox-addtag">
          <input
            type="text"
            className="small"
            aria-label="追加するタグのキー"
            placeholder="新しいタグ（例: LYRICIST）"
            value={newKey}
            onChange={(e) => setNewKey(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter') {
                e.preventDefault()
                addTag()
              }
            }}
          />
          <button type="button" disabled={newKey.trim() === '' || keyProblem != null} onClick={addTag}>
            タグを追加
          </button>
          {keyProblem != null && <span className="error small">{keyProblem}</span>}
        </div>
      )}
      {artwork.error != null && <p className="error small">{artwork.error}</p>}
      <input
        ref={fileInput}
        type="file"
        accept="image/jpeg,image/png,image/webp"
        hidden
        onChange={(e) => {
          if (picRow != null) void takePicture(e.target.files?.[0], picRow)
          e.target.value = ''
        }}
      />
    </>
  )
}

/** 判定バッジ。判定できなかったものは行を開くと message（ルールの足し方）と URL が読める */
function VerdictCell({ source }: { source: InboxSource }) {
  const v = verdictLabel(source)
  const badge = <span className={`badge ${v.ok ? 'inbox-verdict-ok' : 'inbox-verdict-ng'}`}>{v.text}</span>
  if (v.ok && source.url == null) return badge
  return (
    <details className="inbox-verdict-details">
      <summary>{badge}</summary>
      {source.message != null && <pre className="inbox-verdict-message small">{source.message}</pre>}
      {source.url != null && (
        <a className="small" href={source.url} target="_blank" rel="noreferrer">
          {source.url}
        </a>
      )}
      {source.channel != null && <div className="muted small">channel: {source.channel}</div>}
    </details>
  )
}

/** 列の表示の切り替え（隠せる列だけ。グループごと） */
function ColumnMenu({
  columns,
  hidden,
  onChange,
}: {
  columns: InboxColumn[]
  hidden: string[]
  onChange: (v: string[]) => void
}) {
  const hideable = columns.filter((c) => c.hideable)
  const toggle = (id: string, on: boolean) => onChange(on ? hidden.filter((h) => h !== id) : [...hidden, id])
  const tagIds = hideable.filter((c) => c.group === 'tag').map((c) => c.id)
  const titles: Record<InboxColumn['group'], string> = {
    edit: 'この行だけ',
    album: 'アルバム単位',
    file: 'ファイルから（表示のみ）',
    tag: 'ファイルのタグ',
  }
  return (
    <details className="inbox-column-menu">
      <summary className="small">列</summary>
      <div className="inbox-column-menu-body small">
        {(['edit', 'album', 'file', 'tag'] as const).map((g) => {
          const cs = hideable.filter((c) => c.group === g)
          if (cs.length === 0) return null
          return (
            <fieldset key={g}>
              <legend>{titles[g]}</legend>
              {g === 'tag' && (
                <div className="op-row">
                  <button type="button" className="ghost" onClick={() => onChange(hidden.filter((h) => !tagIds.includes(h)))}>
                    すべて出す
                  </button>
                  <button
                    type="button"
                    className="ghost"
                    onClick={() => onChange([...hidden.filter((h) => !tagIds.includes(h)), ...tagIds])}
                  >
                    すべて隠す
                  </button>
                </div>
              )}
              {cs.map((c) => (
                <label key={c.id}>
                  <input type="checkbox" checked={!hidden.includes(c.id)} onChange={(e) => toggle(c.id, e.target.checked)} />{' '}
                  {c.locked ? '🔒 ' : ''}
                  {c.label}
                </label>
              ))}
            </fieldset>
          )
        })}
      </div>
    </details>
  )
}
