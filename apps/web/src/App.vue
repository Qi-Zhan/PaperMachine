<template>
  <div class="app-layout">
    <div v-if="mobileSidebarOpen" class="mobile-sidebar-backdrop" @click="mobileSidebarOpen = false" />
    <div class="sidebar-region" :data-mobile-open="mobileSidebarOpen">
      <AppSidebar
        :researches="researches"
        :sessions-by-research="sessionsByResearch"
        :selected-research-id="selectedResearchId"
        :selected-session-id="selectedSessionId"
        :mode="health?.model_mode ?? 'demo'"
        :online="online"
        :workflows-active="workflowLibraryOpen"
        @home="showHome"
        @close-sidebar="mobileSidebarOpen = false"
        @new-research="researchDialogOpen = true"
        @new-session="openSessionDialog"
        @open-workflows="openWorkflowLibrary"
        @select-research="selectResearch"
        @select-session="selectSession"
      />
    </div>

    <section class="main-region">
      <div v-if="initialLoading" class="full-loading">
        <ScanSearch :size="22" />
        <LoaderCircle class="spin" :size="18" />
      </div>

      <WorkflowLibrary
        v-else-if="workflowLibraryOpen"
        :workflows="workflows"
        @open-sidebar="mobileSidebarOpen = true"
        @saved="workflowSaved"
      />

      <div v-else-if="researches.length === 0" class="zero-state">
        <header class="zero-state-header">
          <button
            class="icon-button mobile-only"
            type="button"
            :title="t('common.openSidebar')"
            :aria-label="t('common.openSidebar')"
            @click="mobileSidebarOpen = true"
          >
            <PanelLeft :size="18" />
          </button>
          <span class="brand-mark zero-mobile-brand"><ScanSearch :size="16" /></span>
          <strong class="zero-mobile-brand">PaperMachine</strong>
          <strong class="zero-desktop-title">Research</strong>
        </header>
        <div class="zero-state-main">
          <FolderSearch2 :size="28" />
          <h1>{{ t('zero.createResearch') }}</h1>
          <button class="primary-button" type="button" @click="researchDialogOpen = true">
            <FolderPlus :size="16" />
            {{ t('sidebar.newResearch') }}
          </button>
        </div>
      </div>

      <SessionWorkspace
        v-else-if="sessionView && selectedResearch"
        :research="selectedResearch"
        :view="sessionView"
        :events="sessionEvents"
        :skills="researchSkills"
        :workflow-run-view="workflowRunView"
        :workflow-loading="workflowLoading"
        :stream-connected="streamConnected"
        :skills-busy="skillsBusy"
        :access-busy="accessBusy"
        @open-sidebar="mobileSidebarOpen = true"
        @select-research="selectResearch"
        @select-session="selectSession"
        @send="createTurn"
        @cancel-turn="cancelTurn"
        @open-workflow="workflowDialogOpen = true"
        @inspect-workflow="inspectWorkflow"
        @pause-workflow="pauseWorkflow"
        @resume-workflow="resumeWorkflow"
        @cancel-workflow="cancelWorkflow"
        @send-control="sendWorkflowControl"
        @answer-human="answerHumanRequest"
        @update-skills="updateSessionSkills"
        @update-access="updateSessionAccess"
        @open-artifact="selectedArtifact = $event"
        @open-workflow-output="selectedWorkflowOutput = $event"
      />

      <ResearchOverview
        v-else-if="researchOverview"
        :overview="researchOverview"
        :skills="researchSkills"
        @open-sidebar="mobileSidebarOpen = true"
        @new-session="openSessionDialog(researchOverview.research.id)"
        @new-skill="skillDialogOpen = true"
        @select-session="selectSession"
        @open-artifact="selectedArtifact = $event"
      />

      <div v-else class="full-loading"><LoaderCircle class="spin" :size="18" /></div>

      <div v-if="globalError" class="global-error" role="alert">
        <AlertCircle :size="16" />
        <span>{{ globalError }}</span>
        <button type="button" :title="t('common.dismissError')" :aria-label="t('common.dismissError')" @click="globalError = ''">
          <X :size="15" />
        </button>
      </div>
    </section>

    <NewResearchDialog
      :open="researchDialogOpen"
      :busy="dialogBusy"
      :error="dialogError"
      @close="closeDialogs"
      @submit="createResearch"
    />
    <NewSessionDialog
      :open="sessionDialogOpen"
      :busy="dialogBusy"
      :error="dialogError"
      :research="dialogResearch"
      :skills="dialogSkills"
      :model-profiles="health?.model_profiles ?? []"
      :default-model="health?.default_model ?? ''"
      @close="closeDialogs"
      @submit="createSession"
    />
    <NewSkillDialog
      :open="skillDialogOpen"
      :busy="dialogBusy"
      :error="dialogError"
      :research-name="selectedResearch?.name ?? ''"
      @close="closeDialogs"
      @submit="createSkill"
    />
    <WorkflowRunDialog
      :open="workflowDialogOpen"
      :busy="dialogBusy"
      :error="dialogError"
      :session="sessionView?.session ?? null"
      :workflows="workflows"
      @close="closeDialogs"
      @submit="createWorkflowRun"
    />
    <ArtifactDialog :artifact="selectedArtifact" @close="selectedArtifact = null" />
    <WorkflowOutputDialog :run="selectedWorkflowOutput" @close="selectedWorkflowOutput = null" />
  </div>
