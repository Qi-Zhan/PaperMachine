import type {
  Artifact,
  ControlMessage,
  ControlMessageKind,
  CreateSessionRequest,
  GeneratedWorkflow,
  Health,
  HumanRequest,
  ProjectCatalogEntry,
  ProjectOverview,
  ProjectSystemPrompt,
  ProjectSkill,
  Session,
  SessionEvent,
  SessionView,
  WorkflowGenerationInput,
  WorkflowProgram,
  WorkflowProgramSource,
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
  listProjects: () => request<ProjectCatalogEntry[]>('/projects'),
  createProject: (name: string, workspacePath?: string) => {
    const workspace = workspacePath?.trim()
    return request<ProjectCatalogEntry>('/projects', {
      method: 'POST',
      body: JSON.stringify({
        name,
        ...(workspace ? { workspace: { path: workspace } } : {}),
      }),
    })
  },
  pickWorkspaceDirectory: () =>
    request<{ path: string | null }>('/workspaces/pick-directory', {
      method: 'POST',
      body: JSON.stringify({}),
    }),
  relocateProject: (projectId: string, workspacePath: string) =>
    request<ProjectCatalogEntry>(`/projects/${projectId}`, { method: 'PUT', body: JSON.stringify({ workspace: { path: workspacePath } }) }),
  removeProject: (projectId: string) => request<void>(`/projects/${projectId}`, { method: 'DELETE' }),
  getProject: (projectId: string) => request<ProjectOverview>(`/projects/${projectId}`),
  listSessions: (projectId: string, limit = 100) =>
    request<Session[]>(`/projects/${projectId}/sessions?limit=${limit}`),
  updateProjectSystemPrompt: (projectId: string, systemPrompt: string) =>
    request<ProjectSystemPrompt>(`/projects/${projectId}/system-prompt`, {
      method: 'PUT',
      body: JSON.stringify({ system_prompt: systemPrompt }),
    }),
  listProjectSkills: (projectId: string) => request<ProjectSkill[]>(`/projects/${projectId}/skills`),
  createProjectSkill: (
    projectId: string,
    input: { slug: string; name: string; description: string; instructions: string },
  ) => request<ProjectSkill>(`/projects/${projectId}/skills`, { method: 'POST', body: JSON.stringify(input) }),
  getSession: (projectId: string, sessionId: string) =>
    request<SessionView>(`/projects/${projectId}/sessions/${sessionId}`),
  closeSession: (projectId: string, sessionId: string) =>
    request<void>(`/projects/${projectId}/sessions/${sessionId}`, { method: 'DELETE' }),
  cancelTurn: (projectId: string, turnId: string) =>
    request<void>(`/projects/${projectId}/turns/${turnId}/cancel`, { method: 'POST' }),
  listSessionEvents: (projectId: string, sessionId: string, after = 0) =>
    request<SessionEvent[]>(`/projects/${projectId}/sessions/${sessionId}/events?after=${after}`),
  createSession: (projectId: string, input: CreateSessionRequest) =>
    request<Session>(`/projects/${projectId}/sessions`, { method: 'POST', body: JSON.stringify(input) }),
  pauseSession: (projectId: string, sessionId: string) =>
    request<void>(`/projects/${projectId}/sessions/${sessionId}/pause`, { method: 'POST' }),
  resumeSession: (projectId: string, sessionId: string) =>
    request<void>(`/projects/${projectId}/sessions/${sessionId}/resume`, { method: 'POST' }),
  cancelSession: (projectId: string, sessionId: string) =>
    request<void>(`/projects/${projectId}/sessions/${sessionId}/cancel`, { method: 'POST' }),
  createControlMessage: (
    projectId: string,
    sessionId: string,
    agentId: string,
    input: { kind: ControlMessageKind; content: string; action_invocation_id?: string },
  ) => request<ControlMessage>(`/projects/${projectId}/sessions/${sessionId}/agents/${agentId}/control`, {
    method: 'POST',
    body: JSON.stringify(input),
  }),
  answerHumanRequest: (projectId: string, requestId: string, answer: unknown) =>
    request<HumanRequest>(`/projects/${projectId}/human-requests/${requestId}/answer`, { method: 'POST', body: JSON.stringify({ answer }) }),
  listWorkflowPrograms: (projectId: string) => request<WorkflowProgram[]>(`/projects/${projectId}/workflow-programs`),
  getWorkflowProgram: (projectId: string, slug: string) =>
    request<WorkflowProgramSource>(`/projects/${projectId}/workflow-programs/${encodeURIComponent(slug)}`),
  generateWorkflow: (projectId: string, input: WorkflowGenerationInput) =>
    request<GeneratedWorkflow>(`/projects/${projectId}/workflow-programs/generate`, { method: 'POST', body: JSON.stringify(input) }),
  validateWorkflow: (projectId: string, source: string) =>
    request<WorkflowValidation>(`/projects/${projectId}/workflow-programs/validate`, { method: 'POST', body: JSON.stringify({ source }) }),
  saveWorkflowProgram: (projectId: string, source: string) =>
    request<WorkflowProgram>(`/projects/${projectId}/workflow-programs`, { method: 'POST', body: JSON.stringify({ source }) }),
  artifactUrl: (artifact: Artifact) => `${API_ROOT}/projects/${artifact.project_id}/artifacts/${artifact.id}/content`,
  readArtifact: async (artifact: Artifact) => {
    const response = await fetch(`${API_ROOT}/projects/${artifact.project_id}/artifacts/${artifact.id}/content`)
    if (!response.ok) throw new Error(`Artifact request failed with status ${response.status}`)
    return response.text()
  },
}

export const sessionEventTypes = [
  'session_created',
  'session_changed',
  'agent_created',
  'action_changed',
  'turn_created',
  'turn_status_changed',
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
  'human_request_opened',
  'human_request_resolved',
  'control_message_queued',
  'control_message_applied',
  'usage_updated',
  'session_resync',
  'warning',
] as const
