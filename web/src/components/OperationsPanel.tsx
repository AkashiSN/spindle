// 右パネル「操作」タブ（D-58、D-87）: 横に並ぶ番号付きの段 ① 対象 → ② 操作を選ぶ → ③ 確かめる / プレビュー →
// ④ 実行。② で 1 つ選ぶと ③ ④ がその操作用になる。操作には「巻き戻せる変更 / ジョブ / 読むだけ / すぐ反映」の印。
// リネーム / 正規化は preview（old → new と衝突理由の一覧）→ 適用、RG 解析 / RG 書き込み / FLAC 検査等は
// 投入して件数を出す。アートワークはアップロード → プレビュー → 選択の埋め込み画像を差し替え（D-60）。
// プレイリストへ追加・album gain（D-74）もここ。状態は hooks/useOperations

import { useState, type ReactNode } from 'react'
import type { AlbumRow, Playlist } from '../api/types'
import { useLocalStorageState } from '../hooks/useLocalStorageState'
import type { Operations, PathKind } from '../hooks/useOperations'
import { ALBUM_GAIN_LIMIT } from '../lib/albumGain'
import { albumTitle, artworkUrl, uploadedSummary } from '../lib/artwork'
import { formatCount } from '../lib/format'
import { pathPreviewSummary } from '../lib/operations'
import { PanelTargetStep, type PanelTarget } from './PanelTargetStep'
import { Step } from './Step'

const PATH_LABEL: Record<PathKind, string> = { rename: 'リネーム', normalize: '正規化（→ FLAC）' }

/** 選択行が属する album の album gain 切り替え（D-74） */
export type AlbumGainControl = {
  albums: AlbumRow[]
  busy: boolean
  error: string | null
  onToggle: (id: number, on: boolean) => void
}

type OpId =
  | 'rename'
  | 'normalize'
  | 'rg'
  | 'rgwrite'
  | 'albumgain'
  | 'flaccheck'
  | 'md5fill'
  | 'hirescheck'
  | 'verify'
  | 'artwork'
  | 'playlist'

/** 操作の性質（③ の見出しの印と ④ の見出し） */
type OpNature = 'undo' | 'job' | 'read' | 'now'

type OpDef = { id: OpId; label: string; nature: OpNature; desc: string }

const GROUPS: { title: string; ops: OpDef[] }[] = [
  {
    title: 'ファイル',
    ops: [
      { id: 'rename', label: 'リネーム', nature: 'undo', desc: '[layout] の規則でパスを並べ直す' },
      { id: 'normalize', label: '正規化（→ FLAC）', nature: 'undo', desc: 'WAV・ALAC を FLAC にする' },
    ],
  },
  {
    title: '音量',
    ops: [
      {
        id: 'rg',
        label: 'ReplayGain を解析',
        nature: 'job',
        desc: '音量を測る（タグには書かない）。album gain が on の album は album 単位、それ以外は曲ごとのジョブ',
      },
      { id: 'rgwrite', label: '解析値をタグに書く', nature: 'undo', desc: '測った値を REPLAYGAIN_*（Opus は R128_*）に書く' },
      {
        id: 'albumgain',
        label: 'album gain',
        nature: 'now',
        desc: 'アルバム通し再生用の値を持つか。on にすると album 単位で解析し直す。off にすると album の値を消す',
      },
    ],
  },
  {
    title: '検査',
    ops: [
      { id: 'flaccheck', label: 'FLAC を検査', nature: 'read', desc: 'デコードして壊れていないか、MD5 が有るかを確かめる' },
      {
        id: 'md5fill',
        label: 'MD5 を補填',
        nature: 'undo',
        desc: '検査で MD5 無しだった FLAC の STREAMINFO に、デコードした PCM の MD5 を書く',
      },
      {
        id: 'hirescheck',
        label: '偽ハイレゾを検出',
        nature: 'read',
        desc: '可逆かつ 48 kHz 超または 16 bit 超のトラックを解析し、アップサンプリング・ビット深度の水増しを判定する',
      },
      {
        id: 'verify',
        label: '遡及照合',
        nature: 'read',
        desc: 'アルバムを CTDB / AccurateRip に照会し、CD 由来の FLAC を格付けする（44.1kHz/16bit/2ch の FLAC だけ。不一致は「要確認」で不良ではない。ログは data/verify/）',
      },
    ],
  },
  {
    title: '画像・プレイリスト',
    ops: [
      { id: 'artwork', label: '埋め込み画像の差し替え', nature: 'undo', desc: '選択の埋め込み画像を 1 枚にする' },
      { id: 'playlist', label: 'プレイリストへ追加', nature: 'now', desc: '手動プレイリストの末尾に足す' },
    ],
  },
]