</template>

<script setup lang="ts">
import {
  AlertCircle,
  FolderPlus,
  FolderSearch2,
  LoaderCircle,
  PanelLeft,
  ScanSearch,
  X,
} from '@lucide/vue'
import { computed, onBeforeUnmount, onMounted, reactive, ref } from 'vue'
import { api, sessionEventTypes } from './api'
import { useAppI18n } from './i18n'
import AppSidebar from './components/AppSidebar.vue'
import ArtifactDialog from './components/ArtifactDialog.vue'
import NewResearchDialog from './components/NewResearchDialog.vue'
import NewSessionDialog from './components/NewSessionDialog.vue'
import NewSkillDialog from './components/NewSkillDialog.vue'
import ResearchOverview from './components/ResearchOverview.vue'
import SessionWorkspace from './components/SessionWorkspace.vue'
import WorkflowLibrary from './components/WorkflowLibrary.vue'
import WorkflowRunDialog from './components/WorkflowRunDialog.vue'
import WorkflowOutputDialog from './components/WorkflowOutputDialog.vue'
import type {
  AgentAccessProfile,
  Artifact,
  CreateSessionInput,
  Health,
  Research,
  ResearchOverview as ResearchOverviewType,
  ResearchSkill,
  Session,
  SessionEvent,
  SessionView,
  WorkflowRegistration,
  WorkflowRun,
  WorkflowRunView,
} from './types'

const researches = ref<Research[]>([])
const { t } = useAppI18n()
const workflows = ref<WorkflowRegistration[]>([])
const sessionsByResearch = reactive<Record<string, Session[]>>({})
const skillsByResearch = reactive<Record<string, ResearchSkill[]>>({})
const selectedResearchId = ref<string | null>(null)
const selectedSessionId = ref<string | null>(null)
const researchOverview = ref<ResearchOverviewType | null>(null)
const sessionView = ref<SessionView | null>(null)
const sessionEvents = ref<SessionEvent[]>([])
const workflowRunView = ref<WorkflowRunView | null>(null)
const workflowLoading = ref(false)
const health = ref<Health | null>(null)
const online = ref(false)
const streamConnected = ref(false)
const initialLoading = ref(true)
const globalError = ref('')
const dialogError = ref('')
const dialogBusy = ref(false)
const skillsBusy = ref(false)
const accessBusy = ref(false)
const researchDialogOpen = ref(false)
const sessionDialogOpen = ref(false)
const skillDialogOpen = ref(false)
const workflowDialogOpen = ref(false)
const dialogResearchId = ref<string | null>(null)
const mobileSidebarOpen = ref(false)
const selectedArtifact = ref<Artifact | null>(null)
const selectedWorkflowOutput = ref<WorkflowRun | null>(null)
const workflowLibraryOpen = ref(false)

let sessionEventSource: EventSource | null = null
let refreshTimer: number | null = null
let pollTimer: number | null = null

