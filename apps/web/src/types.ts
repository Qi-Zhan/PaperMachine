export type Id = string

export const ACCESS_PRESETS = ['model_only', 'read_only', 'workspace', 'research', 'full_access'] as const
export type AccessPreset = (typeof ACCESS_PRESETS)[number]
export type SessionStatus =
  | 'created'
  | 'running'
  | 'waiting_for_input'
  | 'waiting_for_deadline'
  | 'paused'
  | 'closing'
  | 'completed'
  | 'failed'
  | 'cancelled'
export type PromptLayerKind = 'runtime' | 'project' | 'session' | 'agent' | 'skills' | 'control'
export type TurnStatus =
  | 'queued'
  | 'running'
  | 'paused'
  | 'completed'
  | 'failed'
  | 'interrupted'
  | 'cancelled'
export type StepKind = 'model' | 'tool' | 'workflow' | 'system'
export type StepStatus = 'running' | 'completed' | 'failed' | 'aborted' | 'cancelled'
export type ActionStatus = 'scheduled' | 'running' | 'completed' | 'failed' | 'interrupted' | 'cancelled'
export type HumanRequestStatus = 'open' | 'answered' | 'cancelled'
export type ControlMessageKind = 'guide' | 'interrupt' | 'finish'

export interface Project {
  id: Id
  name: string
  workspace: WorkspaceAttachment
  created_at: string
  updated_at: string
}

export interface WorkspaceAttachment {
  id: Id
  revision: number
  path: string
}

export interface ProjectCatalogEntry extends Project {
  workspace_available: boolean
}

export interface SkillSnapshot { slug: string; sha256: string }
export interface PromptLayer {
  kind: PromptLayerKind
  name: string
  source: string
  content: string
  sha256: string
}
export interface PromptSnapshot { layers: PromptLayer[]; rendered: string; sha256: string }
export interface ProjectSystemPrompt { relative_path: string; content: string; sha256: string }
export interface ToolDefinition {
  name: string
  description: string
  input_schema: unknown
  supports_parallel: boolean
}
export interface ToolSetSnapshot { definitions: ToolDefinition[]; sha256: string }
export interface TokenUsage {
  input_tokens: number
  output_tokens: number
  cached_input_tokens: number
  cache_write_input_tokens: number
}

export interface ModelRouteSnapshot {
  profile: string
  provider: string
  upstream_model: string
  context_window: number
  capabilities: { hosted_web_search: boolean }
  reasoning_effort: 'none' | 'low' | 'medium' | 'high' | 'xhigh' | 'max' | null
  config_sha256: string
}

export interface TurnEnvironmentSnapshot {
  workspace: WorkspaceAttachment
  cwd: string
  authorization: {
    preset: AccessPreset
    workspace_root: string
    cwd: string
    filesystem: unknown
    tools: unknown
    network: unknown
    environment: unknown
  }
  authorization_sha256: string
}

export interface Turn {
  id: Id
  agent_id: Id
  status: TurnStatus
  input: string
  output: string | null
  model_route: ModelRouteSnapshot
  prompt: PromptSnapshot
  environment: TurnEnvironmentSnapshot
  tool_set: ToolSetSnapshot
  tools_enabled: boolean
  web_search_context_size: 'low' | 'medium' | 'high' | null
  response_format: unknown | null
  skill_snapshots: SkillSnapshot[]
  usage: TokenUsage
  completed_model_steps: number
  hosted_search_calls_used: number
  checkpoint_message: string | null
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
  tool_call_id: string | null
  status: StepStatus
  input: unknown
  output: unknown | null
  usage: TokenUsage
  duration_ms: number | null
  created_at: string
  updated_at: string
}

export interface SessionUsage {
  agents_created: number
  actions_started: number
  actions_completed: number
  action_steps: number
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
  runtime_sha256: string
  source_code: string
}

export interface SessionTrigger {
  kind: 'user' | 'manual'
  source_session_id: Id | null
}

/** One durable runtime instance of a WorkflowProgram. */
export interface Session {
  id: Id
  project_id: Id
  program: WorkflowProgramSnapshot
  title: string
  request: string
  instructions: string
  trigger: SessionTrigger
  default_model: string
  access: AccessPreset
  enabled_skills: string[]
  agent_access_overrides: Record<string, AccessPreset>
  status: SessionStatus
  closing_status: SessionStatus | null
  params: Record<string, unknown>
  output: unknown | null
  error: string | null
  attention_required: boolean
  usage: SessionUsage
  archived_at: string | null
  created_at: string
  updated_at: string
}