const ALL_OPS = GROUPS.flatMap((g) => g.ops)
const isOpId = (v: unknown): v is OpId => ALL_OPS.some((o) => o.id === v)

const NATURE: Record<OpNature, { badge: string; cls: string; verb: string }> = {
  undo: { badge: '巻き戻せる変更', cls: 'undo', verb: '適用' },
  job: { badge: 'ジョブ', cls: 'info', verb: '投入' },
  read: { badge: '読むだけ', cls: '', verb: '投入' },
  now: { badge: 'すぐ反映', cls: 'info', verb: '反映' },
}

export function OperationsPanel({
  ops,
  hasSelection,
  target,
  playlists,
  onAddToPlaylist,
  albumGain,
}: {
  ops: Operations
  hasSelection: boolean
  target: PanelTarget
  /** 「プレイリストへ追加」の候補（手動のみ）と追加の実行（P1-6） */
  playlists: Playlist[]
  onAddToPlaylist: (playlistId: number) => void
  albumGain: AlbumGainControl
}) {
  const [picked, setPicked] = useLocalStorageState<OpId>('ops.picked', 'rename', isOpId)
  const [description, setDescription] = useState('')
  const [embedDescription, setEmbedDescription] = useState('')
  const [addTo, setAddTo] = useState<number | ''>('')
  const busy = ops.busy != null
  const op = ALL_OPS.find((o) => o.id === picked) ?? ALL_OPS[0]
  const nature = NATURE[op.nature]
  const pathKind: PathKind | null = op.id === 'rename' || op.id === 'normalize' ? op.id : null
  const pv = pathKind != null && ops.pathPreview?.kind === pathKind ? ops.pathPreview : null
  const pathStale = pathKind != null && ops.pathStale === pathKind
  const manual = playlists.filter((p) => p.kind === 'manual')

  const label = (id: string, text: string) => (ops.busy === id ? `${text}…` : text)
  // 実行中は切り替えない（遅れて返った結果が別の操作の段に出ないように）。切り替えたら前の操作の
  // 結果・失敗・反映待ちの確認は消す（「除外して適用」が別の操作を実行しないように）
  const pick = (id: OpId) => {
    if (id === picked || busy) return
    setPicked(id)
    ops.clearMessages()
  }

  // ③ の中身と ④ を押せるか（押せない理由）
  let check: ReactNode = null
  let blocked: string | null = hasSelection ? null : '① で対象を選ぶ'
  let run: (() => void) | null = null
  let runLabel = ''
  switch (op.id) {
    case 'rename':
    case 'normalize': {
      const kind = op.id
      check = (
        <>
          {pv != null ? (
            <div className="path-preview">
              <div className="preview-counts">
                {pathPreviewSummary(pv.preview)}（対象 {formatCount(pv.preview.count)} 件）
              </div>
              {pv.preview.items.length > 0 && (
                <table className="path-items">
                  <tbody>
                    {pv.preview.items.map((it) => (
                      <tr key={it.id} className={it.new == null ? 'conflict' : ''}>
                        <td className="old">{it.old}</td>
                        <td className="arrow">→</td>
                        <td className="new">{it.new ?? <span className="reason">{it.reason ?? '生成できない'}</span>}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              )}
            </div>
          ) : pathStale ? (
            <div className="panel-banner warn small">選択・ソートを変えた。プレビューし直す</div>
          ) : (
            <span className="muted small">プレビューで旧 → 新のパスと衝突を出す</span>
          )}
          <button
            type="button"
            className={pv != null ? undefined : 'primary'}
            disabled={busy || !hasSelection}
            onClick={() => void ops.previewPaths(kind)}
          >
            {label(`preview:${kind}`, pv != null || pathStale ? 'プレビューし直す' : `${PATH_LABEL[kind]}をプレビュー`)}
          </button>
        </>
      )
      if (blocked == null && pv == null) blocked = '③ のプレビューが済むと押せる'
      else if (blocked == null && pv != null && pv.preview.changed === 0) blocked = '変わるファイルが無い'
      runLabel = label(`apply:${kind}`, `${PATH_LABEL[kind]}を適用`)
      run = () => {
        void ops.applyPaths(description).then((ok) => {
          if (ok) setDescription('')
        })
      }
      break
    }
    case 'rg':
      runLabel = label('rg', '解析ジョブを投入')
      run = () => void ops.startRg()
      check = <span className="muted small">済んだら「解析値をタグに書く」で書き込む</span>
      break
    case 'rgwrite':
      runLabel = label('rgwrite', 'タグに書く')
      run = () => void ops.writeRg()
      check = <span className="muted small">解析済みの曲だけに書く（未解析は飛ばす）</span>
      break
    case 'md5fill':
      runLabel = label('md5fill', 'MD5 を補填')
      run = () => void ops.startMd5Fill()
      check = <span className="muted small">先に「FLAC を検査」で MD5 無しと分かった FLAC だけが対象</span>
      break
    case 'flaccheck':
      runLabel = label('flaccheck', '検査を投入')
      run = () => void ops.startFlaccheck()
      check = <span className="muted small">結果はバッジで出る。ファイルは変えない</span>
      break
    case 'hirescheck':
      runLabel = label('hirescheck', '検出を投入')
      run = () => void ops.startHirescheck()
      check = <span className="muted small">結果はバッジで出る。ファイルは変えない</span>
      break
    case 'verify':
      runLabel = label('verify', '照合を投入')
      run = () => void ops.startVerify()
      check = <span className="muted small">アルバム単位のジョブ。結果はバッジと data/verify/ のログ</span>
      break
    case 'albumgain':
      check =
        albumGain.albums.length === 0 ? (
          <span className="muted small">選択にアルバムが無い</span>
        ) : albumGain.albums.length > ALBUM_GAIN_LIMIT ? (
          <span className="muted small">選択が {ALBUM_GAIN_LIMIT} album を超えています。絞ってください</span>
        ) : (
          <div className="album-gain">
            {albumGain.albums.map((a) => (
              <label key={a.id} className="small">
                <input
                  type="checkbox"
                  checked={a.album_gain}
                  disabled={albumGain.busy}
                  onChange={(e) => albumGain.onToggle(a.id, e.target.checked)}
                />{' '}
                {albumTitle(a)}
              </label>
            ))}
            {albumGain.error && <span className="error small">{albumGain.error}</span>}
          </div>
        )
      break
    case 'artwork':
      check = (
        <>
          <input
            type="file"
            accept="image/jpeg,image/png,image/webp"
            aria-label="画像ファイル"
            disabled={busy}
            onChange={(e) => {
              const f = e.target.files?.[0]
              // 取り出したら値を空にする。同じファイルを選び直しても change が発火するように
              // （404 artwork_not_found で「もう一度アップロード」を促す経路）
              e.target.value = ''
              if (f) void ops.uploadArtwork(f)
            }}
          />
          <span className="muted small">JPEG / PNG / WebP</span>
          {ops.uploaded && (
            <div className="artwork-upload">
              <img src={artworkUrl(ops.uploaded.sha256, 256)} alt="アップロードした画像" width={72} height={72} />
              <div className="small">
                {uploadedSummary(ops.uploaded)}{' '}
                <button type="button" className="ghost" disabled={busy} onClick={ops.clearUploaded}>
                  取り消し
                </button>
              </div>
            </div>
          )}
        </>
      )
      if (blocked == null && ops.uploaded == null) blocked = '③ で画像を選ぶ'
      runLabel = label('embed', '埋め込み画像を差し替え')
      run = () => {
        void ops.embedArtwork(embedDescription).then((ok) => {
          if (ok) setEmbedDescription('')
        })
      }
      break
    case 'playlist':
      check = (
        <select
          value={addTo}
          disabled={!hasSelection || manual.length === 0}
          onChange={(e) => setAddTo(e.target.value === '' ? '' : Number(e.target.value))}
          aria-label="追加先のプレイリスト"
        >
          <option value="">{manual.length === 0 ? '手動プレイリストが無い' : '追加先を選ぶ…'}</option>
          {manual.map((p) => (
            <option key={p.id} value={p.id}>
              {p.name}
            </option>
          ))}
        </select>
      )
      if (blocked == null && addTo === '') blocked = '③ で追加先を選ぶ'
      runLabel = '追加'
      run = () => {
        if (addTo !== '') onAddToPlaylist(addTo)
      }
      break
  }

  const checkDone = hasSelection && blocked == null
  const stale = pathStale && pv == null
  const hasDescription = pathKind != null || op.id === 'artwork'

  return (
    <div className="operations panel-steps">
      <PanelTargetStep target={target} hasSelection={hasSelection} />

      <Step no={2} title="操作を選ぶ" done>
        <div className="op-pick">
          {GROUPS.map((g) => (
            <div key={g.title} className="op-group">
              <h4>{g.title}</h4>
              <div className="op-chips" role="radiogroup" aria-label={g.title}>
                {g.ops.map((o) => (
                  <button
                    key={o.id}
                    type="button"
                    role="radio"
                    aria-checked={o.id === op.id}
                    className={o.id === op.id ? 'on' : ''}
                    disabled={busy && o.id !== op.id}
                    title={o.desc}
                    onClick={() => pick(o.id)}
                  >
                    {o.label}
                  </button>
                ))}
              </div>
            </div>
          ))}
        </div>
      </Step>

      <Step
        no={3}
        title={pathKind != null ? 'プレビュー' : '確かめる'}
        aside={
          <>
            {op.label} <span className={`badge ${nature.cls}`}>{nature.badge}</span>
          </>
        }
        wait={!hasSelection}
        done={checkDone}
        stale={stale}
      >
        <div className="op-actions">
          <div className="muted small">{op.desc}</div>
          {check}
        </div>
      </Step>

      <Step no={4} title={nature.verb} wait={!checkDone && ops.notice == null} done={ops.notice != null}>
        <div className="op-actions">
          {run == null ? (
            <span className="muted small">③ で切り替えるとすぐ反映される</span>
          ) : (
            <>
              {hasDescription && (
                <input
                  placeholder="説明（任意。履歴に残る）"
                  value={op.id === 'artwork' ? embedDescription : description}
                  disabled={!checkDone}
                  onChange={(e) =>
                    op.id === 'artwork' ? setEmbedDescription(e.target.value) : setDescription(e.target.value)
                  }
                  aria-label="説明"
                />
              )}
              <button type="button" className="primary" disabled={busy || blocked != null} onClick={run}>
                {runLabel}
              </button>
              {blocked != null && <span className="muted small">{blocked}</span>}
            </>
          )}
          {ops.pendingPrompt && (
            <div className="pending-prompt" role="alertdialog">
              <p>対象のうち {formatCount(ops.pendingPrompt.count)} 件が反映待ちです。</p>
              <button
                type="button"
                onClick={() => {
                  // pendingPrompt は現在の選択に紐づくもの（hooks/useOperations）だけが渡ってくる
                  const p = ops.pendingPrompt
                  if (!p) return
                  if (p.action === 'paths') {
                    void ops.applyPaths(description, true).then((ok) => {
                      if (ok) setDescription('')
                    })
                  } else if (p.action === 'md5fill') {
                    void ops.startMd5Fill(true)
                  } else if (p.action === 'embed') {
                    void ops.embedArtwork(embedDescription, true).then((ok) => {
                      if (ok) setEmbedDescription('')
                    })
                  } else {
                    void ops.writeRg(true)
                  }
                }}
              >
                {formatCount(ops.pendingPrompt.count)} 件を除外して適用
              </button>{' '}
              <button type="button" className="ghost" onClick={ops.dismissPending}>
                待つ
              </button>
            </div>
          )}
          {ops.error && <p className="error small">{ops.error}</p>}
          {ops.notice && (
            <p className="panel-banner ok small">
              {ops.notice}{' '}
              <button type="button" className="ghost" onClick={ops.clearNotice} title="閉じる">
                ×
              </button>
            </p>
          )}
        </div>
      </Step>
    </div>
  )
}