const selectedResearch = computed(
  () => researches.value.find((research) => research.id === selectedResearchId.value) ?? null,
)
const researchSkills = computed(() =>
  selectedResearchId.value ? (skillsByResearch[selectedResearchId.value] ?? []) : [],
)
const dialogResearch = computed(
  () => researches.value.find((research) => research.id === dialogResearchId.value) ?? null,
)
const dialogSkills = computed(() =>
  dialogResearchId.value ? (skillsByResearch[dialogResearchId.value] ?? []) : [],
)

onMounted(async () => {
  window.addEventListener('keydown', onKeydown)
  try {
    const [healthResult, researchResult, workflowResult] = await Promise.all([
      api.health(),
      api.listResearches(),
      api.listWorkflows(),
    ])
    health.value = healthResult
    online.value = true
    researches.value = researchResult
    workflows.value = workflowResult
    await Promise.all(researchResult.map((research) => refreshResearchIndex(research.id)))
    const initialRoute = readRoute()
    let restored = false
    if (initialRoute?.kind === 'session') restored = await selectSession(initialRoute.id)
    else if (initialRoute?.kind === 'research') restored = await selectResearch(initialRoute.id)
    else if (initialRoute?.kind === 'workflows') {
      openWorkflowLibrary()
      restored = true
    }
    if (!restored && researchResult[0]) await selectResearch(researchResult[0].id)
  } catch (error) {
    online.value = false
    globalError.value = messageOf(error)
  } finally {
    initialLoading.value = false
  }
})

onBeforeUnmount(() => {
  window.removeEventListener('keydown', onKeydown)
  closeSessionStream()
  clearTimers()
})

async function refreshResearchIndex(researchId: string) {
  const overview = await api.getResearch(researchId)
  sessionsByResearch[researchId] = overview.sessions
  const index = researches.value.findIndex((research) => research.id === researchId)
  if (index >= 0) researches.value[index] = overview.research
  if (selectedResearchId.value === researchId && !selectedSessionId.value) researchOverview.value = overview
  return overview
}

async function ensureResearchSkills(researchId: string, refresh = false) {
  if (!refresh && skillsByResearch[researchId]) return skillsByResearch[researchId]
  const skills = await api.listResearchSkills(researchId)
  skillsByResearch[researchId] = skills
  return skills
}

async function selectResearch(researchId: string): Promise<boolean> {
  closeSessionStream()
  clearPoll()
  selectedResearchId.value = researchId
  selectedSessionId.value = null
  sessionView.value = null
  sessionEvents.value = []
  workflowRunView.value = null
  workflowLibraryOpen.value = false
  mobileSidebarOpen.value = false
  try {
    const [overview] = await Promise.all([
      refreshResearchIndex(researchId),
      ensureResearchSkills(researchId),
    ])
    if (selectedResearchId.value === researchId && !selectedSessionId.value) {
      researchOverview.value = overview
      writeRoute('research', researchId)
    }
    return true
  } catch (error) {
    if (selectedResearchId.value === researchId) selectedResearchId.value = null
    globalError.value = messageOf(error)
    return false
  }
}

async function selectSession(sessionId: string): Promise<boolean> {
  selectedSessionId.value = sessionId
  sessionView.value = null
  sessionEvents.value = []
  workflowRunView.value = null
  workflowLibraryOpen.value = false
  mobileSidebarOpen.value = false
  closeSessionStream()
  clearPoll()
  try {
    const [view, events] = await Promise.all([api.getSession(sessionId), api.listSessionEvents(sessionId)])
    if (selectedSessionId.value !== sessionId) return false
    selectedResearchId.value = view.session.research_id
    sessionView.value = view
    sessionEvents.value = events
    await Promise.all([
      ensureResearchSkills(view.session.research_id),
      refreshResearchIndex(view.session.research_id),
    ])
    connectSessionStream(sessionId, events.at(-1)?.sequence ?? 0)
    const latestWorkflow = view.workflow_runs[0]
    if (latestWorkflow) void inspectWorkflow(latestWorkflow.id)
    writeRoute('session', sessionId)
    syncPoll()
    return true
  } catch (error) {
    if (selectedSessionId.value === sessionId) selectedSessionId.value = null
    globalError.value = messageOf(error)
    return false
  }
}

