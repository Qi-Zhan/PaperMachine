import { describe, expect, it } from 'vitest'
import { liveAssistantOutput } from './sessionEvents'
import type { SessionEvent } from './types'

function event(sequence: number, type: string, values: Record<string, unknown> = {}): SessionEvent {
  return {
    id: `event-${sequence}`,
    sequence,
    session_id: 'session-1',
    turn_id: 'turn-1',
    step_id: null,
    occurred_at: '2026-08-04T00:00:00Z',
    type,
    ...values,
  }
}

describe('liveAssistantOutput', () => {
  it('drops partial text from a failed sample before showing its retry', () => {
    const events = [
      event(1, 'assistant_message_delta', { delta: 'discard me' }),
      event(2, 'assistant_message_reset'),
      event(3, 'sampling_retry', { attempt: 1 }),
      event(4, 'assistant_message_delta', { delta: 'keep ' }),
      event(5, 'assistant_message_delta', { delta: 'me' }),
    ]

    expect(liveAssistantOutput(events, 'turn-1')).toBe('keep me')
    expect(liveAssistantOutput(events, 'another-turn')).toBe('')
  })
})
