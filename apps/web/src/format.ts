import type { SessionEvent } from './types'
import { currentLocale, statusText, t } from './i18n'

export function shortId(value: string): string {
  return value.slice(-8)
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

export function sessionEventTitle(event: SessionEvent): string {
  const explicit: Record<string, string> = {
    session_created: t('event.sessionCreated'),
    session_status_changed: t('event.sessionUpdated', { status: statusLabel(String(event.status ?? 'updated')) }),
    turn_created: t('event.turnQueued'),
    turn_status_changed: t('event.turnUpdated', { status: statusLabel(String(event.status ?? 'updated')) }),
    agent_started: t('event.agentStarted'),
    assistant_message_completed: t('event.responseCompleted'),
    model_step_completed: t('event.modelStep', { step: String(event.step ?? '') }),
    model_step_failed: t('event.modelStepFailed', { step: String(event.step ?? '') }),
    tool_call_started: t('event.toolStarted'),
    tool_call_completed: t('event.toolCompleted'),
    hosted_tool_completed: t('event.toolCompleted'),
    context_trimmed: t('event.contextCompacted'),
    context_compacted: t('event.contextCompacted'),
    sampling_retry: t('event.modelRetry'),
    workflow_agent_attached: t('event.workflowAgentAttached'),
    human_request_opened: t('event.humanRequested'),
    human_request_resolved: t('event.humanResolved'),
    control_message_applied: t('event.guidanceApplied'),
    warning: t('event.warning'),
  }
  return explicit[event.type] ?? statusLabel(event.type)
}