function connectSessionStream(sessionId: string, after: number) {
  closeSessionStream()
  const source = new EventSource(`/api/sessions/${sessionId}/events/stream?after=${after}`)
  sessionEventSource = source
  source.onopen = () => {
    streamConnected.value = true
    online.value = true
  }
  source.onerror = () => {
    streamConnected.value = false
  }
  const receive = (message: MessageEvent<string>) => {
    try {
      const event = JSON.parse(message.data) as SessionEvent
      if (!sessionEvents.value.some((candidate) => candidate.id === event.id)) {
        sessionEvents.value.push(event)
        sessionEvents.value.sort((left, right) => left.sequence - right.sequence)
      }
      if (event.type !== 'assistant_message_delta') scheduleSessionRefresh(sessionId)
    } catch {
      globalError.value = t('app.invalidSessionEvent')
    }
  }
  for (const type of sessionEventTypes) source.addEventListener(type, receive as EventListener)
}

function closeSessionStream() {
  sessionEventSource?.close()
  sessionEventSource = null
  streamConnected.value = false
}

function scheduleSessionRefresh(sessionId: string) {
  if (refreshTimer !== null) window.clearTimeout(refreshTimer)
  refreshTimer = window.setTimeout(async () => {
    refreshTimer = null
    if (selectedSessionId.value !== sessionId) return
    await refreshSession(sessionId)
  }, 120)
}

async function refreshSession(sessionId: string) {
  try {
    const view = await api.getSession(sessionId)
    if (selectedSessionId.value !== sessionId) return
    sessionView.value = view
    const sessions = sessionsByResearch[view.session.research_id] ?? []
    sessionsByResearch[view.session.research_id] = [
      view.session,
      ...sessions.filter((session) => session.id !== view.session.id),
    ].sort((left, right) => right.updated_at.localeCompare(left.updated_at))
    await refreshResearchIndex(view.session.research_id)
    if (workflowRunView.value) {
      const current = view.workflow_runs.find(
        (run) => run.id === workflowRunView.value?.workflow_run.id,
      )
      if (current) void inspectWorkflow(current.id, true)
    }
    syncPoll()
  } catch (error) {
    globalError.value = messageOf(error)
  }
}

function syncPoll() {
  clearPoll()
  if (!sessionView.value || !hasActiveWork(sessionView.value)) return
  const sessionId = sessionView.value.session.id
  pollTimer = window.setTimeout(async () => {
    pollTimer = null
    if (selectedSessionId.value === sessionId) await refreshSession(sessionId)
  }, 900)
}

function hasActiveWork(view: SessionView): boolean {
  return (
    view.turns.some((turn) => turn.status === 'queued' || turn.status === 'running') ||
    view.workflow_runs.some((run) => ['created', 'running', 'paused'].includes(run.status))
  )
}

function clearPoll() {
  if (pollTimer !== null) window.clearTimeout(pollTimer)
  pollTimer = null
}

function clearTimers() {
  if (refreshTimer !== null) window.clearTimeout(refreshTimer)
  refreshTimer = null
  clearPoll()
}

function showHome() {
  workflowLibraryOpen.value = false
  if (selectedResearchId.value) void selectResearch(selectedResearchId.value)
  else if (researches.value[0]) void selectResearch(researches.value[0].id)
}

function openWorkflowLibrary() {
  writeRoute('workflows')
  closeSessionStream()
  clearPoll()
  selectedSessionId.value = null
  sessionView.value = null
  sessionEvents.value = []
  workflowRunView.value = null
  workflowLibraryOpen.value = true
  mobileSidebarOpen.value = false
}

function workflowSaved(workflow: WorkflowRegistration) {
  workflows.value = [
    ...workflows.value.filter(
      (candidate) =>
        candidate.manifest.slug !== workflow.manifest.slug ||
        candidate.manifest.version !== workflow.manifest.version,
    ),
    workflow,
  ].sort((left, right) => left.manifest.name.localeCompare(right.manifest.name))
}

