// Inbox の承認画面の画像の差し替え（D-86）。画像を置く 2 つの経路: ファイルのアップロード
// （`POST /api/artwork/upload`。ライブラリの「アートワーク」と同じ）と、Cover Art Archive
// （`POST /api/artwork/from-caa`。リリースか、リリースグループの代表。D-93）。どちらも置いた画像の `<mime>:<sha256hex>`（下書きの picture）を返す。
// 失敗は error に入れて null を返す

import { useCallback, useState } from 'react'
import { parseErrorBody } from '../api/client'
import type { UploadedArtwork } from '../lib/artwork'
import { operationErrorMessage } from '../lib/operations'

export type ArtworkUploadState = {
  busy: boolean
  error: string | null
  upload: (file: File) => Promise<string | null>
  fromCaa: (target: CaaTarget) => Promise<string | null>
}

/** Cover Art Archive の取り先。リリースは MBID か MusicBrainz のリリースの URL（貼り付け）。D-93 */
export type CaaTarget = { release_id: string } | { release_group_id: string }

function valueOf(u: UploadedArtwork): string {
  return `${u.mime}:${u.sha256}`
}

export function useArtworkUpload(): ArtworkUploadState {
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const run = useCallback(async (path: string, init: RequestInit): Promise<string | null> => {
    setBusy(true)
    setError(null)
    try {
      const r = await parseErrorBody<UploadedArtwork>(path, init)
      if (r.ok) return valueOf(r.body)
      setError(r.status === 404 ? 'Cover Art Archive にこの盤の画像が無い' : operationErrorMessage(r.status, r.body))
      return null
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
      return null
    } finally {
      setBusy(false)
    }
  }, [])
  const upload = useCallback(
    (file: File) =>
      run('/api/artwork/upload', {
        method: 'POST',
        headers: { 'Content-Type': file.type || 'application/octet-stream' },
        body: file,
      }),
    [run],
  )
  const fromCaa = useCallback(
    (target: CaaTarget) =>
      run('/api/artwork/from-caa', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(target),
      }),
    [run],
  )
  return { busy, error, upload, fromCaa }
}
