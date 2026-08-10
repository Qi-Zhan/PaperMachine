<template>
  <div class="session-workspace">
    <header class="page-header session-page-header">
      <div class="page-leading session-heading">
        <button
          class="icon-button sidebar-toggle"
          type="button"
          :title="t('common.toggleSidebar')"
          :aria-label="t('common.toggleSidebar')"
          @click="$emit('toggle-sidebar')"
        >
          <PanelLeft :size="18" />
        </button>
        <button
          class="icon-button"
          type="button"
          :title="t('session.projectOverview')"
          :aria-label="t('session.projectOverview')"
          @click="navigateBack"
        >
          <ArrowLeft :size="17" />
        </button>
        <div class="session-heading-copy">
          <p class="eyebrow">{{ project.name }}</p>
          <h1 :title="view.session.title">{{ view.session.title }}</h1>
        </div>
      </div>
    </header>

    <main class="session-thread">
      <div ref="timeline" class="conversation-timeline">
        <div v-if="view.turns.length === 0" class="thread-empty">
          <ScanSearch :size="24" />
          <h2>{{ t('session.prompt') }}</h2>
        </div>

        <article v-for="turn in view.turns" :key="turn.id" class="turn-block">
          <div :class="turnIsHumanTriggered(turn.id) ? 'user-message' : 'workflow-message'">
            <span v-if="!turnIsHumanTriggered(turn.id)" class="message-origin">
              <GitBranch :size="12" />
              {{ agentForTurn(turn)?.name ?? t('session.workflowTask') }}
              <template v-if="actionForTurn(turn.id)"> · {{ actionForTurn(turn.id)?.action_name }}</template>
            </span>
            <p>{{ turnMessage(turn) }}</p>
            <time>{{ formatDateTime(turn.created_at) }}</time>
          </div>

          <section
            v-if="turn.prompt.layers.length || stepsFor(turn.id).length || turn.status !== 'completed'"
            class="execution-trace"
          >
            <div v-if="activityStepsFor(turn.id).length || noticesFor(turn.id).length" class="activity-list">
              <details v-for="step in activityStepsFor(turn.id)" :key="step.id" class="activity-row">
                <summary>
                  <span class="activity-icon" :data-kind="activityKind(step)">
                    <Search v-if="activityKind(step) === 'search'" :size="17" />
                    <FileText v-else-if="activityKind(step) === 'read'" :size="17" />
                    <TerminalSquare v-else-if="activityKind(step) === 'command'" :size="17" />
                    <Save v-else-if="activityKind(step) === 'edit'" :size="17" />
                    <Activity v-else :size="17" />
                  </span>
                  <span class="activity-copy">
                    <strong>{{ activityLabel(step) }}</strong>
                    <small v-if="activitySubject(step)">{{ activitySubject(step) }}</small>
                  </span>
                  <span class="activity-meta">
                    <template v-if="step.status !== 'completed'">{{ statusLabel(step.status) }}</template>
                    <template v-if="step.duration_ms !== null"><span v-if="step.status !== 'completed'"> · </span>{{ formatDuration(step.duration_ms) }}</template>
                  </span>
                  <ChevronDown :size="14" />
                </summary>
                <div class="activity-payload step-payload">
                  <dl v-if="step.tool_call_id" class="tool-effect-meta">
                    <div v-if="step.tool_call_id"><dt>{{ t('session.toolCallId') }}</dt><dd>{{ step.tool_call_id }}</dd></div>
                  </dl>
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
              <div v-for="notice in noticesFor(turn.id)" :key="notice.id" class="activity-notice">
                <TriangleAlert v-if="notice.type !== 'context_trimmed'" :size="17" />
                <BrainCircuit v-else :size="17" />
                <span>{{ noticeText(notice) }}</span>
              </div>
            </div>

            <details
              class="execution-details"
              :open="['queued', 'running', 'paused'].includes(turn.status) && activityStepsFor(turn.id).length === 0"
            >
              <summary>
                <span class="turn-state-icon" :data-status="turn.status">
                  <LoaderCircle v-if="turn.status === 'queued' || turn.status === 'running'" class="spin" :size="15" />
                  <Pause v-else-if="turn.status === 'paused'" :size="15" />
                  <CircleCheck v-else-if="turn.status === 'completed'" :size="15" />
                  <CircleX v-else :size="15" />
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
              <details class="prompt-snapshot-row">
                <summary>
                  <span class="step-kind-icon" data-kind="system"><Info :size="13" /></span>
                  <span class="step-name">{{ t('session.environment') }}</span>
                  <span class="step-meta">
                    {{ accessLabel(turn.environment.authorization.preset) }} · {{ turn.environment.authorization_sha256.slice(0, 10) }}
                  </span>
                  <ChevronRight :size="13" />
                </summary>
                <div class="step-payload">
                  <div>
                    <span>{{ t('session.workspaceAttachment') }} · r{{ turn.environment.workspace.revision }}</span>
                    <pre>{{ pretty({ id: turn.environment.workspace.id, path: turn.environment.workspace.path, cwd: turn.environment.cwd }) }}</pre>
                  </div>
                  <div>
                    <span>{{ t('session.materializedAuthorization') }} · {{ turn.environment.authorization_sha256 }}</span>
                    <pre>{{ pretty(turn.environment.authorization) }}</pre>
                  </div>
                </div>
              </details>
              <details v-if="actionForTurn(turn.id)" class="prompt-snapshot-row">
                <summary>
                  <span class="step-kind-icon" data-kind="workflow"><GitBranch :size="13" /></span>
                  <span class="step-name">{{ actionForTurn(turn.id)?.action_name }}</span>
                  <span class="step-meta">{{ t('session.actionInput') }}</span>
                  <ChevronRight :size="13" />
                </summary>
                <div class="step-payload">
                  <div>
                    <span>{{ t('session.actionInput') }}</span>
                    <pre>{{ prettyActionArguments(actionForTurn(turn.id)?.arguments) }}</pre>
                  </div>
                </div>
              </details>
              <details v-if="turn.prompt.layers.length" class="prompt-snapshot-row">
                <summary>
                  <span class="step-kind-icon" data-kind="system"><Info :size="13" /></span>
                  <span class="step-name">{{ t('prompt.snapshot') }}</span>
                  <span class="step-meta">
                    {{ turn.prompt.layers.length }} {{ t('prompt.layers') }} · {{ turn.prompt.sha256.slice(0, 10) }}
                  </span>
                  <ChevronRight :size="13" />
                </summary>
                <div class="prompt-layer-list">
                  <details v-for="layer in turn.prompt.layers" :key="`${layer.kind}:${layer.source}:${layer.sha256}`">
                    <summary>
                      <strong>{{ layer.name }}</strong>
                      <span>{{ layer.kind }} · {{ layer.source }}</span>
                    </summary>
                    <pre>{{ layer.content }}</pre>
                  </details>
                </div>
              </details>
              <details v-for="step in technicalStepsFor(turn.id)" :key="step.id" class="step-row">
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
              </div>
            </details>
          </section>

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
            :disabled="composerDisabled"
            :placeholder="composerPlaceholder"
            :aria-label="t('session.message')"
            @input="resizeComposer"
            @keydown.enter="onComposerEnter"
          />
          <div class="composer-toolbar">
            <div>
              <span class="model-label"><Cpu :size="13" /> {{ view.session.default_model }}</span>
              <span v-if="view.session.enabled_skills.length" class="model-label">
                <Sparkles :size="13" /> {{ view.session.enabled_skills.length }}
              </span>
            </div>
            <button
              v-if="activeTurn && !composerHumanRequest"
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
              :disabled="composerDisabled || !draft.trim()"
            >
              <ArrowUp :size="17" />
            </button>
          </div>
        </form>
      </footer>
    </main>
  </div>
