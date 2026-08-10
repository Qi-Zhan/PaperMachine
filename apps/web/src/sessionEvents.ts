import type {
  ActionAttempt,
  ActionInvocation,
  Agent,
  AgentStep,
  HumanRequest,
  Session,
  SessionEvent,
  SessionView,
  Turn,
} from './types'

export type SessionStreamUpdate = SessionEvent & {
  session?: Session
  agent?: Agent
  turn?: Turn
  step?: AgentStep
  human_request?: HumanRequest
  action?: ActionInvocation
  attempt?: ActionAttempt
}

export type LiveAssistantOutputs = Record<string, string>

export function applySessionStreamUpdate(
  view: SessionView,
  update: SessionStreamUpdate,
): SessionView {
  const turns = update.turn ? upsert(view.turns, update.turn, byCreatedAt) : view.turns
  const turnOrder = new Map(turns.map((turn, index) => [turn.id, index]))
  return {
    ...view,
    session: update.session?.id === view.session.id ? update.session : view.session,
    agents: update.agent ? upsert(view.agents, update.agent, byCreatedAt) : view.agents,
    turns,
    steps: update.step
      ? upsert(view.steps, update.step, (left, right) => {
          const turnDifference =
            (turnOrder.get(left.turn_id) ?? Number.MAX_SAFE_INTEGER) -
            (turnOrder.get(right.turn_id) ?? Number.MAX_SAFE_INTEGER)
          return turnDifference || left.sequence - right.sequence
        })
      : view.steps,
    human_requests: update.human_request
      ? upsert(view.human_requests, update.human_request, byCreatedAt)
      : view.human_requests,
    actions: update.action ? upsert(view.actions, update.action, byCreatedAt) : view.actions,
    attempts: update.attempt ? upsert(view.attempts, update.attempt, byCreatedAt) : view.attempts,
  }
}

export function applyLiveAssistantUpdate(
  outputs: LiveAssistantOutputs,
  update: SessionStreamUpdate,
): LiveAssistantOutputs {
  if (!update.turn_id) return outputs
  if (update.type === 'assistant_message_delta') {
    return {
      ...outputs,
      [update.turn_id]: `${outputs[update.turn_id] ?? ''}${String(update.delta ?? '')}`,
    }
  }
  if (update.type === 'assistant_message_reset') {
    return omit(outputs, update.turn_id)
  }
  if (update.type === 'turn_status_changed' && update.turn && isTerminalTurn(update.turn)) {
    return omit(outputs, update.turn_id)
  }
  return outputs
}

export function isDurableSessionUpdate(update: SessionStreamUpdate): boolean {
  return update.sequence > 0
}

function omit(outputs: LiveAssistantOutputs, turnId: string): LiveAssistantOutputs {
  if (!(turnId in outputs)) return outputs
  const next = { ...outputs }
  delete next[turnId]
  return next
}

function isTerminalTurn(turn: Turn): boolean {
  return ['completed', 'failed', 'interrupted', 'cancelled'].includes(turn.status)
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
