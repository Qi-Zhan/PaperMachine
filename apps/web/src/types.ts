export type Id = string

export const ACCESS_PRESETS = ['model_only', 'read_only', 'workspace', 'research', 'full_access'] as const
export type AccessPreset = (typeof ACCESS_PRESETS)[number]
export type SessionStatus = 'ready' | 'running' | 'paused' | 'failed' | 'archived'
export type SessionOrigin = 'user' | 'workflow_agent'
export type TurnOrigin = 'user' | 'workflow'
export type PromptLayerKind = 'runtime' | 'project' | 'workflow' | 'agent' | 'session' | 'skills' | 'control'
export type TurnStatus =
  | 'queued'
  | 'running'
  | 'paused'
  | 'completed'
  | 'failed'
  | 'interrupted'
  | 'cancelled'
export type StepKind = 'model' | 'tool' | 'workflow' | 'system'
export type StepStatus = 'running' | 'completed' | 'failed' | 'cancelled'
export type WorkflowStatus =
  | 'created'
  | 'running'
  | 'waiting_for_user'
  | 'waiting_for_timer'
  | 'waiting_for_signal'
  | 'paused'
  | 'completed'
  | 'failed'
  | 'cancelled'
export type WorkflowContextMode = 'fresh' | 'project_snapshot'
export type ParticipantStatus = 'active' | 'retired' | 'failed'
export type ActionStatus = 'scheduled' | 'running' | 'completed' | 'failed' | 'interrupted' | 'cancelled'
export type HumanRequestStatus = 'open' | 'answered' | 'cancelled'
export type ControlMessageKind = 'guide' | 'interrupt' | 'finish'

export interface Project {
  id: Id
  name: string
  description: string
  workspace: WorkspaceAttachment
  created_at: string
  updated_at: string
}

export interface WorkspaceAttachment {
  id: Id
  revision: number
  roots: string[]
  primary_root: number
}

export interface ProjectCatalogEntry extends Project {
  available: boolean
  workspace_available: boolean
}

export interface Session {
  id: Id
  project_id: Id
  origin: SessionOrigin
  title: string
  system_prompt: string
  model: string
  access: AccessPreset
  status: SessionStatus
  enabled_skills: string[]
  created_at: string
  updated_at: string
}

export interface SkillSnapshot { slug: string; sha256: string; relative_path: string }
export interface PromptLayer {
  kind: PromptLayerKind
  name: string
  source: string
  content: string
  sha256: string
}
export interface PromptSnapshot { layers: PromptLayer[]; rendered: string; sha256: string }
export interface ProjectSystemPrompt { relative_path: string; content: string; sha256: string }
export interface TokenUsage {
  input_tokens: number
  output_tokens: number
  cached_input_tokens: number
  cache_write_input_tokens: number
}

export interface Turn {
  id: Id
  session_id: Id
  status: TurnStatus
  origin: TurnOrigin
  input: string
  output: string | null
  model: string
  reasoning_effort: 'none' | 'low' | 'medium' | 'high' | 'xhigh' | 'max' | null
  prompt: PromptSnapshot
  environment: TurnEnvironmentSnapshot
  tools_enabled: boolean
  web_search_context_size: 'low' | 'medium' | 'high' | null
  response_format: unknown | null
  skill_snapshots: SkillSnapshot[]
  history: unknown[]
  usage: TokenUsage
  completed_model_steps: number
  hosted_search_calls_used: number
  checkpoint_message: string | null
  error: string | null
  created_at: string
  updated_at: string
}

export interface TurnEnvironmentSnapshot {
  workspace: WorkspaceAttachment
  cwd: string
  authorization: {
    preset: AccessPreset
    workspace_roots: string[]
    cwd: string
    filesystem: unknown
    tools: unknown
    network: unknown
    environment: unknown
  }
  authorization_sha256: string
}

export interface AgentStep {
  id: Id
  turn_id: Id
  sequence: number
  kind: StepKind
  name: string
  tool_call_id: string | null
  status: StepStatus
  input: unknown
  output: unknown | null
  usage: TokenUsage
  duration_ms: number | null
  created_at: string
  updated_at: string
}

