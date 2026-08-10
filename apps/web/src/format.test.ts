import { describe, expect, it } from 'vitest'
import { agentActivityKind, agentActivitySubject, primaryActionText, sessionEventTitle, sessionIsTerminal, sessionTitle, shortId, statusLabel } from './format'
import { setLocale } from './i18n'
import type { Session } from './types'

describe('format helpers', () => {
  it('formats canonical session labels', () => {
    setLocale('en')
    expect(shortId('019fcaaa-0000-7000-8000-000000000001')).toBe('00000001')
    expect(
      sessionEventTitle({
        id: 'event',
        sequence: 3,
        project_id: 'project',
        session_id: 'session',
        agent_id: 'agent',
        turn_id: 'turn',
        step_id: null,
        occurred_at: '2026-08-04T00:00:00Z',
        type: 'tool_call_completed',
      }),
    ).toBe('Tool completed')
  })

  it('formats canonical session labels in Chinese', () => {
    setLocale('zh-CN')
    expect(
      sessionEventTitle({
        id: 'event',
        sequence: 3,
        project_id: 'project',
        session_id: 'session',
        agent_id: 'agent',
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

  it('summarizes tool steps as Codex-style activity rows', () => {
    expect(agentActivityKind('web_search')).toBe('search')
    expect(agentActivityKind('read_file')).toBe('read')
    expect(agentActivityKind('apply_patch')).toBe('edit')
    expect(agentActivityKind('shell_command')).toBe('command')
    expect(agentActivitySubject({ action: { query: '  exact   clue search  ' } })).toBe('exact clue search')
    expect(agentActivitySubject({ opaque: {} })).toBeNull()
  })

  it('uses the Session title for a running WorkflowProgram', () => {
    const session = {
      title: 'Investigation',
      request: '',
      program: { manifest: { name: 'Interactive agent' } },
    } as Session
    expect(sessionTitle(session)).toBe('Investigation')
  })

  it('recognizes terminal Session states', () => {
    expect(sessionIsTerminal({ status: 'running' } as Session)).toBe(false)
    expect(sessionIsTerminal({ status: 'completed' } as Session)).toBe(true)
    expect(sessionIsTerminal({ status: 'failed' } as Session)).toBe(true)
    expect(sessionIsTerminal({ status: 'cancelled' } as Session)).toBe(true)
  })
})
