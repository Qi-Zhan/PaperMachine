<template>
  <div class="session-workspace">
    <header class="page-header session-page-header">
      <div class="page-leading session-heading">
        <button
          class="icon-button mobile-only"
          type="button"
          :title="t('common.openSidebar')"
          :aria-label="t('common.openSidebar')"
          @click="$emit('open-sidebar')"
        >
          <PanelLeft :size="18" />
        </button>
        <button
          class="icon-button"
          type="button"
          :title="t('session.researchOverview')"
          :aria-label="t('session.researchOverview')"
          @click="navigateBack"
        >
          <ArrowLeft :size="17" />
        </button>
        <div class="session-heading-copy">
          <p class="eyebrow">{{ research.name }}<template v-if="view.session.origin === 'workflow_agent'"> · {{ t('session.workflowAgent') }}</template></p>
          <h1 :title="view.session.title">{{ view.session.title }}</h1>
        </div>
      </div>
      <div class="header-actions">
        <span class="stream-state" :data-connected="streamConnected" :title="t('session.eventStream')">
          <Radio :size="13" />
        </span>
        <StatusBadge :status="view.session.status" />
        <button
          class="icon-button"
          type="button"
          :title="t('session.startWorkflow')"
          :aria-label="t('session.startWorkflow')"
          @click="$emit('open-workflow')"
        >
          <GitBranch :size="16" />
        </button>
        <button
          class="icon-button inspector-toggle"
          type="button"
          :title="t('session.context')"
          :aria-label="t('session.context')"
          @click="inspectorOpen = !inspectorOpen"
        >
          <PanelRight :size="17" />
        </button>
      </div>
    </header>

    <div class="session-layout">
      <main class="session-thread">
        <div ref="timeline" class="conversation-timeline">
          <div v-if="view.turns.length === 0" class="thread-empty">
            <ScanSearch :size="24" />
            <h2>{{ t('session.prompt') }}</h2>
          </div>

          <article v-for="turn in view.turns" :key="turn.id" class="turn-block">
            <div class="user-message">
              <p>{{ turn.input }}</p>
              <time>{{ formatDateTime(turn.created_at) }}</time>
            </div>

            <details
              v-if="stepsFor(turn.id).length || turn.status !== 'completed'"
              class="execution-details"
              :open="['queued', 'running', 'waiting_for_human', 'paused'].includes(turn.status)"
            >
              <summary>
                <span class="turn-state-icon" :data-status="turn.status">
                  <LoaderCircle v-if="turn.status === 'queued' || turn.status === 'running'" class="spin" :size="14" />
                  <CircleHelp v-else-if="turn.status === 'waiting_for_human'" :size="14" />
                  <Pause v-else-if="turn.status === 'paused'" :size="14" />
                  <CircleCheck v-else-if="turn.status === 'completed'" :size="14" />
                  <CircleX v-else :size="14" />
                </span>
                <span>{{ executionLabel(turn) }}</span>
                <span class="execution-meta">
                  {{ stepsFor(turn.id).length }} {{ t(stepsFor(turn.id).length === 1 ? 'session.step' : 'session.steps') }}
                  <template v-if="turn.usage.input_tokens + turn.usage.output_tokens">
                    · {{ formatCount(turn.usage.input_tokens + turn.usage.output_tokens) }} {{ t('session.tokens') }}
                  </template>
                  <template v-if="turn.usage.cached_input_tokens">
                    · {{ formatCount(turn.usage.cached_input_tokens) }} {{ t('session.cachedTokens') }}
                  </template>
                  <template v-if="turn.usage.cache_write_input_tokens">
                    · {{ formatCount(turn.usage.cache_write_input_tokens) }} {{ t('session.cacheWriteTokens') }}
                  </template>
                </span>
                <ChevronDown :size="14" />
              </summary>
              <div class="step-list">
                <details v-for="step in stepsFor(turn.id)" :key="step.id" class="step-row">
                  <summary>
                    <span class="step-kind-icon" :data-kind="step.kind">
                      <TerminalSquare v-if="step.kind === 'tool'" :size="13" />
                      <BrainCircuit v-else-if="step.kind === 'model'" :size="13" />
                      <GitBranch v-else-if="step.kind === 'workflow'" :size="13" />
                      <Info v-else :size="13" />
                    </span>
                    <span class="step-name">{{ step.name }}</span>
                    <span class="step-meta">
                      {{ statusLabel(step.status) }}
                      <template v-if="step.usage.cached_input_tokens"> · {{ formatCount(step.usage.cached_input_tokens) }} {{ t('session.cachedTokens') }}</template>
                      <template v-if="step.usage.cache_write_input_tokens"> · {{ formatCount(step.usage.cache_write_input_tokens) }} {{ t('session.cacheWriteTokens') }}</template>
                      <template v-if="step.duration_ms !== null"> · {{ formatDuration(step.duration_ms) }}</template>
                    </span>
                    <ChevronRight :size="13" />
                  </summary>
                  <div class="step-payload">
                    <div>
                      <span>{{ t('common.input') }}</span>
                      <pre>{{ pretty(step.input) }}</pre>
                    </div>
                    <div v-if="step.output !== null">
                      <span>{{ t('common.output') }}</span>
                      <pre>{{ pretty(step.output) }}</pre>
                    </div>
                  </div>
                </details>
                <div v-for="notice in noticesFor(turn.id)" :key="notice.id" class="execution-notice">
                  <TriangleAlert :size="13" />
                  <span>{{ noticeText(notice) }}</span>
                </div>
              </div>
            </details>

            <div v-if="turn.output" class="assistant-message">
              <MarkdownView :source="turn.output" />
            </div>
            <div v-else-if="liveOutput(turn.id)" class="assistant-message assistant-message--live">
              <MarkdownView :source="liveOutput(turn.id)" />
              <span class="stream-caret" />
            </div>
            <div v-else-if="['queued', 'running'].includes(turn.status)" class="assistant-pending">
              <span /><span /><span />
            </div>
            <div v-else-if="turn.error" class="turn-error" role="alert">
              <CircleAlert :size="15" />
              <span>{{ turn.error }}</span>
            </div>
          </article>
        </div>

        <footer class="session-composer-region">
          <form class="session-composer" @submit.prevent="send">
            <textarea
              ref="composer"
              v-model="draft"
              rows="1"
              :disabled="Boolean(activeTurn)"
              :placeholder="activeTurn ? t('session.running') : t('session.message')"
              :aria-label="t('session.message')"
              @input="resizeComposer"
              @keydown.enter="onComposerEnter"
            />
            <div class="composer-toolbar">
              <div>
                <span class="model-label"><Cpu :size="13" /> {{ view.session.model }}</span>
                <span v-if="view.session.enabled_skills.length" class="model-label">
                  <Sparkles :size="13" /> {{ view.session.enabled_skills.length }}
                </span>
              </div>
              <button
                v-if="activeTurn"
                class="composer-action composer-action--stop"
                type="button"
                :title="t('session.stopTurn')"
                :aria-label="t('session.stopTurn')"
                @click="$emit('cancel-turn', activeTurn.id)"
              >
                <Square :size="13" fill="currentColor" />
              </button>
              <button
                v-else
                class="composer-action"
                type="submit"
                :title="t('common.send')"
                :aria-label="t('common.send')"
                :disabled="!draft.trim()"
              >
                <ArrowUp :size="17" />
              </button>
            </div>
          </form>
        </footer>
      </main>

      <div v-if="inspectorOpen" class="inspector-mobile-backdrop" @click="inspectorOpen = false" />
      <aside class="session-inspector" :data-mobile-open="inspectorOpen">
        <header class="inspector-header">
          <div>
            <p class="eyebrow">Session</p>
            <h2>{{ t('session.context') }}</h2>
          </div>
          <button
            class="icon-button inspector-mobile-close"
            type="button"
            :title="t('session.closeContext')"
            :aria-label="t('session.closeContext')"
            @click="inspectorOpen = false"
          >
            <X :size="16" />
          </button>
        </header>

        <section class="inspector-section access-profile-control">
          <div class="inspector-title-row">
            <h3>{{ t('session.access') }}</h3>
            <span>{{ accessLabel(view.session.access) }}</span>
          </div>
          <select
            class="select-input"
            :value="view.session.access"
            :disabled="Boolean(activeTurn) || accessBusy"
            @change="requestAccessChange"
          >
            <option v-for="profile in accessProfiles" :key="profile.value" :value="profile.value">
              {{ profile.label }}
            </option>
          </select>
          <p>{{ accessDescription(view.session.access) }}</p>
          <small v-if="activeTurn">{{ t('session.accessLocked') }}</small>
        </section>

        <section class="inspector-section session-facts">
          <dl>
            <div><dt>{{ t('common.model') }}</dt><dd>{{ view.session.model }}</dd></div>
            <div><dt>Session</dt><dd>{{ shortId(view.session.id) }}</dd></div>
            <div><dt>{{ t('common.updated') }}</dt><dd>{{ formatDateTime(view.session.updated_at) }}</dd></div>
          </dl>
          <p v-if="view.session.instructions">{{ view.session.instructions }}</p>
        </section>

        <section class="inspector-section">
          <div class="inspector-title-row">
            <h3>{{ t('session.skills') }}</h3>
            <span>{{ enabledSkills.length }}</span>
          </div>
          <div v-if="skills.length" class="inspector-skill-list">
            <label v-for="skill in skills" :key="skill.slug">
              <input
                v-model="enabledSkills"
                type="checkbox"
                :value="skill.slug"
                :disabled="Boolean(activeTurn) || skillsBusy"
                @change="saveSkills"
              />
              <span>
                <strong>{{ skill.name }}</strong>
                <small>{{ skill.slug }}</small>
              </span>
            </label>
          </div>
          <p v-else class="section-empty">{{ t('research.noSkills') }}</p>
        </section>

        <section class="inspector-section workflow-inspector-section">
          <div class="inspector-title-row">
            <h3>{{ t('session.workflowRuns') }}</h3>
            <button
              class="icon-button"
              type="button"
              :title="t('session.startWorkflow')"
              :aria-label="t('session.startWorkflow')"
              @click="$emit('open-workflow')"
            >
              <Plus :size="14" />
            </button>
          </div>
          <div v-if="view.workflow_runs.length" class="workflow-run-list">
            <button
              v-for="workflowRun in view.workflow_runs"
              :key="workflowRun.id"
              type="button"
              :data-active="workflowRunView?.workflow_run.id === workflowRun.id"
              @click="$emit('inspect-workflow', workflowRun.id)"
            >
              <GitBranch :size="14" />
              <span>
                <strong>{{ workflowRun.objective }}</strong>
                <small>{{ formatDateTime(workflowRun.updated_at) }}</small>
              </span>
              <span class="status-pin" :data-status="workflowRun.status" />
            </button>
          </div>
          <p v-else class="section-empty">{{ t('session.noWorkflowRuns') }}</p>

          <div v-if="workflowLoading" class="inspector-loading"><LoaderCircle class="spin" :size="16" /></div>
          <div v-else-if="workflowRunView" class="workflow-run-detail">
            <div class="workflow-detail-heading">
              <StatusBadge :status="workflowRunView.workflow_run.status" />
              <span class="workflow-detail-controls">
                <button
                  v-if="workflowRunView.workflow_run.status === 'running'"
                  class="icon-button"
                  type="button"
                  :title="t('session.pauseWorkflow')"
                  :aria-label="t('session.pauseWorkflow')"
                  @click="$emit('pause-workflow', workflowRunView.workflow_run.id)"
                >
                  <Pause :size="12" fill="currentColor" />
                </button>
                <button
                  v-if="workflowRunView.workflow_run.status === 'paused'"
                  class="icon-button"
                  type="button"
                  :title="t('session.resumeWorkflow')"
                  :aria-label="t('session.resumeWorkflow')"
                  @click="$emit('resume-workflow', workflowRunView.workflow_run.id)"
                >
                  <Play :size="12" fill="currentColor" />
                </button>
              <button
                v-if="workflowIsActive"
                class="icon-button danger-hover"
                type="button"
                :title="t('session.cancelWorkflow')"
                :aria-label="t('session.cancelWorkflow')"
                @click="$emit('cancel-workflow', workflowRunView.workflow_run.id)"
              >
                <Square :size="12" fill="currentColor" />
              </button>
              </span>
            </div>

            <button
              v-if="workflowRunView.workflow_run.output !== null"
              class="workflow-output-button"
              type="button"
              @click="$emit('open-workflow-output', workflowRunView.workflow_run)"
            >
              <FileText :size="14" />
              <span>{{ t('session.viewWorkflowOutput') }}</span>
              <ExternalLink :size="12" />
            </button>

            <div v-if="currentParticipant && workflowIsActive" class="workflow-control-box">
              <textarea
                v-model="controlDraft"
                class="text-area"
                rows="2"
                :placeholder="t('session.guidePlaceholder')"
                :aria-label="t('session.guidance')"
              />
              <div>
                <button class="secondary-button" type="button" :disabled="!controlDraft.trim()" @click="submitControl('guide')">
                  <SendHorizontal :size="13" /> {{ t('session.guide') }}
                </button>
                <button class="secondary-button danger-hover" type="button" :disabled="!controlDraft.trim()" @click="submitControl('interrupt')">
                  <OctagonX :size="13" /> {{ t('session.interrupt') }}
                </button>
                <button
                  v-if="currentAction"
                  class="secondary-button"
                  type="button"
                  @click="submitControl('finish')"
                >
                  <Flag :size="13" /> {{ t('session.finishAction') }}
                </button>
              </div>
            </div>

            <div v-if="openHumanRequests.length" class="human-request-list">
              <div v-for="request in openHumanRequests" :key="request.id">
                <span><CircleHelp :size="13" /> {{ t('session.humanInput') }}</span>
                <strong>{{ request.question }}</strong>
                <div v-if="request.response_schema.type === 'boolean'" class="human-request-actions">
                  <button class="primary-button" type="button" @click="submitBooleanHumanAnswer(request, true)">
                    {{ t('common.approve') }}
                  </button>
                  <button class="secondary-button" type="button" @click="submitBooleanHumanAnswer(request, false)">
                    {{ t('common.deny') }}
                  </button>
                </div>
                <textarea
                  v-else
                  v-model="humanAnswers[request.id]"
                  class="text-area"
                  rows="2"
                  :placeholder="t('session.answer')"
                  :aria-label="t('session.answerAria', { question: request.question })"
                />
                <button v-if="request.response_schema.type !== 'boolean'" class="primary-button" type="button" :disabled="!humanAnswers[request.id]?.trim()" @click="submitHumanAnswer(request)">
                  <SendHorizontal :size="13" /> {{ t('session.answer') }}
                </button>
              </div>
              <p v-if="humanAnswerError" class="form-error">{{ humanAnswerError }}</p>
            </div>

            <div v-if="workflowRunView.participants.length" class="participant-session-list">
              <button
                v-for="(participant, index) in workflowRunView.participants"
                :key="participant.id"
                type="button"
                @click="$emit('select-session', participant.session_id)"
              >
                <span class="participant-number">{{ String(index + 1).padStart(2, '0') }}</span>
                <span>
                  <strong>{{ participant.name }}</strong>
                  <small>{{ participant.role }}</small>
                </span>
                <span class="status-pin" :data-status="participant.status" />
              </button>
            </div>

            <div v-if="workflowRunView.actions.length" class="workflow-action-list">
              <div v-for="action in workflowRunView.actions.slice(-6).reverse()" :key="action.id">
                <Activity :size="13" />
                <span>
                  <strong>{{ action.action_name }}</strong>
                  <small>{{ participantName(action.agent_instance_id) }}</small>
                </span>
                <span class="status-pin" :data-status="action.status" />
              </div>
            </div>

            <div v-if="workflowRunView.timers.length" class="workflow-action-list">
              <div v-for="timer in workflowRunView.timers" :key="timer.id">
                <AlarmClock :size="13" />
                <span>
                  <strong>{{ timer.name }}</strong>
                  <small>{{ t('session.timerFires', { count: timer.fire_count }) }} · {{ timer.policy }}</small>
                </span>
                <span class="status-pin" :data-status="timer.status" />
              </div>
            </div>
            <div v-if="workflowRunView.artifacts.length" class="inspector-artifact-list">
              <button
                v-for="artifact in workflowRunView.artifacts"
                :key="artifact.id"
                type="button"
                @click="$emit('open-artifact', artifact)"
              >
                <FileText :size="13" />
                <span>{{ artifact.name }}</span>
                <ExternalLink :size="12" />
              </button>
            </div>
          </div>
        </section>

      </aside>
    </div>
  </div>
