import type {
  AgentAccessProfile,
  Artifact,
  ControlMessage,
  ControlMessageKind,
  CreateWorkflowInput,
  GeneratedWorkflow,
  Health,
  HumanRequest,
  Project,
  ProjectOverview,
  ProjectSystemPrompt,
  ProjectSkill,
  Session,
  SessionEvent,
  SessionView,
  Turn,
  WorkflowGenerationInput,
  WorkflowProgram,
  Workflow,
  WorkflowView,
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
  listProjects: () => request<Project[]>('/projects'),
  createProject: (name: string, description: string, rootPath: string) =>
    request<Project>('/projects', { method: 'POST', body: JSON.stringify({ name, description, root_path: rootPath }) }),
  getProject: (projectId: string) => request<ProjectOverview>(`/projects/${projectId}`),
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
  getSession: (sessionId: string) => request<SessionView>(`/sessions/${sessionId}`),
  createTurn: (sessionId: string, input: string) =>
    request<Turn>(`/sessions/${sessionId}/turns`, { method: 'POST', body: JSON.stringify({ input }) }),
  cancelTurn: (turnId: string) => request<void>(`/turns/${turnId}/cancel`, { method: 'POST' }),
  updateSessionSkills: (sessionId: string, enabledSkills: string[]) =>
    request<Session>(`/sessions/${sessionId}/skills`, { method: 'PUT', body: JSON.stringify({ enabled_skills: enabledSkills }) }),
  updateSessionAccess: (sessionId: string, access: AgentAccessProfile) =>
    request<Session>(`/sessions/${sessionId}/access`, { method: 'PUT', body: JSON.stringify({ access }) }),
  updateSessionSystemPrompt: (sessionId: string, systemPrompt: string) =>
    request<Session>(`/sessions/${sessionId}/system-prompt`, {
      method: 'PUT',
      body: JSON.stringify({ system_prompt: systemPrompt }),
    }),
  listSessionEvents: (sessionId: string, after = 0) =>
    request<SessionEvent[]>(`/sessions/${sessionId}/events?after=${after}`),
  createWorkflow: (projectId: string, input: CreateWorkflowInput) =>
    request<Workflow>(`/projects/${projectId}/workflows`, { method: 'POST', body: JSON.stringify(input) }),
  getWorkflow: (workflowId: string) => request<WorkflowView>(`/workflows/${workflowId}`),
  pauseWorkflow: (workflowId: string) => request<void>(`/workflows/${workflowId}/pause`, { method: 'POST' }),
  resumeWorkflow: (workflowId: string) => request<void>(`/workflows/${workflowId}/resume`, { method: 'POST' }),
  cancelWorkflow: (workflowId: string) => request<void>(`/workflows/${workflowId}/cancel`, { method: 'POST' }),
  sendControl: (
    runId: string,
    sessionId: string,
    kind: ControlMessageKind,
    content: string,
    actionInvocationId?: string,
  ) => request<ControlMessage>(`/workflows/${runId}/sessions/${sessionId}/control`, {
    method: 'POST',
    body: JSON.stringify({ kind, content, action_invocation_id: actionInvocationId ?? null }),
  }),
  answerHumanRequest: (requestId: string, answer: unknown) =>
    request<HumanRequest>(`/human-requests/${requestId}/answer`, { method: 'POST', body: JSON.stringify({ answer }) }),
  listWorkflowPrograms: (projectId: string) => request<WorkflowProgram[]>(`/projects/${projectId}/workflow-programs`),
  getWorkflowProgram: (projectId: string, slug: string) =>
    request<WorkflowProgramSource>(`/projects/${projectId}/workflow-programs/${encodeURIComponent(slug)}`),
  generateWorkflow: (input: WorkflowGenerationInput) =>
    request<GeneratedWorkflow>('/workflow-programs/generate', { method: 'POST', body: JSON.stringify(input) }),
  validateWorkflow: (source: string) =>
    request<WorkflowValidation>('/workflow-programs/validate', { method: 'POST', body: JSON.stringify({ source }) }),
  saveWorkflowProgram: (projectId: string, source: string) =>
    request<WorkflowProgram>(`/projects/${projectId}/workflow-programs`, { method: 'POST', body: JSON.stringify({ source }) }),
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
