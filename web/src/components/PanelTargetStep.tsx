// 右パネルの一括編集・操作タブの「① 対象」（D-87）。表で選んだトラックの件数・反映待ち・アルバム

import { formatCount } from '../lib/format'
import { Step } from './Step'

/** 選択の要約（RightPanel が選択と読み込み済みの行から作る） */
export type PanelTarget = {
  /** 選択件数。filter 形でサーバの件数がまだ無ければ null */
  count: number | null
  /** うち反映待ち。判定できなければ null */
  pending: number | null
  /** 選択行が属するアルバム名（読み込み済みの行から。filter 形は表示中の一部だけ） */
  albums: string[]
}

/** アルバム名を出す上限（超えた分は「ほか N」） */
const ALBUM_LIMIT = 4

export function PanelTargetStep({ target, hasSelection }: { target: PanelTarget; hasSelection: boolean }) {
  const shown = target.albums.slice(0, ALBUM_LIMIT)
  const rest = target.albums.length - shown.length
  return (
    <Step
      no={1}
      title="対象"
      done={hasSelection}
      wait={!hasSelection}
      hint="下の表で選んだトラック。選び直すとプレビューはやり直しになる。反映待ち（前のバッチがまだファイルに書かれていない）の行は、除外して進められる"
    >
      {!hasSelection ? (
        <span className="muted small">下の表でトラックを選ぶ</span>
      ) : (
        <dl className="panel-facts">
          <dt>選択</dt>
          <dd>
            <b>{formatCount(target.count)}</b> 曲
          </dd>
          {target.albums.length > 0 && (
            <>
              <dt>アルバム</dt>
              <dd>
                {shown.join('、')}
                {rest > 0 && <span className="muted">　ほか {rest}</span>}
              </dd>
            </>
          )}
          {target.pending != null && target.pending > 0 && (
            <>
              <dt>反映待ち</dt>
              <dd>
                <span className="badge warn">{formatCount(target.pending)} 曲</span>
              </dd>
            </>
          )}
        </dl>
      )}
    </Step>
  )
}