</template>

<script setup lang="ts">
import {
  Activity,
  AlarmClock,
  ArrowLeft,
  ArrowUp,
  BrainCircuit,
  ChevronDown,
  ChevronRight,
  CircleAlert,
  CircleCheck,
  CircleHelp,
  CircleX,
  Cpu,
  ExternalLink,
  FileText,
  Flag,
  GitBranch,
  Info,
  LoaderCircle,
  PanelLeft,
  PanelRight,
  Pause,
  Play,
  Plus,
  Radio,
  ScanSearch,
  SendHorizontal,
  Sparkles,
  Square,
  TerminalSquare,
  TriangleAlert,
  OctagonX,
  X,
} from '@lucide/vue'
import { computed, nextTick, reactive, ref, watch } from 'vue'
import { formatCount, formatDateTime, formatDuration, shortId, statusLabel } from '../format'
import { useAppI18n } from '../i18n'
import { liveAssistantOutput } from '../sessionEvents'
import type {
  AgentAccessProfile,
  Artifact,
  Research,
  ResearchSkill,
  SessionEvent,
  SessionView,
  Turn,
  HumanRequest,
  WorkflowRunView,
  WorkflowRun,
} from '../types'
import MarkdownView from './MarkdownView.vue'
import StatusBadge from './StatusBadge.vue'

