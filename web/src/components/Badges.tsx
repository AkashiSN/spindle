// バッジ列（SPEC §12.2）。1 セルに複数アイコン、ホバーで文言

import type { TrackRow } from '../api/types'
import { badgesOf, deviceBadges, type DeviceName } from '../lib/badges'

export function Badges({ track, devices }: { track: TrackRow; devices: readonly DeviceName[] }) {
  return (
    <span className="badges">
      {[...badgesOf(track), ...deviceBadges(track, devices)].map((b) => (
        <span key={b.key} className={b.cls} title={b.label} aria-label={b.label}>
          {b.icon}
        </span>
      ))}
    </span>
  )
}
