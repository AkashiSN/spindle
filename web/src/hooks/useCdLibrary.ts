// CD 画面の「ライブラリにある」（§12.6 CD）。TOC（と選んだ候補のリリース）が変わるたびに
// `POST /api/cd/library` を引く。DB だけを読む軽い API なので間引かない。
//
// 入力が変わったら描画中に結果を捨て（lib/cdLibrary.ts の libraryOnInput。A → B → A と戻っても
// 1 回目の A の結果は出さない）、応答はいまの入力のものだけ採る。同じ入力を引き直した古い要求の応答は
// effect の後始末（live）で捨てる

import { useEffect, useState } from 'react'
import { apiPost } from '../api/client'
import {
  libraryOnInput,
  libraryOnLoaded,
  type LibraryResponse,
  type LibraryState,
} from '../lib/cdLibrary'

export function useCdLibrary(toc: string, releaseId: string | null): LibraryResponse | null {
  const trimmed = toc.trim()
  const key = trimmed === '' ? '' : `${trimmed}\u0000${releaseId ?? ''}`
  const [state, setState] = useState<LibraryState>({ key: '', value: null })
  // 描画中に直す React の作法（effect にすると前の盤の帯が 1 回描かれる）
  const next = libraryOnInput(state, key)
  if (next !== state) setState(next)
  useEffect(() => {
    if (trimmed === '') return
    let live = true
    apiPost<LibraryResponse>('/api/cd/library', { toc: trimmed, release_id: releaseId })
      .then((value) => {
        if (live) setState((s) => libraryOnLoaded(s, key, value))
      })
      // 判定は補助の表示なので、失敗しても画面は止めない（帯を出さないだけ）
      .catch(() => {
        if (live) setState((s) => libraryOnLoaded(s, key, null))
      })
    return () => {
      live = false
    }
  }, [key, trimmed, releaseId])
  return next.key === key && key !== '' ? next.value : null
}