const props = defineProps<{
  research: Research
  view: SessionView
  events: SessionEvent[]
  skills: ResearchSkill[]
  workflowRunView: WorkflowRunView | null
  workflowLoading: boolean
  streamConnected: boolean
  skillsBusy: boolean
  accessBusy: boolean
}>()
const emit = defineEmits<{
  'open-sidebar': []
  'select-research': [researchId: string]
  'select-session': [sessionId: string]
  send: [input: string]
  'cancel-turn': [turnId: string]
  'open-workflow': []
  'inspect-workflow': [workflowRunId: string]
  'pause-workflow': [workflowRunId: string]
  'resume-workflow': [workflowRunId: string]
  'cancel-workflow': [workflowRunId: string]
  'send-control': [input: { workflowRunId: string; sessionId: string; kind: 'guide' | 'interrupt' | 'finish'; content: string; actionInvocationId?: string }]
  'answer-human': [input: { requestId: string; answer: unknown; workflowRunId: string }]
  'update-skills': [slugs: string[]]
  'update-access': [access: AgentAccessProfile]
  'open-artifact': [artifact: Artifact]
  'open-workflow-output': [workflowRun: WorkflowRun]
}>()

const draft = ref('')
const composer = ref<HTMLTextAreaElement | null>(null)
const timeline = ref<HTMLElement | null>(null)
const inspectorOpen = ref(false)
const enabledSkills = ref<string[]>([...props.view.session.enabled_skills])
const controlDraft = ref('')
const humanAnswers = reactive<Record<string, string>>({})
const humanAnswerError = ref('')
const { t } = useAppI18n()
const accessProfiles = computed(() => [
  { value: 'model_only' as const, label: t('access.modelOnly') },
  { value: 'read_only' as const, label: t('access.readOnly') },
  { value: 'workspace' as const, label: t('access.workspace') },
  { value: 'research' as const, label: t('access.research') },
  { value: 'full_access' as const, label: t('access.fullAccess') },
])