async function inspectWorkflow(workflowRunId: string, quiet = false) {
  if (!quiet) workflowLoading.value = true
  try {
    const view = await api.getWorkflowRun(workflowRunId)
    if (
      selectedSessionId.value &&
      (view.workflow_run.origin_session_id === selectedSessionId.value ||
        view.participants.some((participant) => participant.session_id === selectedSessionId.value))
    ) {
      workflowRunView.value = view
    }
  } catch (error) {
    if (!quiet) globalError.value = messageOf(error)
  } finally {
    if (!quiet) workflowLoading.value = false
  }
}

function openSessionDialog(researchId: string) {
  workflowLibraryOpen.value = false
  dialogResearchId.value = researchId
  dialogError.value = ''
  sessionDialogOpen.value = true
  void ensureResearchSkills(researchId).catch((error) => {
    dialogError.value = messageOf(error)
  })
}

function closeDialogs() {
  if (dialogBusy.value) return
  researchDialogOpen.value = false
  sessionDialogOpen.value = false
  skillDialogOpen.value = false
  workflowDialogOpen.value = false
  dialogError.value = ''
}

async function createResearch(input: { name: string; description: string }) {
  dialogBusy.value = true
  dialogError.value = ''
  try {
    const research = await api.createResearch(input.name, input.description)
    researches.value = [research, ...researches.value]
    sessionsByResearch[research.id] = []
    skillsByResearch[research.id] = []
    researchDialogOpen.value = false
    await selectResearch(research.id)
  } catch (error) {
    dialogError.value = messageOf(error)
  } finally {
    dialogBusy.value = false
  }
}

async function createSession(input: CreateSessionInput) {
  const research = dialogResearch.value
  if (!research) return
  dialogBusy.value = true
  dialogError.value = ''
  try {
    const session = await api.createSession(research.id, input)
    sessionsByResearch[research.id] = [session, ...(sessionsByResearch[research.id] ?? [])]
    sessionDialogOpen.value = false
    await selectSession(session.id)
  } catch (error) {
    dialogError.value = messageOf(error)
  } finally {
    dialogBusy.value = false
  }
}

async function createTurn(input: string) {
  const view = sessionView.value
  if (!view) return
  try {
    const turn = await api.createTurn(view.session.id, input)
    if (sessionView.value?.session.id === view.session.id) {
      sessionView.value = { ...sessionView.value, turns: [...sessionView.value.turns, turn] }
      syncPoll()
    }
  } catch (error) {
    globalError.value = messageOf(error)
  }
}

async function cancelTurn(turnId: string) {
  try {
    await api.cancelTurn(turnId)
    if (selectedSessionId.value) scheduleSessionRefresh(selectedSessionId.value)
  } catch (error) {
    globalError.value = messageOf(error)
  }
}

async function updateSessionSkills(slugs: string[]) {
  const view = sessionView.value
  if (!view || skillsBusy.value) return
  skillsBusy.value = true
  try {
    const session = await api.updateSessionSkills(view.session.id, slugs)
    if (sessionView.value?.session.id === session.id) sessionView.value.session = session
  } catch (error) {
    globalError.value = messageOf(error)
    if (sessionView.value) sessionView.value = { ...sessionView.value }
  } finally {
    skillsBusy.value = false
  }
}

async function updateSessionAccess(access: AgentAccessProfile) {
  const view = sessionView.value
  if (!view || accessBusy.value) return
  accessBusy.value = true
  try {
    const session = await api.updateSessionAccess(view.session.id, access)
    if (sessionView.value?.session.id === session.id) sessionView.value.session = session
    const sessions = sessionsByResearch[session.research_id] ?? []
    sessionsByResearch[session.research_id] = [
      session,
      ...sessions.filter((candidate) => candidate.id !== session.id),
    ].sort((left, right) => right.updated_at.localeCompare(left.updated_at))
  } catch (error) {
    globalError.value = messageOf(error)
    if (selectedSessionId.value === view.session.id) await refreshSession(view.session.id)
  } finally {
    accessBusy.value = false
  }
}

