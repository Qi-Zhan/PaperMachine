import { describe, expect, it } from 'vitest'
import {
  applyLiveAssistantUpdate,
  applySessionStreamUpdate,
  isDurableSessionUpdate,
  type SessionEntityUpdate,
} from './sessionEvents'
import type { SessionView, Turn } from './types'

function event(sequence: number, type: string, values: Record<string, unknown> = {}): SessionEntityUpdate {
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

function turn(status: Turn['status'], output: string | null = null): Turn {
  return {
    id: 'turn-1',
    session_id: 'session-1',
    action_attempt_id: 'attempt-1',
    origin: 'workflow',
    input: 'research',
    model: 'test-model',
    status,
    output,
    error: null,
    usage: {
      input_tokens: 0,
      output_tokens: 0,
      cached_input_tokens: 0,
      cache_write_input_tokens: 0,
    },
    created_at: '2026-08-04T00:00:00Z',
    updated_at: '2026-08-04T00:00:00Z',
    prompt_snapshot: [],
    environment_snapshot: null,
    tool_set: null,
  } as unknown as Turn
}

function view(): SessionView {
  return {
    session: { id: 'session-1' } as SessionView['session'],
    turns: [turn('running')],
    steps: [],
    rollout: { version: 1, last_sequence: 2, projected_sequence: 2 },
    workflows: [],
    workflow_memberships: [],
    human_requests: [],
  }
}

describe('Session stream reducer', () => {
  it('keeps transient model text outside durable event history and resets retries', () => {
    let output = {}
    output = applyLiveAssistantUpdate(output, event(0, 'assistant_message_delta', { delta: 'discard me' }))
    output = applyLiveAssistantUpdate(output, event(0, 'assistant_message_reset'))
    output = applyLiveAssistantUpdate(output, event(0, 'assistant_message_delta', { delta: 'keep me' }))

    expect(output).toEqual({ 'turn-1': 'keep me' })
    expect(isDurableSessionUpdate(event(0, 'assistant_message_delta'))).toBe(false)
    expect(isDurableSessionUpdate(event(3, 'sampling_retry'))).toBe(true)
  })

  it('replaces authoritative Turn state and advances the rollout cursor', () => {
    const completed = turn('completed', 'done')
    const next = applySessionStreamUpdate(
      view(),
      event(7, 'turn_status_changed', { turn: completed }),
    )

    expect(next.turns).toEqual([completed])
    expect(next.rollout.last_sequence).toBe(7)
    expect(next.rollout.projected_sequence).toBe(7)
  })

  it('updates a Workflow without fabricating a durable Session event', () => {
    const workflow = {
      id: 'workflow-1',
      updated_at: '2026-08-04T00:00:01Z',
    } as SessionView['workflows'][number]
    const next = applySessionStreamUpdate(view(), { type: 'workflow_changed', workflow })

    expect(next.workflows).toEqual([workflow])
    expect(next.rollout.last_sequence).toBe(2)
  })
})
