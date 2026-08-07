import { describe, expect, it } from 'vitest'
import { agentActivityKind, agentActivitySubject, primaryActionText, sessionEventTitle, shortId, statusLabel, workflowIsTerminal, workflowTitle } from './format'
import { setLocale } from './i18n'
import type { Workflow } from './types'

describe('format helpers', () => {
  it('formats canonical session labels', () => {
    setLocale('en')
    expect(shortId('019fcaaa-0000-7000-8000-000000000001')).toBe('00000001')
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

  it('summarizes tool steps as Codex-style activity rows', () => {
    expect(agentActivityKind('web_search')).toBe('search')
    expect(agentActivityKind('read_file')).toBe('read')
    expect(agentActivityKind('apply_patch')).toBe('edit')
    expect(agentActivityKind('shell_command')).toBe('command')
    expect(agentActivitySubject({ action: { query: '  exact   clue search  ' } })).toBe('exact clue search')
    expect(agentActivitySubject({ opaque: {} })).toBeNull()
  })

  it('uses the Workflow name when an interactive run has no user task', () => {
    const workflow = {
      request: '',
      program: { manifest: { name: 'Interactive agent' } },
    } as Workflow
    expect(workflowTitle(workflow)).toBe('Interactive agent')
  })

  it('releases a Session composer when its Workflow reaches a terminal state', () => {
    expect(workflowIsTerminal({ status: 'running' } as Workflow)).toBe(false)
    expect(workflowIsTerminal({ status: 'completed' } as Workflow)).toBe(true)
    expect(workflowIsTerminal({ status: 'failed' } as Workflow)).toBe(true)
    expect(workflowIsTerminal({ status: 'cancelled' } as Workflow)).toBe(true)
  })
})
