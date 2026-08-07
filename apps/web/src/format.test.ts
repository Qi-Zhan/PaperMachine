import { describe, expect, it } from 'vitest'
import { primaryActionText, sessionEventTitle, shortId, statusLabel } from './format'
import { setLocale } from './i18n'

describe('format helpers', () => {
  it('formats canonical session labels', () => {
    setLocale('en')
    expect(shortId('019fcaaa-0000-7000-8000-000000000001')).toBe('00000001')
    expect(statusLabel('waiting_for_human')).toBe('Waiting for human')
    expect(
      sessionEventTitle({
        id: 'event',
        sequence: 3,
        session_id: 'session',
        turn_id: 'turn',
        step_id: null,
        occurred_at: '2026-08-04T00:00:00Z',
        type: 'tool_call_completed',
      }),
    ).toBe('Tool completed')
  })

  it('formats canonical session labels in Chinese', () => {
    setLocale('zh-CN')
    expect(statusLabel('waiting_for_human')).toBe('等待人工输入')
    expect(
      sessionEventTitle({
        id: 'event',
        sequence: 3,
        session_id: 'session',
        turn_id: 'turn',
        step_id: null,
        occurred_at: '2026-08-04T00:00:00Z',
        type: 'tool_call_completed',
      }),
    ).toBe('Tool 已完成')
    setLocale('en')
  })

  it('selects conversational text from structured Workflow Action input', () => {
    expect(
      primaryActionText({
        question: 'The broad user request',
        objective: 'Verify the disputed primary-source claim',
        coverage_ids: ['evidence'],
      }),
    ).toBe('Verify the disputed primary-source claim')
    expect(primaryActionText({ evidence_ledger: [] })).toBeNull()
  })
})
