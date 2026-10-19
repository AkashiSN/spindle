// ルールの検証と件数（POST /api/playlists/preview）。入力が止まって 250ms 後に 1 回だけ問い合わせ、
// 古い問い合わせは中断する（PC の SmartRuleEditor と同じ間引き）

import { useEffect, useState } from 'react'
import { ApiError } from '../api/client'
import { previewRule } from './api'

export type RulePreviewState =
  | { kind: 'idle' }
  | { kind: 'loading' }
  | { kind: 'ok'; count: number; ast: unknown }
  | { kind: 'error'; message: string }

const DEBOUNCE_MS = 250

/** 問い合わせの失敗を画面の文言にする（400 はサーバの構文エラーの文言をそのまま出す） */
export function previewErrorMessage(e: unknown): string {
  if (e instanceof ApiError) return e.status === 400 ? e.message : `確かめられませんでした（${e.code}）`
  return '確かめられませんでした'
}

/** rule が null か空なら idle（問い合わせない） */
export function useRulePreview(rule: string | null): RulePreviewState {
  const trimmed = rule?.trim() ?? ''
  const [result, setResult] = useState<{ rule: string; state: RulePreviewState } | null>(null)
  useEffect(() => {
    if (trimmed === '') return
    const ac = new AbortController()
    const timer = window.setTimeout(() => {
      previewRule(trimmed, ac.signal)
        .then((r) => {
          if (!ac.signal.aborted) setResult({ rule: trimmed, state: { kind: 'ok', count: r.count, ast: r.ast } })
        })
        .catch((e: unknown) => {
          if (!ac.signal.aborted) setResult({ rule: trimmed, state: { kind: 'error', message: previewErrorMessage(e) } })
        })
    }, DEBOUNCE_MS)
    return () => {
      window.clearTimeout(timer)
      ac.abort()
    }
  }, [trimmed])
  if (trimmed === '') return { kind: 'idle' }
  // 今の文に対する結果が届くまでは「確かめ中」
  return result != null && result.rule === trimmed ? result.state : { kind: 'loading' }
}