export interface WorkflowUsage {
  agents_created: number
  actions_started: number
  actions_completed: number
  action_steps: number
  timer_fires: number
  hosted_search_calls: number
  tokens: TokenUsage
  wall_time_seconds: number
  estimated_cost_usd: number | null
}

export interface WorkflowProgramManifest {
  id: Id
  slug: string
  name: string
  description: string
  entrypoint: string
  request_mode: 'required' | 'none'
  params_schema: Record<string, unknown>
  output_schema: Record<string, unknown>
}

export interface WorkflowProgram {
  project_id: Id | null
  manifest: WorkflowProgramManifest
  source: 'builtin' | 'user'
  definition_path: string
  sha256: string
  updated_at: string
}

export interface WorkflowProgramSnapshot extends WorkflowProgram {
  source_code: string
}

export interface WorkflowLaunchContext {
  mode: WorkflowContextMode
  snapshot: Record<string, unknown> | null
}

export type WorkflowTriggerKind = 'user' | 'workflow' | 'timer' | 'manual'

export interface WorkflowTrigger {
  kind: WorkflowTriggerKind
  source_workflow_id: Id | null
  source_session_id: Id | null
  source_timer_id: Id | null
}

export interface Workflow {
  id: Id
  project_id: Id
  started_from_session_id: Id | null
  program: WorkflowProgramSnapshot
  request: string
  instructions: string
  trigger: WorkflowTrigger
  default_model: string
  access: AccessPreset
  enabled_skills: string[]
  launch_context: WorkflowLaunchContext
  agent_access_overrides: Record<string, AccessPreset>
  status: WorkflowStatus
  params: Record<string, unknown>
  output: unknown | null
  error: string | null
  attention_required: boolean
  usage: WorkflowUsage
  created_at: string
  updated_at: string
}

export interface WorkflowEffect {
  workflow_id: Id
  key: string
  kind: string
  request_sha256: string
  payload: unknown
  status: 'started' | 'completed' | 'failed'
  result: unknown | null
  error: string | null
  started_at: string
  completed_at: string | null
}

export interface WorkflowParticipant {
  id: Id
  workflow_id: Id
  session_id: Id
  class_name: string
  name: string
  role: string
  system_prompt: string
  model: string
  skills: string[]
  status: ParticipantStatus
  created_at: string
  updated_at: string
}

export interface TaskScope {
  id: Id
  workflow_id: Id
  parent_id: Id | null
  name: string
  objective: string
  status: 'open' | 'completed' | 'cancelled'
  created_at: string
  updated_at: string
}

export interface ActionInvocation {
  id: Id
  workflow_id: Id
  task_scope_id: Id | null
  agent_instance_id: Id
  session_id: Id
  action_name: string
  contract: string
  arguments: unknown
  source_human_request_id: Id | null
  status: ActionStatus
  output: unknown | null
  error: string | null
  created_at: string
  updated_at: string
}

export interface ActionAttempt {
  id: Id
  workflow_id: Id
  invocation_id: Id
  number: number
  turn_id: Id | null
  status: ActionStatus
  guidance: string | null
  error: string | null
  created_at: string
  updated_at: string
}

export interface WorkflowTeam {
  id: Id
  workflow_id: Id
  name: string
  member_ids: Id[]
  created_at: string
  updated_at: string
}

export interface AgentRelation {
  id: Id
  workflow_id: Id
  source_agent_id: Id
  target_agent_id: Id
  kind: string
  instructions: string
  created_at: string
}

export interface WorkflowTimer {
  id: Id
  workflow_id: Id
  name: string
  interval_ms: number
  policy: 'coalesce' | 'skip' | 'queue'
  status: 'active' | 'paused' | 'completed' | 'cancelled'
  fire_count: number
  next_fire_at: string
  last_fired_at: string | null
  created_at: string
  updated_at: string
}

export interface WorkflowChannel { id: Id; workflow_id: Id; name: string; schema: unknown; created_at: string }
export interface WorkflowSignal { id: Id; workflow_id: Id; channel_id: Id; sender_agent_id: Id | null; sequence: number; value: unknown; created_at: string }

export interface HumanRequest {
  id: Id
  workflow_id: Id
  session_id: Id
  question: string
  response_schema: Record<string, unknown>
  status: HumanRequestStatus
  answer: unknown | null
  created_at: string
  resolved_at: string | null
}

