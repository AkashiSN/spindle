// ライブラリの左サイドバーのプレイリストの行から、端末の選曲（device_playlists）を触る（SPEC §12.6「端末」「選曲の近道」）

import type { Device } from '../api/types'

export type ChipStatus = 'synced' | 'pending' | 'syncing' | 'error' | 'removing'
export type Chip = { device: Device; status: ChipStatus; title: string }

/** そのプレイリストの端末ごとの印（名前順）。選曲に入っている端末と、外したがまだ端末に残っている端末に出す */
export function chipsFor(playlistId: number, devices: readonly Device[]): Chip[] {
  const out: Chip[] = []
  for (const d of devices) {
    const st = d.playlist_states.find((s) => s.playlist_id === playlistId)
    const selected = d.playlist_ids.includes(playlistId)
    if (!selected) {
      if (st?.state === 'removing') out.push({ device: d, status: 'removing', title: `${d.name}: 次の同期で端末から消える` })
      continue
    }
    // 付けたばかり（表示用の状況がまだ追いついていない）なら同期待ち
    let status: ChipStatus = st == null || st.state === 'removing' ? 'pending' : st.state
    // 実行中の同期で片付くもの（送る曲かプレイリストのファイルがある同期待ち）だけを同期中にする。
    // 送れないもの・準備待ちだけのものは同期しても変わらない
    const sendable = st == null || st.state !== 'pending' || st.pending > 0 || st.waiting === 0
    if (status === 'pending' && sendable && isSyncing(d)) status = 'syncing'
    out.push({ device: d, status, title: `${d.name}: ${describe(status, st?.state === status ? st : undefined)}` })
  }
  return out.sort((a, b) => a.device.name.localeCompare(b.device.name, 'ja'))
}

/** 同期の実行中か。Android は device_sync ジョブ、iPhone は Mac の同期の進捗（open な計画のものだけ届く） */
function isSyncing(d: Device): boolean {
  return d.sync_job?.state === 'running' || d.sync_progress != null
}

function describe(status: ChipStatus, st: Device['playlist_states'][number] | undefined): string {
  switch (status) {
    case 'synced':
      return '同期済み'
    case 'syncing':
      return '同期中'
    case 'removing':
      return '次の同期で端末から消える'
    case 'pending':
      if (st == null) return '同期待ち'
      if (st.pending === 0 && st.waiting === 0) return '同期待ち（プレイリストの更新）'
      return [st.pending > 0 && `同期待ち（${st.pending} 曲）`, st.waiting > 0 && `準備待ち（${st.waiting} 曲）`]
        .filter(Boolean)
        .join('・')
    case 'error':
      if (st != null && st.errors > 0) return `${st.errors} 曲を送れない`
      return `送れない（${st?.reason ?? '不明'}）`
  }
}

/** 印を付け外しした後の playlist_ids（昇順） */
export function togglePlaylist(d: Device, playlistId: number): number[] {
  const set = new Set(d.playlist_ids)
  if (set.has(playlistId)) set.delete(playlistId)
  else set.add(playlistId)
  return [...set].sort((a, b) => a - b)
}

/** 一覧の 1 台の playlist_ids だけを差し替える（PUT の完了を待たずに画面と次の切り替えへ反映するため） */
export function withPlaylistIds(items: readonly Device[] | null, id: number, playlistIds: number[]): Device[] | null {
  if (items == null) return null
  return items.map((d) => (d.id === id ? { ...d, playlist_ids: playlistIds } : d))
}

/** 取得した一覧に、送信待ち・送信中の端末の望む playlist_ids を重ねる（取り直しが楽観更新を古い値で潰さないように） */
export function mergePending(items: readonly Device[], pending: ReadonlyMap<number, number[]>): Device[] {
  if (pending.size === 0) return [...items]
  return items.map((d) => {
    const ids = pending.get(d.id)
    return ids ? { ...d, playlist_ids: ids } : d
  })
}

/** 選曲タブの印の下書きを付け外しする。`draft` が null（未編集）なら保存済みの `saved` から始める。
 *  結果が保存済みと同じになったら null（未編集）に戻す（その後のサイドバーでの付け外しに追随させる） */
export function toggleDraft(
  draft: readonly number[] | null,
  saved: readonly number[],
  playlistId: number,
  on: boolean,
): number[] | null {
  const base = draft ?? saved
  const next = on ? (base.includes(playlistId) ? [...base] : [...base, playlistId]) : base.filter((x) => x !== playlistId)
  return sameIdSet(next, saved) ? null : next
}

/** 順序を問わず同じ集合か */
export function sameIdSet(a: readonly number[], b: readonly number[]): boolean {
  if (a.length !== b.length) return false
  const s = new Set(a)
  return b.every((x) => s.has(x))
}