</template>

<script setup lang="ts">
import {
  Activity,
  ArrowLeft,
  ArrowUp,
  BrainCircuit,
  ChevronDown,
  ChevronRight,
  CircleAlert,
  CircleCheck,
  CircleX,
  Cpu,
  FileText,
  GitBranch,
  Info,
  LoaderCircle,
  PanelLeft,
  Pause,
  Save,
  ScanSearch,
  Search,
  Sparkles,
  Square,
  TerminalSquare,
  TriangleAlert,
} from '@lucide/vue'
import { computed, nextTick, ref, watch } from 'vue'
import {
  agentActivityKind,
  agentActivitySubject,
  formatCount,
  formatDateTime,
  formatDuration,
  primaryActionText,
  statusLabel,
} from '../format'
import { useAppI18n } from '../i18n'
import type {
  AccessPreset,
  AgentStep,
  Project,
  SessionEvent,
  SessionView,
  Turn,
} from '../types'
import MarkdownView from './MarkdownView.vue'

const props = defineProps<{
  project: Project
  workspaceAvailable: boolean
  view: SessionView
  events: SessionEvent[]
  liveOutputs: Record<string, string>
}>()
const emit = defineEmits<{
  'toggle-sidebar': []
  'select-project': [projectId: string]
  'select-session': [projectId: string, sessionId: string]
  send: [input: string]
  'cancel-turn': [turnId: string]
}>()