export interface ControlMessage {
  id: Id
  workflow_id: Id
  session_id: Id
  action_invocation_id: Id | null
  kind: ControlMessageKind
  content: string
  status: 'pending' | 'applied' | 'cancelled'
  created_at: string
  applied_at: string | null
}

export interface Artifact {
  id: Id
  project_id: Id
  workflow_id: Id
  session_id: Id | null
  action_invocation_id: Id | null
  kind: string
  name: string
  media_type: string
  relative_path: string
  sha256: string
  size_bytes: number
  metadata: Record<string, unknown>
  created_at: string
}

export interface ProjectSkill {
  slug: string
  name: string
  description: string
  relative_path: string
  sha256: string
  instructions: string
}

export interface ProjectOverview {
  project: Project
  system_prompt: ProjectSystemPrompt
  sessions: Session[]
  workflows: Workflow[]
  workflow_participants: WorkflowParticipant[]
  human_requests: HumanRequest[]
  artifacts: Artifact[]
}

export interface SessionView {
  session: Session
  turns: Turn[]
  steps: AgentStep[]
  workflows: Workflow[]
  workflow_memberships: WorkflowParticipant[]
  human_requests: HumanRequest[]
}

export interface WorkflowView {
  workflow: Workflow
  effects: WorkflowEffect[]
  participants: WorkflowParticipant[]
  sessions: Session[]
  actions: ActionInvocation[]
  attempts: ActionAttempt[]
  teams: WorkflowTeam[]
  relations: AgentRelation[]
  task_scopes: TaskScope[]
  timers: WorkflowTimer[]
  channels: WorkflowChannel[]
  signals: WorkflowSignal[]
  human_requests: HumanRequest[]
  control_messages: ControlMessage[]
  artifacts: Artifact[]
}

export interface SessionEvent {
  id: Id
  sequence: number
  session_id: Id
  turn_id: Id | null
  step_id: Id | null
  occurred_at: string
  type: string
  [key: string]: unknown
}

export interface Health {
  status: string
  model_mode: 'demo' | 'providers'
  default_model: string
  model_context_window: number
  model_profiles: ModelProfile[]
  model_providers: ModelProvider[]
  workflow_runtime: string
}

export interface ModelProfile {
  id: string
  provider: string
  model: string
  context_window: number
}

export interface ModelProvider {
  id: string
  kind: string
  endpoint: string
  max_request_retries: number
  request_timeout_seconds: number
  stream_idle_timeout_seconds: number
  responses_websockets: boolean
  hosted_web_search: boolean
  prompt_cache_mode: string
}

export interface WorkflowProgramSource {
  registration: WorkflowProgram
  source: string
  validation: WorkflowValidation
}
export interface WorkflowAgentDeclaration { class_name: string; actions: string[]; access: AccessPreset }
export interface WorkflowTimerDeclaration { callback: string; seconds: number | null; policy: string | null }
export interface WorkflowFeatureSummary {
  parallel_blocks: number
  teams: string[]
  relations: number
  scopes: string[]
  channels: string[]
  timers: WorkflowTimerDeclaration[]
  human_checkpoints: number
  background_tasks: number
  project_snapshots: number
  artifacts: number
}
export interface WorkflowDiagnostic { severity: 'error' | 'warning'; message: string; line: number | null; column: number | null }
export interface WorkflowValidation {
  valid: boolean
  manifest: WorkflowProgramManifest | null
  agents: WorkflowAgentDeclaration[]
  features: WorkflowFeatureSummary
  diagnostics: WorkflowDiagnostic[]
}
export interface GeneratedWorkflow { source: string; validation: WorkflowValidation }
export interface WorkflowGenerationInput { description: string; name?: string; slug?: string; model?: string }
export interface CreateSessionInput {
  title: string
  system_prompt: string
  model: string
  enabled_skills: string[]
  access: AccessPreset
}
export interface CreateWorkflowInput {
  program_slug: string
  request?: string
  instructions: string
  params: Record<string, unknown>
  started_from_session_id?: Id
  model: string
  access: AccessPreset
  enabled_skills: string[]
  context_mode?: WorkflowContextMode
  agent_access_overrides?: Record<string, AccessPreset>
}
