export type Id = string

export const AGENT_ACCESS_PROFILES = ['model_only', 'read_only', 'workspace', 'research', 'full_access'] as const
export type AgentAccessProfile = (typeof AGENT_ACCESS_PROFILES)[number]
export type SessionStatus = 'ready' | 'running' | 'waiting_for_human' | 'paused' | 'failed' | 'archived'
export type SessionOrigin = 'user' | 'workflow_agent'
export type TurnStatus =
  | 'queued'
  | 'running'
  | 'waiting_for_human'
  | 'paused'
  | 'completed'
  | 'failed'
  | 'interrupted'
  | 'cancelled'
export type StepKind = 'model' | 'tool' | 'workflow' | 'system'
export type StepStatus = 'running' | 'completed' | 'failed' | 'cancelled'
export type WorkflowRunStatus = 'created' | 'running' | 'paused' | 'completed' | 'failed' | 'cancelled'
export type ParticipantStatus = 'active' | 'waiting_for_human' | 'retired' | 'failed'
export type ActionStatus = 'scheduled' | 'running' | 'waiting_for_human' | 'completed' | 'failed' | 'interrupted' | 'cancelled'
export type HumanRequestStatus = 'open' | 'answered' | 'cancelled'
export type ControlMessageKind = 'guide' | 'interrupt' | 'finish'

export interface Research {
  id: Id
  name: string
  description: string
  created_at: string
  updated_at: string
}

export interface Session {
  id: Id
  research_id: Id
  origin: SessionOrigin
  title: string
  instructions: string
  model: string
  access: AgentAccessProfile
  status: SessionStatus
  enabled_skills: string[]
  created_at: string
  updated_at: string
}

export interface SkillSnapshot { slug: string; sha256: string; relative_path: string }
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
  input: string
  output: string | null
  model: string
  reasoning_effort: 'none' | 'low' | 'medium' | 'high' | 'xhigh' | 'max' | null
  instructions: string
  access: AgentAccessProfile
  max_steps: number
  max_search_calls: number | null
  web_search_context_size: 'low' | 'medium' | 'high' | null
  max_output_tokens: number | null
  response_format: unknown | null
  skill_snapshots: SkillSnapshot[]
  history: unknown[]
  usage: TokenUsage
  error: string | null
  created_at: string
  updated_at: string
}

export interface AgentStep {
  id: Id
  turn_id: Id
  sequence: number
  kind: StepKind
  name: string
  status: StepStatus
  input: unknown
  output: unknown | null
  usage: TokenUsage
  duration_ms: number | null
  created_at: string
  updated_at: string
}

export interface Budget {
  max_agents: number
  max_concurrent_actions: number
  max_action_steps: number
  max_total_tokens: number | null
  max_uncached_tokens: number | null
  max_hosted_search_calls: number | null
  max_wall_time_seconds: number | null
  max_cost_usd: number | null
}

export interface BudgetUsage {
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

export interface WorkflowManifest {
  id: Id
  slug: string
  name: string
  version: string
  description: string
  entrypoint: string
  input_schema: Record<string, unknown>
  output_schema: Record<string, unknown>
  default_budget: Budget
}

export interface WorkflowRegistration {
  manifest: WorkflowManifest
  source: 'builtin' | 'user'
  definition_path: string
  sha256: string
  updated_at: string
}

export interface WorkflowSnapshot extends WorkflowRegistration {
  source_code: string
}

export interface WorkflowRun {
  id: Id
  research_id: Id
  origin_session_id: Id
  workflow: WorkflowSnapshot
  objective: string
  status: WorkflowRunStatus
  input: Record<string, unknown>
  output: unknown | null
  error: string | null
  attention_required: boolean
  budget: Budget
  usage: BudgetUsage
  created_at: string
  updated_at: string
}

export interface WorkflowParticipant {
  id: Id
  workflow_run_id: Id
  session_id: Id
  class_name: string
  name: string
  role: string
  instructions: string
  model: string
  skills: string[]
  status: ParticipantStatus
  created_at: string
  updated_at: string
}

export interface TaskScope {
  id: Id
  workflow_run_id: Id
  parent_id: Id | null
  name: string
  objective: string
  status: 'open' | 'completed' | 'cancelled'
  created_at: string
  updated_at: string
}

export interface ActionInvocation {
  id: Id
  workflow_run_id: Id
  task_scope_id: Id | null
  agent_instance_id: Id
  session_id: Id
  action_name: string
  objective: string
  arguments: unknown
  status: ActionStatus
  output: unknown | null
  error: string | null
  created_at: string
  updated_at: string
}

export interface ActionAttempt {
  id: Id
  workflow_run_id: Id
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
  workflow_run_id: Id
  name: string
  member_ids: Id[]
  created_at: string
  updated_at: string
}

export interface AgentRelation {
  id: Id
  workflow_run_id: Id
  source_agent_id: Id
  target_agent_id: Id
  kind: string
  instructions: string
  created_at: string
}

export interface WorkflowTimer {
  id: Id
  workflow_run_id: Id
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

export interface WorkflowChannel { id: Id; workflow_run_id: Id; name: string; schema: unknown; created_at: string }
export interface WorkflowSignal { id: Id; workflow_run_id: Id; channel_id: Id; sender_agent_id: Id | null; sequence: number; value: unknown; created_at: string }

export interface HumanRequest {
  id: Id
  workflow_run_id: Id
  action_invocation_id: Id | null
  action_attempt_id: Id | null
  session_id: Id
  turn_id: Id | null
  question: string
  response_schema: Record<string, unknown>
  status: HumanRequestStatus
  answer: unknown | null
  created_at: string
  resolved_at: string | null
}

export interface ControlMessage {
  id: Id
  workflow_run_id: Id
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
  research_id: Id
  workflow_run_id: Id
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

export interface ResearchSkill {
  slug: string
  name: string
  description: string
  relative_path: string
  sha256: string
  instructions: string
}

export interface ResearchOverview {
  research: Research
  sessions: Session[]
  workflow_runs: WorkflowRun[]
  workflow_participants: WorkflowParticipant[]
  human_requests: HumanRequest[]
  artifacts: Artifact[]
}

export interface SessionView {
  session: Session
  turns: Turn[]
  steps: AgentStep[]
  workflow_runs: WorkflowRun[]
  workflow_memberships: WorkflowParticipant[]
  human_requests: HumanRequest[]
}

export interface WorkflowRunView {
  workflow_run: WorkflowRun
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
  model_mode: 'demo' | 'openai' | 'providers'
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
  prompt_cache_mode: string
}

export interface WorkflowSource { registration: WorkflowRegistration; source: string }
export interface WorkflowAgentDeclaration { class_name: string; actions: string[]; access: AgentAccessProfile }
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
}
export interface WorkflowDiagnostic { severity: 'error' | 'warning'; message: string; line: number | null; column: number | null }
export interface WorkflowValidation {
  valid: boolean
  manifest: WorkflowManifest | null
  agents: WorkflowAgentDeclaration[]
  features: WorkflowFeatureSummary
  diagnostics: WorkflowDiagnostic[]
}
export interface GeneratedWorkflow { source: string; validation: WorkflowValidation }
export interface WorkflowGenerationInput { description: string; name?: string; slug?: string; model?: string }
export interface CreateSessionInput {
  title: string
  instructions: string
  model: string
  enabled_skills: string[]
  access: AgentAccessProfile
}
export interface CreateWorkflowRunInput { workflow_slug: string; workflow_version: string; objective: string; input: Record<string, unknown> }
