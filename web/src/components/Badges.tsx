// バッジ列（SPEC §12.2）。1 セルに複数アイコン、ホバーで文言

import type { TrackRow } from '../api/types'
import { badgesOf } from '../lib/badges'

export function Badges({ track }: { track: TrackRow }) {
  return (
    <span className="badges">
      {badgesOf(track).map((b) => (
        <span key={b.key} className={b.cls} title={b.label} aria-label={b.label}>
          {b.icon}
        </span>
      ))}
    </span>
  )
}
