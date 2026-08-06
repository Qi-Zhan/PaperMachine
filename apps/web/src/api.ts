import type {
  AgentAccessProfile,
  Artifact,
  ControlMessage,
  ControlMessageKind,
  CreateSessionInput,
  CreateWorkflowRunInput,
  GeneratedWorkflow,
  Health,
  HumanRequest,
  Research,
  ResearchOverview,
  ResearchSkill,
  Session,
  SessionEvent,
  SessionView,
  Turn,
  WorkflowGenerationInput,
  WorkflowRegistration,
  WorkflowRun,
  WorkflowRunView,
  WorkflowSource,
  WorkflowValidation,
} from './types'

const API_ROOT = '/api'

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const response = await fetch(`${API_ROOT}${path}`, {
    ...init,
    headers: {
      ...(init?.body ? { 'content-type': 'application/json' } : {}),
      ...init?.headers,
    },
  })
  if (!response.ok) {
    const payload = (await response.json().catch(() => null)) as { error?: string } | null
    throw new Error(payload?.error ?? `Request failed with status ${response.status}`)
  }
  if (response.status === 202 || response.status === 204) return undefined as T
  return response.json() as Promise<T>
}

export const api = {
  health: () => request<Health>('/health'),
  listResearches: () => request<Research[]>('/researches'),
  createResearch: (name: string, description: string) =>
    request<Research>('/researches', { method: 'POST', body: JSON.stringify({ name, description }) }),
  getResearch: (researchId: string) => request<ResearchOverview>(`/researches/${researchId}`),
  listResearchSkills: (researchId: string) => request<ResearchSkill[]>(`/researches/${researchId}/skills`),
  createResearchSkill: (
    researchId: string,
    input: { slug: string; name: string; description: string; instructions: string },
  ) => request<ResearchSkill>(`/researches/${researchId}/skills`, { method: 'POST', body: JSON.stringify(input) }),
  createSession: (researchId: string, input: CreateSessionInput) =>
    request<Session>(`/researches/${researchId}/sessions`, { method: 'POST', body: JSON.stringify(input) }),
  getSession: (sessionId: string) => request<SessionView>(`/sessions/${sessionId}`),
  createTurn: (sessionId: string, input: string) =>
    request<Turn>(`/sessions/${sessionId}/turns`, { method: 'POST', body: JSON.stringify({ input }) }),
  cancelTurn: (turnId: string) => request<void>(`/turns/${turnId}/cancel`, { method: 'POST' }),
  updateSessionSkills: (sessionId: string, enabledSkills: string[]) =>
    request<Session>(`/sessions/${sessionId}/skills`, { method: 'PUT', body: JSON.stringify({ enabled_skills: enabledSkills }) }),
  updateSessionAccess: (sessionId: string, access: AgentAccessProfile) =>
    request<Session>(`/sessions/${sessionId}/access`, { method: 'PUT', body: JSON.stringify({ access }) }),
  listSessionEvents: (sessionId: string, after = 0) =>
    request<SessionEvent[]>(`/sessions/${sessionId}/events?after=${after}`),
  createWorkflowRun: (sessionId: string, input: CreateWorkflowRunInput) =>
    request<WorkflowRun>(`/sessions/${sessionId}/workflow-runs`, { method: 'POST', body: JSON.stringify(input) }),
  getWorkflowRun: (runId: string) => request<WorkflowRunView>(`/workflow-runs/${runId}`),
  pauseWorkflowRun: (runId: string) => request<void>(`/workflow-runs/${runId}/pause`, { method: 'POST' }),
  resumeWorkflowRun: (runId: string) => request<void>(`/workflow-runs/${runId}/resume`, { method: 'POST' }),
  cancelWorkflowRun: (runId: string) => request<void>(`/workflow-runs/${runId}/cancel`, { method: 'POST' }),
  sendControl: (
    runId: string,
    sessionId: string,
    kind: ControlMessageKind,
    content: string,
    actionInvocationId?: string,
  ) => request<ControlMessage>(`/workflow-runs/${runId}/sessions/${sessionId}/control`, {
    method: 'POST',
    body: JSON.stringify({ kind, content, action_invocation_id: actionInvocationId ?? null }),
  }),
  answerHumanRequest: (requestId: string, answer: unknown) =>
    request<HumanRequest>(`/human-requests/${requestId}/answer`, { method: 'POST', body: JSON.stringify({ answer }) }),
  listWorkflows: () => request<WorkflowRegistration[]>('/workflows'),
  getWorkflow: (slug: string, version: string) =>
    request<WorkflowSource>(`/workflows/${encodeURIComponent(slug)}/${encodeURIComponent(version)}`),
  generateWorkflow: (input: WorkflowGenerationInput) =>
    request<GeneratedWorkflow>('/workflows/generate', { method: 'POST', body: JSON.stringify(input) }),
  validateWorkflow: (source: string) =>
    request<WorkflowValidation>('/workflows/validate', { method: 'POST', body: JSON.stringify({ source }) }),
  saveWorkflow: (source: string) =>
    request<WorkflowRegistration>('/workflows', { method: 'POST', body: JSON.stringify({ source }) }),
  artifactUrl: (artifact: Artifact) => `${API_ROOT}/artifacts/${artifact.id}/content`,
  readArtifact: async (artifact: Artifact) => {
    const response = await fetch(`${API_ROOT}/artifacts/${artifact.id}/content`)
    if (!response.ok) throw new Error(`Artifact request failed with status ${response.status}`)
    return response.text()
  },
}

export const sessionEventTypes = [
  'session_created',
  'session_status_changed',
  'turn_created',
  'turn_status_changed',
  'agent_started',
  'assistant_message_delta',
  'assistant_message_reset',
  'assistant_message_completed',
  'model_step_started',
  'model_step_completed',
  'model_step_failed',
  'tool_call_started',
  'tool_call_completed',
  'hosted_tool_completed',
  'context_trimmed',
  'context_compacted',
  'sampling_retry',
  'workflow_agent_attached',
  'human_request_opened',
  'human_request_resolved',
  'control_message_applied',
  'warning',
] as const