async function createSkill(input: { slug: string; name: string; description: string; instructions: string }) {
  const researchId = selectedResearchId.value
  if (!researchId) return
  dialogBusy.value = true
  dialogError.value = ''
  try {
    const skill = await api.createResearchSkill(researchId, input)
    skillsByResearch[researchId] = [...(skillsByResearch[researchId] ?? []), skill].sort((left, right) =>
      left.name.localeCompare(right.name),
    )
    skillDialogOpen.value = false
  } catch (error) {
    dialogError.value = messageOf(error)
  } finally {
    dialogBusy.value = false
  }
}

async function createWorkflowRun(input: {
  workflow: WorkflowRegistration
  objective: string
  input: Record<string, unknown>
}) {
  const view = sessionView.value
  if (!view) return
  dialogBusy.value = true
  dialogError.value = ''
  try {
    const workflowRun = await api.createWorkflowRun(view.session.id, {
      workflow_slug: input.workflow.manifest.slug,
      workflow_version: input.workflow.manifest.version,
      objective: input.objective,
      input: input.input,
    })
    if (sessionView.value?.session.id === view.session.id) {
      sessionView.value = {
        ...sessionView.value,
        workflow_runs: [workflowRun, ...sessionView.value.workflow_runs],
      }
    }
    workflowDialogOpen.value = false
    await inspectWorkflow(workflowRun.id)
    syncPoll()
  } catch (error) {
    dialogError.value = messageOf(error)
  } finally {
    dialogBusy.value = false
  }
}

async function cancelWorkflow(workflowRunId: string) {
  try {
    await api.cancelWorkflowRun(workflowRunId)
    if (selectedSessionId.value) scheduleSessionRefresh(selectedSessionId.value)
  } catch (error) {
    globalError.value = messageOf(error)
  }
}

async function pauseWorkflow(workflowRunId: string) {
  try {
    await api.pauseWorkflowRun(workflowRunId)
    if (selectedSessionId.value) scheduleSessionRefresh(selectedSessionId.value)
  } catch (error) {
    globalError.value = messageOf(error)
  }
}

async function resumeWorkflow(workflowRunId: string) {
  try {
    await api.resumeWorkflowRun(workflowRunId)
    if (selectedSessionId.value) scheduleSessionRefresh(selectedSessionId.value)
  } catch (error) {
    globalError.value = messageOf(error)
  }
}

async function sendWorkflowControl(input: {
  workflowRunId: string
  sessionId: string
  kind: 'guide' | 'interrupt' | 'finish'
  content: string
  actionInvocationId?: string
}) {
  try {
    await api.sendControl(
      input.workflowRunId,
      input.sessionId,
      input.kind,
      input.content,
      input.actionInvocationId,
    )
    await inspectWorkflow(input.workflowRunId, true)
  } catch (error) {
    globalError.value = messageOf(error)
  }
}

async function answerHumanRequest(input: { requestId: string; answer: unknown; workflowRunId: string }) {
  try {
    await api.answerHumanRequest(input.requestId, input.answer)
    await inspectWorkflow(input.workflowRunId, true)
    if (selectedSessionId.value) scheduleSessionRefresh(selectedSessionId.value)
  } catch (error) {
    globalError.value = messageOf(error)
  }
}

function onKeydown(event: KeyboardEvent) {
  if (event.key !== 'Escape') return
  if (selectedWorkflowOutput.value) selectedWorkflowOutput.value = null
  else if (selectedArtifact.value) selectedArtifact.value = null
  else if (!dialogBusy.value) closeDialogs()
  mobileSidebarOpen.value = false
}

function messageOf(error: unknown): string {
  return error instanceof Error ? error.message : String(error)
}

function writeRoute(kind: 'research' | 'session' | 'workflows', id?: string) {
  const hash = id ? `#${kind}/${encodeURIComponent(id)}` : `#${kind}`
  if (window.location.hash !== hash) window.history.replaceState(null, '', hash)
}

function readRoute(): { kind: 'research' | 'session'; id: string } | { kind: 'workflows' } | null {
  const [kind, encodedId] = window.location.hash.slice(1).split('/', 2)
  if (kind === 'workflows') return { kind }
  if ((kind === 'research' || kind === 'session') && encodedId) {
    return { kind, id: decodeURIComponent(encodedId) }
  }
  return null
}
</script>
