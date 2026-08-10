import { describe, expect, it } from 'vitest'
import {
  applyLiveAssistantUpdate,
  applySessionStreamUpdate,
  isDurableSessionUpdate,
  type SessionStreamUpdate,
} from './sessionEvents'
import type { SessionView, Turn } from './types'

function event(sequence: number, type: string, values: Record<string, unknown> = {}): SessionStreamUpdate {
  return {
    id: `event-${sequence}`,
    sequence,
    project_id: 'project-1',
    session_id: 'session-1',
    agent_id: 'agent-1',
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
    agent_id: 'agent-1',
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
    agents: [],
    turns: [turn('running')],
    steps: [],
    rollouts: [{ agent_id: 'agent-1', status: { version: 1, last_sequence: 2, projected_sequence: 2 } }],
    effects: [],
    actions: [],
    attempts: [],
    human_requests: [],
    agent_inputs: [],
    artifacts: [],
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

  it('replaces authoritative Turn state without fabricating Agent rollout progress', () => {
    const completed = turn('completed', 'done')
    const next = applySessionStreamUpdate(
      view(),
      event(7, 'turn_status_changed', { turn: completed }),
    )

    expect(next.turns).toEqual([completed])
    expect(next.rollouts[0]?.status.last_sequence).toBe(2)
    expect(next.rollouts[0]?.status.projected_sequence).toBe(2)
  })

  it('updates the Action and Attempt attached to one Session event', () => {
    const action = {
      id: 'action-1',
      updated_at: '2026-08-04T00:00:01Z',
      created_at: '2026-08-04T00:00:00Z',
    } as SessionView['actions'][number]
    const attempt = {
      id: 'attempt-1',
      updated_at: '2026-08-04T00:00:01Z',
      created_at: '2026-08-04T00:00:00Z',
    } as SessionView['attempts'][number]
    const next = applySessionStreamUpdate(
      view(),
      event(4, 'action_changed', { action, attempt }),
    )

    expect(next.actions).toEqual([action])
    expect(next.attempts).toEqual([attempt])
  })
})