export interface Agent {
  id: Id
  session_id: Id
  class_name: string
  name: string
  role: string
  system_prompt: string
  model: string
  access: AccessPreset
  skills: string[]
  created_at: string
}

export interface SessionEffect {
  session_id: Id
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

export interface ActionInvocation {
  id: Id
  session_id: Id
  agent_id: Id
  action_name: string
  contract: string
  arguments: unknown
  input: string
  source:
    | { kind: 'workflow' }
    | { kind: 'human_request'; request_id: Id }
    | { kind: 'agent'; sender_agent_id: Id }
  requested_tools: string[]
  tools_enabled: boolean
  web_search_context_size: WebSearchContextSize | null
  reasoning_effort: ReasoningEffort | null
  response_format: unknown | null
  status: ActionStatus
  output: unknown | null
  error: string | null
  created_at: string
  updated_at: string
}

export interface ActionAttempt {
  id: Id
  invocation_id: Id
  number: number
  turn_id: Id | null
  status: ActionStatus
  guidance: string | null
  error: string | null
  created_at: string
  updated_at: string
}

export interface HumanRequest {
  id: Id
  session_id: Id
  agent_id: Id
  question: string
  response_schema: Record<string, unknown>
  status: HumanRequestStatus
  answer: unknown | null
  created_at: string
  resolved_at: string | null
}

export interface ControlMessage {
  id: Id
  session_id: Id
  agent_id: Id
  action_invocation_id: Id | null
  kind: ControlMessageKind
  content: string
  status: 'pending' | 'claimed' | 'applied'
  created_at: string
  claimed_turn_id: Id | null
  claimed_at: string | null
  applied_at: string | null
}

export interface Artifact {
  id: Id
  project_id: Id
  session_id: Id
  agent_id: Id | null
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

export interface ProjectHome {
  project_id: Id
  artifact_id: Id
  source_artifact_id: Id
  revision: string
  updated_at: string
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
  project_home: ProjectHome | null
  project_home_artifact: Artifact | null
  summary_session: Session | null
}

export interface AgentRolloutStatus {
  version: number
  last_sequence: number
  projected_sequence: number
}

export interface AgentRolloutView {
  agent_id: Id
  status: AgentRolloutStatus
}

export interface SessionView {
  session: Session
  agents: Agent[]
  turns: Turn[]
  steps: AgentStep[]
  rollouts: AgentRolloutView[]
  effects: SessionEffect[]
  actions: ActionInvocation[]
  attempts: ActionAttempt[]
  human_requests: HumanRequest[]
  control_messages: ControlMessage[]
  artifacts: Artifact[]
}

export interface SessionEvent {
  id: Id
  sequence: number
  project_id: Id
  session_id: Id
  agent_id: Id | null
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
  capabilities: string[]
  default_reasoning_effort: 'none' | 'low' | 'medium' | 'high' | 'xhigh' | 'max' | null
  config_sha256: string
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

export interface WorkflowProgramSource {
  registration: WorkflowProgram
  source: string
  validation: WorkflowValidation
}
export interface WorkflowAgentDeclaration {
  class_name: string
  actions: WorkflowActionDeclaration[]
  access: AccessPreset
}
export interface WorkflowActionDeclaration { name: string; tools: string[] }
export interface WorkflowDiagnostic { severity: 'error' | 'warning'; message: string; line: number | null; column: number | null }
export interface WorkflowValidation {
  valid: boolean
  manifest: WorkflowProgramManifest | null
  agents: WorkflowAgentDeclaration[]
  diagnostics: WorkflowDiagnostic[]
}
export interface GeneratedWorkflow { source: string; validation: WorkflowValidation }
export interface WorkflowGenerationInput { description: string; name?: string; slug?: string; model?: string }

export interface CreateSessionRequest {
  program_slug: string
  title?: string
  request?: string
  instructions: string
  params: Record<string, unknown>
  source_session_id?: Id
  model: string
  access: AccessPreset
  enabled_skills: string[]
  agent_access_overrides?: Record<string, AccessPreset>
}

export interface CreateInteractiveSessionInput {
  title: string
  system_prompt: string
  model: string
  enabled_skills: string[]
  access: AccessPreset
}