const draft = ref('')
const composer = ref<HTMLTextAreaElement | null>(null)
const timeline = ref<HTMLElement | null>(null)
const { t } = useAppI18n()
const accessProfiles = computed(() => [
  { value: 'model_only' as const, label: t('access.modelOnly') },
  { value: 'read_only' as const, label: t('access.readOnly') },
  { value: 'workspace' as const, label: t('access.workspace') },
  { value: 'research' as const, label: t('access.research') },
  { value: 'full_access' as const, label: t('access.fullAccess') },
])

const activeTurn = computed(() =>
  props.view.turns.find((turn) => ['queued', 'running', 'paused'].includes(turn.status)),
)
const sessionIsArchived = computed(() => props.view.session.archived_at !== null)
const composerHumanRequest = computed(() =>
  props.view.human_requests.find(
    (request) =>
      request.status === 'open' &&
      request.session_id === props.view.session.id &&
      (!request.response_schema.type || request.response_schema.type === 'string'),
  ),
)
const composerDisabled = computed(
  () => !props.workspaceAvailable || sessionIsArchived.value || !composerHumanRequest.value,
)
const composerPlaceholder = computed(() => {
  if (!props.workspaceAvailable) return t('session.workspaceUnavailable')
  if (sessionIsArchived.value) return t('session.closed')
  if (composerHumanRequest.value) return composerHumanRequest.value.question
  return activeTurn.value ? t('session.running') : t('session.preparingNextTurn')
})
watch(
  () => props.view.session.id,
  () => {
    draft.value = ''
    nextTick(scrollToBottom)
  },
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
function activityStepsFor(turnId: string) {
  return stepsFor(turnId).filter((step) => step.kind === 'tool')
}
function technicalStepsFor(turnId: string) {
  return stepsFor(turnId).filter((step) => step.kind !== 'tool')
}
function activityKind(step: AgentStep) {
  return agentActivityKind(step.name)
}
function activityLabel(step: AgentStep): string {
  const kind = activityKind(step)
  if (kind === 'search') return t('session.activitySearched')
  if (kind === 'read') return t('session.activityRead')
  if (kind === 'command') return t('session.activityRanCommand')
  if (kind === 'edit') return t('session.activityEdited')
  return t('session.activityUsedTool', { name: step.name })
}
function activitySubject(step: AgentStep): string | null {
  return agentActivitySubject(step.input)
}
function actionForTurn(turnId: string) {
  const attempt = props.view.attempts.find((candidate) => candidate.turn_id === turnId)
  if (!attempt) return null
  return props.view.actions.find((action) => action.id === attempt.invocation_id) ?? null
}
function agentForTurn(turn: Turn) {
  return props.view.agents.find((agent) => agent.id === turn.agent_id) ?? null
}
function turnIsHumanTriggered(turnId: string): boolean {
  return actionForTurn(turnId)?.source_human_request_id != null
}
function turnMessage(turn: Turn): string {
  const primary = primaryActionText(actionForTurn(turn.id)?.arguments)
  if (primary) return primary
  return actionForTurn(turn.id) ? t('session.workflowStructuredInput') : turn.input
}
function prettyActionArguments(value: unknown): string {
  const rendered = pretty(value)
  const limit = 20_000
  return rendered.length <= limit ? rendered : `${rendered.slice(0, limit)}\n…`
}
function noticesFor(turnId: string) {
  return props.events.filter(
    (event) => event.turn_id === turnId && ['sampling_retry', 'context_trimmed', 'warning'].includes(event.type),
  )
}
function liveOutput(turnId: string): string {
  return props.liveOutputs[turnId] ?? ''
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
  emit('select-project', props.project.id)
}
function accessLabel(access: AccessPreset): string {
  return accessProfiles.value.find((profile) => profile.value === access)?.label ?? access
}
function send() {
  const value = draft.value.trim()
  if (!value || composerDisabled.value) return
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
</script>
