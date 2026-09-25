// Inbox の承認画面の「別のリリースから取る」（D-93）。メタデータは取り込むリリースのまま、表の画像だけを
// 同じリリースグループの別の版（初回限定盤・BD 付き等）から Cover Art Archive で取る。
// - 同じグループの版を `GET /api/cd/release-group/{id}/releases` で並べ、表の画像のある版をジャケット付きで出す
//   （ジャケットは CD 画面の候補と同じ `/api/cd/cover/{id}` の中継）。画像の無い版は数だけ
// - グループの代表画像（CAA がグループに選んだ 1 枚）
// - 別のグループに登録された版は MusicBrainz のリリースの URL か MBID を貼る
// どれも `POST /api/artwork/from-caa` で置き、呼び出し側が全曲の picture に入れる

import { useEffect, useState, type FormEvent } from 'react'
import { ApiError, apiFetch } from '../api/client'
import type { CaaTarget } from '../hooks/useArtworkUpload'
import { currentListing, groupReleaseSummary, type GroupListing, type GroupReleases } from '../lib/cd'

function message(e: unknown): string {
  if (e instanceof ApiError && e.status === 404) return 'MusicBrainz にこのリリースグループが無い'
  if (e instanceof ApiError) return e.message
  return e instanceof Error ? e.message : String(e)
}

/** 版のジャケット。取れなければ同じ寸法の空枠 */
function Thumb({ releaseId }: { releaseId: string }) {
  const [failed, setFailed] = useState(false)
  if (failed) return <span className="caa-picker-thumb empty" aria-hidden="true" />
  return (
    <img
      className="caa-picker-thumb"
      src={`/api/cd/cover/${releaseId}`}
      alt=""
      loading="lazy"
      onError={() => setFailed(true)}
    />
  )
}

export function CaaReleasePicker({
  groupId,
  releaseId,
  busy,
  pick,
  close,
}: {
  /** 取り込むリリースのグループ（無ければ一覧を出さず貼り付けだけ） */
  groupId: string | null
  /** 取り込むリリース（一覧で印を付ける） */
  releaseId: string | null
  busy: boolean
  /** 画像を置いて全曲に入れる。置けたら true */
  pick: (target: CaaTarget) => Promise<boolean>
  close: () => void
}) {
  // 結果はどのグループのものかと組で持つ（グループが変われば currentListing が loading に戻す）
  const [stored, setStored] = useState<{ groupId: string; listing: GroupListing } | null>(null)
  const listing = currentListing(stored, groupId)
  const [pasted, setPasted] = useState('')

  useEffect(() => {
    if (groupId == null) return
    const ctrl = new AbortController()
    apiFetch<GroupReleases>(`/api/cd/release-group/${encodeURIComponent(groupId)}/releases`, { signal: ctrl.signal })
      .then((value) => setStored({ groupId, listing: { state: 'ok', value } }))
      .catch((e: unknown) => {
        if (!ctrl.signal.aborted) setStored({ groupId, listing: { state: 'error', message: message(e) } })
      })
    return () => ctrl.abort()
  }, [groupId])

  const take = async (target: CaaTarget) => {
    if (await pick(target)) close()
  }
  const onPaste = (e: FormEvent) => {
    e.preventDefault()
    const v = pasted.trim()
    if (v !== '') void take({ release_id: v })
  }

  const withFront = listing.state === 'ok' ? listing.value.releases.filter((r) => r.front) : []
  const without = listing.state === 'ok' ? listing.value.releases.length - withFront.length : 0
  const omitted = listing.state === 'ok' ? listing.value.total - listing.value.releases.length : 0

  return (
    <div className="caa-picker">
      <div className="caa-picker-head">
        <strong className="small">表の画像だけを別のリリースから取る</strong>
        <span className="muted small">アルバム名などのメタデータは今のリリースのまま</span>
        <button type="button" className="ghost" onClick={close}>
          閉じる
        </button>
      </div>
      {groupId != null && (
        <>
          {listing.state === 'loading' && <div className="muted small">同じリリースグループの版を MusicBrainz から取っている…</div>}
          {listing.state === 'error' && <div className="error small">{listing.message}</div>}
          {listing.state === 'ok' && (
            <>
              {withFront.length === 0 ? (
                <div className="muted small">同じリリースグループに表の画像のある版が無い</div>
              ) : (
                <ul className="caa-picker-list">
                  {withFront.map((r) => (
                    <li key={r.release_id}>
                      <button
                        type="button"
                        className="caa-picker-item"
                        disabled={busy}
                        title="この版の表の画像を全曲に入れる"
                        onClick={() => void take({ release_id: r.release_id })}
                      >
                        <Thumb releaseId={r.release_id} />
                        <span>
                          {r.title}
                          {r.release_id === releaseId && <span className="badge">取り込むリリース</span>}
                        </span>
                        <span className="muted small">{groupReleaseSummary(r)}</span>
                      </button>
                      <a
                        href={`https://musicbrainz.org/release/${r.release_id}`}
                        target="_blank"
                        rel="noopener noreferrer"
                        className="small"
                      >
                        MusicBrainz で見る
                      </a>
                    </li>
                  ))}
                </ul>
              )}
              {(without > 0 || omitted > 0) && (
                <div className="muted small">
                  {without > 0 && `表の画像の無い版 ${without} 件は並べていない。`}
                  {omitted > 0 && `版が多いので先頭の ${listing.value.releases.length} 件だけ見た（残り ${omitted} 件は URL を貼る）。`}
                </div>
              )}
            </>
          )}
          <div className="op-row">
            <button type="button" disabled={busy} onClick={() => void take({ release_group_id: groupId })}>
              リリースグループの代表画像を取る
            </button>
          </div>
        </>
      )}
      <form className="op-row" onSubmit={onPaste}>
        <input
          type="text"
          className="caa-picker-paste"
          value={pasted}
          placeholder="MusicBrainz のリリースの URL か MBID"
          aria-label="画像を取るリリース（MusicBrainz の URL か MBID）"
          onChange={(e) => setPasted(e.target.value)}
        />
        <button type="submit" disabled={busy || pasted.trim() === ''}>
          このリリースから取る
        </button>
      </form>
    </div>
  )
}
