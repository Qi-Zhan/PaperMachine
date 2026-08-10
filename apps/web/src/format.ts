import type { Session, SessionEvent } from './types'
import { currentLocale, statusText, t } from './i18n'

export function shortId(value: string): string {
  return value.slice(-8)
}

export function sessionTitle(session: Session): string {
  return session.title || session.request.trim() || session.program.manifest.name
}

export function sessionIsTerminal(session: Session): boolean {
  return ['completed', 'failed', 'cancelled'].includes(session.status)
}

export function formatDate(value: string): string {
  const date = new Date(value)
  const today = new Date()
  if (date.toDateString() === today.toDateString()) {
    return new Intl.DateTimeFormat(currentLocale(), {
      hour: '2-digit',
      minute: '2-digit',
    }).format(date)
  }
  return new Intl.DateTimeFormat(currentLocale(), {
    month: 'short',
    day: 'numeric',
  }).format(date)
}

export function formatDateTime(value: string): string {
  return new Intl.DateTimeFormat(currentLocale(), {
    month: 'short',
    day: 'numeric',
    hour: '2-digit',
    minute: '2-digit',
  }).format(new Date(value))
}

export function formatCount(value: number): string {
  return new Intl.NumberFormat(currentLocale(), { notation: value >= 10_000 ? 'compact' : 'standard' }).format(
    value,
  )
}

export function formatDuration(value: number | null): string {
  if (value === null) return ''
  if (value < 1_000) return `${value} ms`
  return `${(value / 1_000).toFixed(value < 10_000 ? 1 : 0)} s`
}

export function statusLabel(status: string): string {
  return statusText(status)
}

export function primaryActionText(value: unknown): string | null {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return null
  const argumentsObject = value as Record<string, unknown>
  for (const key of ['message', 'objective', 'question', 'request', 'task', 'guidance', 'prompt', 'topic']) {
    const candidate = argumentsObject[key]
    if (typeof candidate === 'string' && candidate.trim()) return candidate.trim()
  }
  return null
}

export type AgentActivityKind = 'search' | 'read' | 'command' | 'edit' | 'tool'

export function agentActivityKind(name: string): AgentActivityKind {
  const normalized = name.toLowerCase()
  if (normalized.includes('search')) return 'search'
  if (['write', 'edit', 'patch', 'save'].some((part) => normalized.includes(part))) return 'edit'
  if (['shell', 'command', 'exec', 'terminal'].some((part) => normalized.includes(part))) return 'command'
  if (['read', 'list', 'find', 'open', 'fetch', 'get'].some((part) => normalized.includes(part))) return 'read'
  return 'tool'
}

export function agentActivitySubject(value: unknown): string | null {
  const subject = findActivitySubject(value)
  if (subject === null || subject === undefined) return null
  const rendered = String(subject).replace(/\s+/g, ' ').trim()
  if (!rendered) return null
  return rendered.length <= 180 ? rendered : `${rendered.slice(0, 177)}…`
}

function findActivitySubject(value: unknown, depth = 0): unknown {
  if (typeof value === 'string' || typeof value === 'number') return value
  if (!value || typeof value !== 'object' || depth > 2) return null
  if (Array.isArray(value)) {
    for (const item of value) {
      const candidate = findActivitySubject(item, depth + 1)
      if (candidate !== null) return candidate
    }
    return null
  }
  const record = value as Record<string, unknown>
  for (const key of ['query', 'url', 'path', 'file_path', 'command', 'cmd', 'pattern', 'name']) {
    const candidate = record[key]
    if (typeof candidate === 'string' || typeof candidate === 'number') return candidate
  }
  for (const candidate of Object.values(record)) {
    const nested = findActivitySubject(candidate, depth + 1)
    if (nested !== null) return nested
  }
  return null
}

export function sessionEventTitle(event: SessionEvent): string {
  const explicit: Record<string, string> = {
    session_created: t('event.sessionCreated'),
    session_changed: t('event.sessionUpdated', { status: statusLabel(String(event.status ?? 'updated')) }),
    turn_created: t('event.turnQueued'),
    turn_status_changed: t('event.turnUpdated', { status: statusLabel(String(event.status ?? 'updated')) }),
    assistant_message_completed: t('event.responseCompleted'),
    model_step_completed: t('event.modelStep'),
    model_step_failed: t('event.modelStepFailed'),
    tool_call_started: t('event.toolStarted'),
    tool_call_completed: t('event.toolCompleted'),
    hosted_tool_completed: t('event.toolCompleted'),
    context_trimmed: t('event.contextCompacted'),
    context_compacted: t('event.contextCompacted'),
    sampling_retry: t('event.modelRetry'),
    agent_created: t('event.agentCreated'),
    human_request_opened: t('event.humanRequested'),
    human_request_resolved: t('event.humanResolved'),
    agent_input_applied: t('event.guidanceApplied'),
    warning: t('event.warning'),
  }
  return explicit[event.type] ?? statusLabel(event.type)
}
