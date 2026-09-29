import { useCallback, useEffect, useState } from 'react'
import type { HoustonClient } from './client'
import type { HarnessFinding } from './generated/HarnessFinding'
import type { HarnessReview } from './generated/HarnessReview'
import type { Routine } from './generated/Routine'

export interface HarnessState {
  workspace: string
  routine: Routine | null
  reviews: HarnessReview[]
  findings: HarnessFinding[]
}

export interface HarnessReport {
  reviewId: number
  markdown: string
  truncated: boolean
}

/// A workspace's reviews, re-read whenever the daemon says they changed or a
/// routine mutation may have changed the review routine.
export function useHarness(
  client: HoustonClient | null,
  workspace: string | null
): {
  state: HarnessState | null
  report: HarnessReport | null
  loadReport: (reviewId: number) => void
} {
  const [state, setState] = useState<HarnessState | null>(null)
  const [report, setReport] = useState<HarnessReport | null>(null)

  useEffect(() => {
    setState(null)
    setReport(null)
    if (!client || !workspace) return
    const offState = client.subscribe('harness_state', (msg) => {
      if (msg.workspace !== workspace) return
      setState({
        workspace: msg.workspace,
        routine: msg.routine ?? null,
        reviews: msg.reviews,
        findings: msg.findings
      })
    })
    const offChanged = client.subscribe('harness_changed', (msg) => {
      if (msg.workspace === workspace) client.harnessState(workspace)
    })
    const offRoutines = client.subscribe('routines', () => client.harnessState(workspace))
    const offReport = client.subscribe('harness_report', (msg) =>
      setReport({
        reviewId: msg.review_id,
        markdown: msg.markdown,
        truncated: msg.truncated
      })
    )
    client.harnessState(workspace)
    return () => {
      offState()
      offChanged()
      offRoutines()
      offReport()
    }
  }, [client, workspace])

  const loadReport = useCallback(
    (reviewId: number) => {
      if (client) client.harnessReport(reviewId)
    },
    [client]
  )

  return { state, report, loadReport }
}
