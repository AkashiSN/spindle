// Inbox の承認画面の画像の欄（② の先頭。D-86）。画像の状態で見せ方と操作を変える:
// - 全曲が同じ画像: 1 枚として見せ、差し替えは全曲に効く
// - 曲ごとに違う（YouTube）: 既定で曲ごとの画像を保つ。「画像の無い曲に入れる」「全曲を 1 枚にそろえる」は
//   明示的な操作。1 曲ずつはトラック表の画像列で差し替える
// - 画像なし: 点線の枠（クリックかドロップ）で全曲に入れる
// CD の取り込みでリリースが決まっていれば、表の画像を Cover Art Archive から自動で取って初期値に入れてある
// （D-91。「画像を外す」で外せる）。手動の「Cover Art Archive から取る」も残す。画像は配置のときに埋め込む

import { useRef, useState, type DragEvent } from 'react'
import type { ArtworkUploadState } from '../hooks/useArtworkUpload'
import {
  applyPicture,
  pictureState,
  resetPictures,
  trackPictureUrl,
  usesCaaPicture,
  type InboxDraft,
  type InboxFile,
  type InboxItem,
  type PictureTarget,
} from '../lib/inbox'

export function InboxCover({
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
  /** 下書きを直す（アップロードの待ち中の編集を消さないよう関数で渡す） */
  update: (f: (d: InboxDraft) => InboxDraft) => void
}) {
  const input = useRef<HTMLInputElement>(null)
  const [target, setTarget] = useState<PictureTarget>('all')
  const [over, setOver] = useState(false)
  const st = pictureState(files, draft)
  // 吸い出したリリースの表の画像を Cover Art Archive から自動で取り、提案に入れてある（D-91）
  const fromCaa = usesCaaPicture(item, draft)
  const n = draft.tracks.length
  const urls = draft.tracks.map((t) => trackPictureUrl(item, files.get(t.rel_path), t))
  // 代表（最頻。同数なら先）
  const counts = new Map<string, number>()
  for (const u of urls) if (u != null) counts.set(u, (counts.get(u) ?? 0) + 1)
  const cover = [...counts.entries()].sort((a, b) => b[1] - a[1])[0]?.[0] ?? null

  const pick = (t: PictureTarget) => {
    setTarget(t)
    input.current?.click()
  }
  const take = async (file: File | undefined, t: PictureTarget) => {
    if (file == null) return
    const value = await artwork.upload(file)
    if (value != null) update((d) => applyPicture(d, files, value, t))
  }
  const onDrop = (e: DragEvent) => {
    e.preventDefault()
    setOver(false)
    if (editable) void take(e.dataTransfer.files[0], 'all')
  }
  const caa = async () => {
    if (draft.release_id == null) return
    const value = await artwork.fromCaa(draft.release_id)
    if (value != null) update((d) => applyPicture(d, files, value, 'all'))
  }

  let state: string
  if (st.mode === 'mixed') {
    state = `トラックごとの画像（${n} 曲・${st.kinds} 種類${st.missing > 0 ? `・画像なし ${st.missing} 曲` : ''}）。そのまま保って配置する。1 曲ずつの差し替えは ③ の「画像」列をダブルクリック`
  } else if (st.mode === 'uniform') {
    state = fromCaa
      ? `Cover Art Archive の表の画像（吸い出したリリースから自動で取った）を配置のとき全 ${n} 曲に埋め込む`
      : st.changed > 0
        ? `選んだ画像を配置のとき全 ${n} 曲に埋め込む`
        : `ファイルの埋め込み画像（${n} 曲とも同じ）`
  } else {
    state = 'カバー画像なし。画像を選ぶかここにドロップすると、配置のとき全曲に埋め込む'
  }

  return (
    <div className="inbox-cover-row">
      {st.mode === 'mixed' ? (
        <div className="inbox-cover-strip">
          {draft.tracks.map((t, i) =>
            urls[i] != null ? (
              <img key={t.rel_path} src={urls[i]!} alt="" title={`#${t.track_no} ${t.title}`} />
            ) : (
              <span key={t.rel_path} className="inbox-nopic" title={`#${t.track_no} 画像なし`}>
                なし
              </span>
            ),
          )}
        </div>
      ) : (
        <button
          type="button"
          className={`inbox-cover-drop${cover == null ? ' empty' : ''}${over ? ' over' : ''}`}
          disabled={!editable || artwork.busy}
          aria-label="カバー画像を選ぶ（ドロップも可）"
          onClick={() => pick('all')}
          onDragOver={(e) => {
            e.preventDefault()
            setOver(true)
          }}
          onDragLeave={() => setOver(false)}
          onDrop={onDrop}
        >
          {cover != null ? <img src={cover} alt="カバー画像" /> : <span>＋ 画像を<br />ドロップ</span>}
        </button>
      )}
      <div className="inbox-cover-info">
        <div className="small">
          {st.changed > 0 && <span className="inbox-changed">変更 {st.changed} 曲</span>} {state}
        </div>
        {editable && (
          <div className="op-row">
            {st.mode === 'mixed' ? (
              <>
                {st.missing > 0 && (
                  <button type="button" disabled={artwork.busy} onClick={() => pick('missing')}>
                    画像の無い {st.missing} 曲に入れる…
                  </button>
                )}
                <button type="button" className="ghost" disabled={artwork.busy} onClick={() => pick('all')}>
                  全曲を 1 枚にそろえる…
                </button>
              </>
            ) : (
              <button type="button" disabled={artwork.busy} onClick={() => pick('all')}>
                {cover == null ? '画像を選ぶ…' : st.changed > 0 ? '別の画像を選ぶ…' : '差し替える…'}
              </button>
            )}
            {item.rip != null && draft.release_id != null && draft.release_id !== '' && st.mode !== 'mixed' && (
              <button type="button" disabled={artwork.busy} onClick={() => void caa()}>
                Cover Art Archive から取る
              </button>
            )}
            {st.changed > 0 && (
              <button type="button" className="ghost" onClick={() => update(resetPictures)}>
                {fromCaa ? '画像を外す' : '画像の変更を戻す'}
              </button>
            )}
            {artwork.busy && <span className="muted small">画像を置いている…</span>}
          </div>
        )}
        <span className="muted small">JPEG / PNG / WebP。配置のとき選んだ曲の埋め込み画像をこの 1 枚にする</span>
        {artwork.error != null && <span className="error small">{artwork.error}</span>}
      </div>
      <input
        ref={input}
        type="file"
        accept="image/jpeg,image/png,image/webp"
        hidden
        onChange={(e) => {
          void take(e.target.files?.[0], target)
          e.target.value = ''
        }}
      />
    </div>
  )
}