const activeTurn = computed(() =>
  props.view.turns.find((turn) => ['queued', 'running', 'waiting_for_human', 'paused'].includes(turn.status)),
)
const workflowIsActive = computed(() =>
  ['created', 'running', 'paused'].includes(props.workflowRunView?.workflow_run.status ?? ''),
)
const currentParticipant = computed(() =>
  props.workflowRunView?.participants.find((participant) => participant.session_id === props.view.session.id) ?? null,
)
const currentAction = computed(() =>
  props.workflowRunView?.actions.find(
    (action) => action.session_id === props.view.session.id && action.status === 'running',
  ) ?? null,
)
const openHumanRequests = computed(() =>
  props.workflowRunView?.human_requests.filter((request) => request.status === 'open') ?? [],
)

watch(
  () => props.view.session.id,
  () => {
    draft.value = ''
    enabledSkills.value = [...props.view.session.enabled_skills]
    inspectorOpen.value = false
    controlDraft.value = ''
    humanAnswerError.value = ''
    nextTick(scrollToBottom)
  },
)
watch(
  () => props.view.session.enabled_skills,
  (value) => {
    enabledSkills.value = [...value]
  },
  { deep: true },
)
watch(
  () => props.events.length + props.view.turns.length + props.view.steps.length,
  async () => {
    const element = timeline.value
    const wasNearBottom = !element || element.scrollHeight - element.scrollTop - element.clientHeight < 160
    await nextTick()
    if (wasNearBottom) scrollToBottom()
  },
)

