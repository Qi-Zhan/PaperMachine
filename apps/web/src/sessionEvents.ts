import type {
  AgentStep,
  HumanRequest,
  Session,
  SessionEvent,
  SessionView,
  Turn,
  Workflow,
  WorkflowParticipant,
} from './types'

export type SessionEntityUpdate = SessionEvent & {
  session?: Session
  turn?: Turn
  step?: AgentStep
  workflow?: Workflow
  participant?: WorkflowParticipant
  human_request?: HumanRequest
}

export interface SessionWorkflowUpdate {
  type: 'workflow_changed'
  workflow: Workflow
}

export type SessionStreamUpdate = SessionEntityUpdate | SessionWorkflowUpdate
export type LiveAssistantOutputs = Record<string, string>

export function applySessionStreamUpdate(
  view: SessionView,
  update: SessionStreamUpdate,
): SessionView {
  const sessionUpdate = isSessionEntityUpdate(update) ? update : null
  const workflow = update.workflow
  const turns = sessionUpdate?.turn ? upsert(view.turns, sessionUpdate.turn, byCreatedAt) : view.turns
  const turnOrder = new Map(turns.map((turn, index) => [turn.id, index]))

  return {
    ...view,
    session:
      sessionUpdate?.session?.id === view.session.id ? sessionUpdate.session : view.session,
    turns,
    steps: sessionUpdate?.step
      ? upsert(view.steps, sessionUpdate.step, (left, right) => {
          const turnDifference =
            (turnOrder.get(left.turn_id) ?? Number.MAX_SAFE_INTEGER) -
            (turnOrder.get(right.turn_id) ?? Number.MAX_SAFE_INTEGER)
          return turnDifference || left.sequence - right.sequence
        })
      : view.steps,
    workflows: workflow ? upsert(view.workflows, workflow, byUpdatedAtDescending) : view.workflows,
    workflow_memberships: sessionUpdate?.participant
      ? upsert(view.workflow_memberships, sessionUpdate.participant, byCreatedAt)
      : view.workflow_memberships,
    human_requests: sessionUpdate?.human_request
      ? upsert(view.human_requests, sessionUpdate.human_request, byCreatedAt)
      : view.human_requests,
    rollout:
      sessionUpdate && sessionUpdate.sequence > 0
        ? {
            ...view.rollout,
            last_sequence: Math.max(view.rollout.last_sequence, sessionUpdate.sequence),
            projected_sequence: Math.max(view.rollout.projected_sequence, sessionUpdate.sequence),
          }
        : view.rollout,
  }
}

export function applyLiveAssistantUpdate(
  outputs: LiveAssistantOutputs,
  update: SessionStreamUpdate,
): LiveAssistantOutputs {
  if (!isSessionEntityUpdate(update) || !update.turn_id) return outputs
  if (update.type === 'assistant_message_delta') {
    return {
      ...outputs,
      [update.turn_id]: `${outputs[update.turn_id] ?? ''}${String(update.delta ?? '')}`,
    }
  }
  if (update.type === 'assistant_message_reset') {
    if (!(update.turn_id in outputs)) return outputs
    const next = { ...outputs }
    delete next[update.turn_id]
    return next
  }
  if (update.type === 'turn_status_changed' && update.turn && isTerminalTurn(update.turn)) {
    if (!(update.turn_id in outputs)) return outputs
    const next = { ...outputs }
    delete next[update.turn_id]
    return next
  }
  return outputs
}

export function isDurableSessionUpdate(
  update: SessionStreamUpdate,
): update is SessionEntityUpdate {
  return isSessionEntityUpdate(update) && update.sequence > 0
}

function isSessionEntityUpdate(update: SessionStreamUpdate): update is SessionEntityUpdate {
  return 'session_id' in update
}

function isTerminalTurn(turn: Turn): boolean {
  return ['completed', 'failed', 'cancelled'].includes(turn.status)
}

function upsert<T extends { id: string }>(
  values: T[],
  value: T,
  compare: (left: T, right: T) => number,
): T[] {
  return [value, ...values.filter((candidate) => candidate.id !== value.id)].sort(compare)
}

function byCreatedAt<T extends { created_at: string; id: string }>(left: T, right: T): number {
  return left.created_at.localeCompare(right.created_at) || left.id.localeCompare(right.id)
}

function byUpdatedAtDescending<T extends { updated_at: string; id: string }>(
  left: T,
  right: T,
): number {
  return right.updated_at.localeCompare(left.updated_at) || left.id.localeCompare(right.id)
}