function stepsFor(turnId: string) {
  return props.view.steps.filter((step) => step.turn_id === turnId)
}
function noticesFor(turnId: string) {
  return props.events.filter(
    (event) => event.turn_id === turnId && ['sampling_retry', 'context_trimmed', 'warning'].includes(event.type),
  )
}
function liveOutput(turnId: string): string {
  return liveAssistantOutput(props.events, turnId)
}
function noticeText(event: SessionEvent): string {
  if (event.type === 'context_trimmed') return t('session.contextCompacted', { count: String(event.removed_items ?? 0) })
  if (event.type === 'sampling_retry') {
    return t('session.modelRetry', { attempt: String(event.attempt ?? ''), error: String(event.error ?? '') })
  }
  return String(event.message ?? t('session.warning'))
}
function executionLabel(turn: Turn): string {
  if (turn.status === 'running') return statusLabel('working')
  if (turn.status === 'cancelled') return statusLabel('stopped')
  return statusLabel(turn.status)
}
function pretty(value: unknown): string {
  if (typeof value === 'string') return value
  try {
    return JSON.stringify(value, null, 2)
  } catch {
    return String(value)
  }
}
function navigateBack() {
  emit('select-research', props.research.id)
}
function participantName(agentId: string) {
  return props.workflowRunView?.participants.find((participant) => participant.id === agentId)?.name ?? shortId(agentId)
}
function submitControl(kind: 'guide' | 'interrupt' | 'finish') {
  const content = controlDraft.value.trim() || (kind === 'finish' ? t('session.finishDefault') : '')
  const run = props.workflowRunView?.workflow_run
  if (!content || !run) return
  emit('send-control', {
    workflowRunId: run.id,
    sessionId: props.view.session.id,
    kind,
    content,
    actionInvocationId: currentAction.value?.id,
  })
  controlDraft.value = ''
}
function submitHumanAnswer(request: HumanRequest) {
  const raw = humanAnswers[request.id]?.trim()
  if (!raw) return
  let answer: unknown = raw
  if (request.response_schema.type && request.response_schema.type !== 'string') {
    try {
      answer = JSON.parse(raw)
    } catch {
      humanAnswerError.value = t('session.validJsonAnswer')
      return
    }
  }
  humanAnswerError.value = ''
  emit('answer-human', { requestId: request.id, answer, workflowRunId: request.workflow_run_id })
  delete humanAnswers[request.id]
}
function submitBooleanHumanAnswer(request: HumanRequest, answer: boolean) {
  humanAnswerError.value = ''
  emit('answer-human', { requestId: request.id, answer, workflowRunId: request.workflow_run_id })
}
function requestAccessChange(event: Event) {
  const select = event.target as HTMLSelectElement
  const access = select.value as AgentAccessProfile
  if (access === props.view.session.access) return
  if (access === 'full_access' && !window.confirm(t('session.fullAccessConfirm'))) {
    select.value = props.view.session.access
    return
  }
  emit('update-access', access)
}
function accessLabel(access: AgentAccessProfile): string {
  return accessProfiles.value.find((profile) => profile.value === access)?.label ?? access
}
function accessDescription(access: AgentAccessProfile): string {
  if (access === 'model_only') return t('access.modelOnlyDescription')
  if (access === 'read_only') return t('access.readOnlyDescription')
  if (access === 'workspace') return t('access.workspaceDescription')
  if (access === 'research') return t('access.researchDescription')
  return t('access.fullAccessDescription')
}
function send() {
  const value = draft.value.trim()
  if (!value || activeTurn.value) return
  emit('send', value)
  draft.value = ''
  nextTick(resizeComposer)
}
function onComposerEnter(event: KeyboardEvent) {
  if (event.shiftKey || event.isComposing) return
  event.preventDefault()
  send()
}
function resizeComposer() {
  const element = composer.value
  if (!element) return
  element.style.height = 'auto'
  element.style.height = `${Math.min(element.scrollHeight, 180)}px`
}
function scrollToBottom() {
  const element = timeline.value
  if (element) element.scrollTop = element.scrollHeight
}
function saveSkills() {
  emit('update-skills', [...enabledSkills.value])
}
</script>
