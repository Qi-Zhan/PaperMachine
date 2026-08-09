<template>
  <div
    class="app-layout"
    :data-sidebar-open="desktopSidebarOpen"
    :data-sidebar-resizing="sidebarResizing"
    :style="{ '--sidebar-width': `${sidebarWidth}px` }"
  >
    <div v-if="mobileSidebarOpen" class="mobile-sidebar-backdrop" @click="mobileSidebarOpen = false" />
    <div class="sidebar-region" :data-mobile-open="mobileSidebarOpen">
      <AppSidebar
        :projects="projects"
        :sessions-by-project="sessionsByProject"
        :selected-project-id="selectedProjectId"
        :selected-session-id="selectedSessionId"
        :mode="health?.model_mode ?? 'demo'"
        :model-label="defaultModelLabel"
        :online="online"
        :workflows-active="workflowLibraryOpen"
        @home="showHome"
        @close-sidebar="mobileSidebarOpen = false"
        @new-project="projectDialogOpen = true"
        @new-session="openSessionDialog"
        @relocate-project="openRelocateProjectDialog"
        @remove-project="removeProject"
        @open-workflows="openWorkflowLibrary"
        @select-project="selectProject"
        @select-session="selectSession"
      />
    </div>
    <div
      class="sidebar-resizer"
      :data-resizing="sidebarResizing"
      role="separator"
      aria-orientation="vertical"
      :aria-label="t('common.resizeSidebar')"
      :aria-valuemin="SIDEBAR_MIN_WIDTH"
      :aria-valuemax="SIDEBAR_MAX_WIDTH"
      :aria-valuenow="sidebarWidth"
      tabindex="0"
      @dblclick="resetSidebarWidth"
      @keydown="resizeSidebarWithKeyboard"
      @pointerdown="startSidebarResize"
    />

    <section class="main-region">
      <div v-if="initialLoading" class="full-loading">
        <ScanSearch :size="22" />
        <LoaderCircle class="spin" :size="18" />
      </div>

      <WorkflowLibrary
        v-else-if="workflowLibraryOpen && selectedProjectId"
        :project-id="selectedProjectId"
        :workflows="workflowPrograms"
        @toggle-sidebar="toggleSidebar"
        @saved="workflowSaved"
      />

      <div v-else-if="projects.length === 0" class="zero-state">
        <header class="zero-state-header">
          <button
            class="icon-button sidebar-toggle"
            type="button"
            :title="t('common.toggleSidebar')"
            :aria-label="t('common.toggleSidebar')"
            @click="toggleSidebar"
          >
            <PanelLeft :size="18" />
          </button>
          <span class="brand-mark zero-mobile-brand"><ScanSearch :size="16" /></span>
          <strong class="zero-mobile-brand">PaperMachine</strong>
          <strong class="zero-desktop-title">Project</strong>
        </header>
        <div class="zero-state-main">
          <FolderSearch2 :size="28" />
          <h1>{{ t('zero.createProject') }}</h1>
          <button class="primary-button" type="button" @click="projectDialogOpen = true">
            <FolderPlus :size="16" />
            {{ t('sidebar.newProject') }}
          </button>
        </div>
      </div>

      <SessionWorkspace
        v-else-if="sessionView && selectedProject"
        :project="selectedProject"
        :workspace-available="selectedProject.workspace_available"
        :view="sessionView"
        :events="sessionEvents"
        :live-outputs="liveAssistantOutputs"
        :skills="projectSkills"
        :workflow-view="workflowView"
        :workflow-loading="workflowLoading"
        :stream-connected="streamConnected"
        :skills-busy="skillsBusy"
        :access-busy="accessBusy"
        :prompt-busy="promptBusy"
        :hosted-web-search="sessionHostedWebSearch"
        @toggle-sidebar="toggleSidebar"
        @select-project="selectProject"
        @select-session="selectSession"
        @close-session="closeSession"
        @send="sendSessionMessage"
        @cancel-turn="cancelTurn"
        @open-workflow="openSessionWorkflowDialog"
        @inspect-workflow="inspectWorkflow"
        @pause-workflow="pauseWorkflow"
        @resume-workflow="resumeWorkflow"
        @cancel-workflow="cancelWorkflow"
        @send-control="sendWorkflowControl"
        @answer-human="answerHumanRequest"
        @update-skills="updateSessionSkills"
        @update-access="updateSessionAccess"
        @update-system-prompt="updateSessionSystemPrompt"
        @open-artifact="selectedArtifact = $event"
        @open-workflow-output="selectedWorkflowOutput = $event"
      />

      <ProjectOverview
        v-else-if="projectOverview"
        :overview="projectOverview"
        :workspace-available="selectedProject?.workspace_available ?? false"
        :summary-busy="projectSummaryBusy"
        @toggle-sidebar="toggleSidebar"
        @new-session="openSessionDialog(projectOverview.project.id)"
        @run-workflow="openProjectWorkflowDialog"
        @run-summary="runProjectSummary"
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

    <NewProjectDialog
      :open="projectDialogOpen"
      :busy="dialogBusy"
      :error="dialogError"
      @close="closeDialogs"
      @submit="createProject"
    />
    <ProjectPathDialog
      :open="projectPathDialogOpen"
      :busy="dialogBusy"
      :error="dialogError"
      :project-name="projectPathDialogProject?.name"
      :initial-path="projectPathDialogProject?.workspace.path"
      @close="closeProjectPathDialog"
      @submit="submitProjectPath"
    />
    <NewSessionDialog
      :open="sessionDialogOpen"
      :busy="dialogBusy"
      :error="dialogError"
      :project="dialogProject"
      :skills="dialogSkills"
      :model-profiles="health?.model_profiles ?? []"
      :default-model="health?.default_model ?? ''"
      @close="closeDialogs"
      @submit="createSession"
    />
    <StartWorkflowDialog
      :open="workflowDialogOpen"
      :busy="dialogBusy"
      :error="dialogError"
      :project="workflowDialogProject"
      :session="workflowDialogOriginSession"
      :workflows="workflowPrograms"
      :skills="workflowDialogSkills"
      :model-profiles="health?.model_profiles ?? []"
      :default-model="health?.default_model ?? ''"
      @close="closeDialogs"
      @submit="createWorkflow"
    />
    <ArtifactDialog :artifact="selectedArtifact" @close="selectedArtifact = null" />
    <WorkflowOutputDialog :workflow="selectedWorkflowOutput" @close="selectedWorkflowOutput = null" />
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
import {
  applyLiveAssistantUpdate,
  applySessionStreamUpdate,
  isDurableSessionUpdate,
  type LiveAssistantOutputs,
  type SessionStreamUpdate,
} from './sessionEvents'
import AppSidebar from './components/AppSidebar.vue'
import ArtifactDialog from './components/ArtifactDialog.vue'
import NewProjectDialog from './components/NewProjectDialog.vue'
import ProjectPathDialog from './components/ProjectPathDialog.vue'
import NewSessionDialog from './components/NewSessionDialog.vue'
import ProjectOverview from './components/ProjectOverview.vue'
import SessionWorkspace from './components/SessionWorkspace.vue'
import WorkflowLibrary from './components/WorkflowLibrary.vue'
import StartWorkflowDialog from './components/StartWorkflowDialog.vue'
import WorkflowOutputDialog from './components/WorkflowOutputDialog.vue'
import type {
  AccessPreset,
  Artifact,
  CreateSessionInput,
  Health,
  ProjectCatalogEntry,
  ProjectOverview as ProjectOverviewType,
  ProjectSkill,
  Session,
  SessionEvent,
  SessionView,
  WorkflowProgram,
  WorkflowContextMode,
  Workflow,
  WorkflowView,
} from './types'

const projects = ref<ProjectCatalogEntry[]>([])
const { t } = useAppI18n()
const workflowPrograms = ref<WorkflowProgram[]>([])
const sessionsByProject = reactive<Record<string, Session[]>>({})
const skillsByProject = reactive<Record<string, ProjectSkill[]>>({})
const selectedProjectId = ref<string | null>(null)
const selectedSessionId = ref<string | null>(null)
const projectOverview = ref<ProjectOverviewType | null>(null)
const sessionView = ref<SessionView | null>(null)
const sessionEvents = ref<SessionEvent[]>([])
const liveAssistantOutputs = ref<LiveAssistantOutputs>({})
const workflowView = ref<WorkflowView | null>(null)
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
const promptBusy = ref(false)
const summaryBusy = ref(false)
const projectDialogOpen = ref(false)
const projectPathDialogOpen = ref(false)
const projectPathDialogProjectId = ref<string | null>(null)
const sessionDialogOpen = ref(false)
const workflowDialogOpen = ref(false)
const workflowDialogProjectId = ref<string | null>(null)
const workflowDialogOriginSession = ref<Session | null>(null)
const dialogProjectId = ref<string | null>(null)
const mobileSidebarOpen = ref(false)
const SIDEBAR_DEFAULT_WIDTH = 280
const SIDEBAR_MIN_WIDTH = 220
const SIDEBAR_MAX_WIDTH = 480
const SIDEBAR_WIDTH_STORAGE_KEY = 'papermachine.sidebar.width'
const SIDEBAR_OPEN_STORAGE_KEY = 'papermachine.sidebar.open'
const sidebarWidth = ref(readSidebarWidth())
const desktopSidebarOpen = ref(readSidebarOpen())
const sidebarResizing = ref(false)
const selectedArtifact = ref<Artifact | null>(null)
const selectedWorkflowOutput = ref<Workflow | null>(null)
const workflowLibraryOpen = ref(false)

let sessionEventSource: EventSource | null = null
let projectEventSource: EventSource | null = null
let workflowRefreshTimer: number | null = null
let projectRefreshTimer: number | null = null

const selectedProject = computed(
  () => projects.value.find((project) => project.id === selectedProjectId.value) ?? null,
)
const projectSkills = computed(() =>
  selectedProjectId.value ? (skillsByProject[selectedProjectId.value] ?? []) : [],
)
const dialogProject = computed(
  () => projects.value.find((project) => project.id === dialogProjectId.value) ?? null,
)
const dialogSkills = computed(() =>
  dialogProjectId.value ? (skillsByProject[dialogProjectId.value] ?? []) : [],
)
const workflowDialogProject = computed(
  () => projects.value.find((project) => project.id === workflowDialogProjectId.value) ?? null,
)
const workflowDialogSkills = computed(() =>
  workflowDialogProjectId.value ? (skillsByProject[workflowDialogProjectId.value] ?? []) : [],
)
const projectPathDialogProject = computed(() =>
  projects.value.find((project) => project.id === projectPathDialogProjectId.value) ?? null,
)
const defaultModelLabel = computed(() => {
  if (!health.value || health.value.model_mode === 'demo') return ''
  const profile = health.value.model_profiles.find(
    (candidate) => candidate.id === health.value?.default_model,
  )
  return profile ? `${profile.provider} · ${profile.model}` : health.value.default_model
})
const sessionHostedWebSearch = computed(() => {
  const profile = health.value?.model_profiles.find(
    (candidate) => candidate.id === sessionView.value?.session.model,
  )
  return profile?.capabilities.includes('hosted_web_search') ?? false
})
const projectSummaryBusy = computed(
  () =>
    summaryBusy.value ||
    ['created', 'running'].includes(projectOverview.value?.summary_workflow?.status ?? ''),
)

onMounted(async () => {
  window.addEventListener('keydown', onKeydown)
  try {
    const [healthResult, projectResult] = await Promise.all([
      api.health(),
      api.listProjects(),
    ])
    health.value = healthResult
    online.value = true
    projects.value = projectResult
    const initialRoute = readRoute()
    let restored = false
    if (initialRoute?.kind === 'session') restored = await selectSession(initialRoute.id)
    else if (initialRoute?.kind === 'project') restored = await selectProject(initialRoute.id)
    else if (initialRoute?.kind === 'workflows') restored = await openWorkflowLibrary(initialRoute.id)
    const firstProject = projectResult[0]
    if (!restored && firstProject) await selectProject(firstProject.id)
  } catch (error) {
    online.value = false
    globalError.value = messageOf(error)
  } finally {
    initialLoading.value = false
  }
})

onBeforeUnmount(() => {
  window.removeEventListener('keydown', onKeydown)
  stopSidebarResize()
  closeSessionStream()
  closeProjectStream()
  clearTimers()
})

function readSidebarWidth(): number {
  try {
    const stored = Number(window.localStorage.getItem(SIDEBAR_WIDTH_STORAGE_KEY))
    if (Number.isFinite(stored) && stored > 0) return clampSidebarWidth(stored)
  } catch {
    // Storage can be unavailable in restricted browser contexts.
  }
  return SIDEBAR_DEFAULT_WIDTH
}

function readSidebarOpen(): boolean {
  try {
    return window.localStorage.getItem(SIDEBAR_OPEN_STORAGE_KEY) !== 'false'
  } catch {
    return true
  }
}

function clampSidebarWidth(width: number): number {
  return Math.min(SIDEBAR_MAX_WIDTH, Math.max(SIDEBAR_MIN_WIDTH, Math.round(width)))
}

function persistSidebarWidth() {
  try {
    window.localStorage.setItem(SIDEBAR_WIDTH_STORAGE_KEY, String(sidebarWidth.value))
  } catch {
    // Resizing still works when storage is unavailable.
  }
}

function toggleSidebar() {
  if (window.matchMedia('(max-width: 900px)').matches) {
    mobileSidebarOpen.value = !mobileSidebarOpen.value
    return
  }
  desktopSidebarOpen.value = !desktopSidebarOpen.value
  try {
    window.localStorage.setItem(SIDEBAR_OPEN_STORAGE_KEY, String(desktopSidebarOpen.value))
  } catch {
    // Toggling still works when storage is unavailable.
  }
}

function startSidebarResize(event: PointerEvent) {
  if (event.button !== 0 || !desktopSidebarOpen.value) return
  event.preventDefault()
  sidebarResizing.value = true
  document.body.classList.add('sidebar-resizing')
  window.addEventListener('pointermove', resizeSidebar)
  window.addEventListener('pointerup', stopSidebarResize)
  window.addEventListener('pointercancel', stopSidebarResize)
}

function resizeSidebar(event: PointerEvent) {
  sidebarWidth.value = clampSidebarWidth(event.clientX)
}

function stopSidebarResize() {
  if (!sidebarResizing.value) return
  sidebarResizing.value = false
  document.body.classList.remove('sidebar-resizing')
  window.removeEventListener('pointermove', resizeSidebar)
  window.removeEventListener('pointerup', stopSidebarResize)
  window.removeEventListener('pointercancel', stopSidebarResize)
  persistSidebarWidth()
}

function resizeSidebarWithKeyboard(event: KeyboardEvent) {
  const step = event.shiftKey ? 40 : 16
  let nextWidth = sidebarWidth.value
  if (event.key === 'ArrowLeft') nextWidth -= step
  else if (event.key === 'ArrowRight') nextWidth += step
  else if (event.key === 'Home') nextWidth = SIDEBAR_MIN_WIDTH
  else if (event.key === 'End') nextWidth = SIDEBAR_MAX_WIDTH
  else return
  event.preventDefault()
  sidebarWidth.value = clampSidebarWidth(nextWidth)
  persistSidebarWidth()
}

function resetSidebarWidth() {
  sidebarWidth.value = SIDEBAR_DEFAULT_WIDTH
  persistSidebarWidth()
}

async function refreshProjectIndex(projectId: string) {
  const overview = await api.getProject(projectId)
  const index = projects.value.findIndex((project) => project.id === projectId)
  if (index >= 0) {
    projects.value[index] = {
      ...overview.project,
      workspace_available: projects.value[index].workspace_available,
    }
  }
  if (selectedProjectId.value === projectId && !selectedSessionId.value) projectOverview.value = overview
  return overview
}

async function refreshProjectSessions(projectId: string) {
  const sessions = await api.listSessions(projectId)
  sessionsByProject[projectId] = sessions
  return sessions
}

async function ensureProjectSkills(projectId: string, refresh = false) {
  if (!refresh && skillsByProject[projectId]) return skillsByProject[projectId]
  const skills = await api.listProjectSkills(projectId)
  skillsByProject[projectId] = skills
  return skills
}

async function selectProject(projectId: string): Promise<boolean> {
  const catalogEntry = projects.value.find((project) => project.id === projectId)
  if (!catalogEntry) return false
  closeSessionStream()
  closeProjectStream()
  selectedProjectId.value = projectId
  selectedSessionId.value = null
  sessionView.value = null
  sessionEvents.value = []
  liveAssistantOutputs.value = {}
  workflowView.value = null
  workflowPrograms.value = []
  workflowLibraryOpen.value = false
  mobileSidebarOpen.value = false
  try {
    const [overview] = await Promise.all([
      refreshProjectIndex(projectId),
      refreshProjectSessions(projectId),
    ])
    if (selectedProjectId.value === projectId && !selectedSessionId.value) {
      projectOverview.value = overview
      writeRoute('project', projectId)
      connectProjectStream(projectId)
    }
    return true
  } catch (error) {
    if (selectedProjectId.value === projectId) selectedProjectId.value = null
    globalError.value = messageOf(error)
    return false
  }
}

async function selectSession(sessionId: string): Promise<boolean> {
  selectedSessionId.value = sessionId
  sessionView.value = null
  sessionEvents.value = []
  liveAssistantOutputs.value = {}
  workflowView.value = null
  workflowLibraryOpen.value = false
  mobileSidebarOpen.value = false
  closeSessionStream()
  closeProjectStream()
  try {
    const [view, events] = await Promise.all([api.getSession(sessionId), api.listSessionEvents(sessionId)])
    if (selectedSessionId.value !== sessionId) return false
    selectedProjectId.value = view.session.project_id
    sessionView.value = view
    sessionEvents.value = events
    await Promise.all([
      ensureProjectSkills(view.session.project_id),
      refreshProjectSessions(view.session.project_id),
      loadWorkflowPrograms(view.session.project_id),
    ])
    connectSessionStream(sessionId, events.at(-1)?.sequence ?? 0)
    const latestWorkflow = view.workflows[0]
    if (latestWorkflow) void inspectWorkflow(latestWorkflow.id)
    writeRoute('session', sessionId)
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
      const update = JSON.parse(message.data) as SessionStreamUpdate
      if (update.type === 'session_resync') {
        void reconcileSession(sessionId)
        return
      }
      liveAssistantOutputs.value = applyLiveAssistantUpdate(liveAssistantOutputs.value, update)
      if (sessionView.value?.session.id === sessionId) {
        sessionView.value = applySessionStreamUpdate(sessionView.value, update)
      }
      if (
        isDurableSessionUpdate(update) &&
        !sessionEvents.value.some((candidate) => candidate.id === update.id)
      ) {
        sessionEvents.value.push(update)
        sessionEvents.value.sort((left, right) => left.sequence - right.sequence)
      }
      if ('session' in update && update.session) updateSidebarSession(update.session)
      const updatedWorkflow = 'workflow' in update ? update.workflow : undefined
      if (updatedWorkflow && workflowView.value?.workflow.id === updatedWorkflow.id) {
        scheduleWorkflowInspection(updatedWorkflow.id)
      }
    } catch {
      globalError.value = t('app.invalidSessionEvent')
    }
  }
  for (const type of sessionEventTypes) source.addEventListener(type, receive as EventListener)
}

async function reconcileSession(sessionId: string) {
  try {
    const after = sessionEvents.value.at(-1)?.sequence ?? 0
    const [view, events] = await Promise.all([
      api.getSession(sessionId),
      api.listSessionEvents(sessionId, after),
    ])
    if (selectedSessionId.value !== sessionId) return
    sessionView.value = view
    const known = new Set(sessionEvents.value.map((event) => event.id))
    sessionEvents.value = [...sessionEvents.value, ...events.filter((event) => !known.has(event.id))]
      .sort((left, right) => left.sequence - right.sequence)
    updateSidebarSession(view.session)
  } catch (error) {
    globalError.value = messageOf(error)
  }
}

function closeSessionStream() {
  sessionEventSource?.close()
  sessionEventSource = null
  streamConnected.value = false
}

type ProjectStreamUpdate =
  | { type: 'session_changed'; session: Session }
  | { type: 'workflow_changed'; workflow: Workflow }
  | { type: 'project_resync' }

function connectProjectStream(projectId: string) {
  closeProjectStream()
  const source = new EventSource(`/api/projects/${projectId}/events/stream`)
  projectEventSource = source
  source.onopen = () => {
    if (selectedProjectId.value === projectId && !selectedSessionId.value) {
      void refreshProjectIndex(projectId)
    }
  }
  source.onerror = () => {
    // EventSource reconnects automatically; onopen reconciles missed state.
  }
  const receive = (message: MessageEvent<string>) => {
    try {
      const update = JSON.parse(message.data) as ProjectStreamUpdate
      if (update.type === 'project_resync') {
        scheduleProjectRefresh(projectId)
      } else if (update.type === 'session_changed') {
        updateSidebarSession(update.session)
      } else if (update.workflow.program.manifest.slug === 'project-summary') {
        if (projectOverview.value?.project.id === projectId) {
          projectOverview.value = {
            ...projectOverview.value,
            summary_workflow: update.workflow,
          }
        }
        scheduleProjectRefresh(projectId)
      }
    } catch {
      globalError.value = t('app.invalidSessionEvent')
    }
  }
  source.addEventListener('session_changed', receive as EventListener)
  source.addEventListener('workflow_changed', receive as EventListener)
  source.addEventListener('project_resync', receive as EventListener)
}

function closeProjectStream() {
  projectEventSource?.close()
  projectEventSource = null
  if (projectRefreshTimer !== null) window.clearTimeout(projectRefreshTimer)
  projectRefreshTimer = null
}

function scheduleProjectRefresh(projectId: string) {
  if (projectRefreshTimer !== null) window.clearTimeout(projectRefreshTimer)
  projectRefreshTimer = window.setTimeout(() => {
    projectRefreshTimer = null
    if (selectedProjectId.value === projectId && !selectedSessionId.value) {
      void refreshProjectIndex(projectId)
    }
  }, 120)
}

function updateSidebarSession(session: Session) {
  const sessions = sessionsByProject[session.project_id] ?? []
  sessionsByProject[session.project_id] = [
    session,
    ...sessions.filter((candidate) => candidate.id !== session.id),
  ].sort((left, right) => right.updated_at.localeCompare(left.updated_at))
}

function scheduleWorkflowInspection(workflowId: string) {
  if (workflowRefreshTimer !== null) window.clearTimeout(workflowRefreshTimer)
  workflowRefreshTimer = window.setTimeout(() => {
    workflowRefreshTimer = null
    if (workflowView.value?.workflow.id === workflowId) void inspectWorkflow(workflowId, true)
  }, 120)
}

function clearTimers() {
  if (workflowRefreshTimer !== null) window.clearTimeout(workflowRefreshTimer)
  workflowRefreshTimer = null
  if (projectRefreshTimer !== null) window.clearTimeout(projectRefreshTimer)
  projectRefreshTimer = null
}

function showHome() {
  workflowLibraryOpen.value = false
  if (selectedProjectId.value) void selectProject(selectedProjectId.value)
  else {
    const firstProject = projects.value[0]
    if (firstProject) void selectProject(firstProject.id)
  }
}

async function openWorkflowLibrary(projectId = selectedProjectId.value ?? projects.value[0]?.id): Promise<boolean> {
  if (!projectId) return false
  selectedProjectId.value = projectId
  try {
    await loadWorkflowPrograms(projectId, true)
  } catch (error) {
    globalError.value = messageOf(error)
    return false
  }
  writeRoute('workflows', projectId)
  closeSessionStream()
  closeProjectStream()
  selectedSessionId.value = null
  sessionView.value = null
  sessionEvents.value = []
  workflowView.value = null
  workflowLibraryOpen.value = true
  mobileSidebarOpen.value = false
  return true
}

function workflowSaved(workflow: WorkflowProgram) {
  workflowPrograms.value = [
    ...workflowPrograms.value.filter((candidate) => candidate.manifest.slug !== workflow.manifest.slug),
    workflow,
  ].sort((left, right) => left.manifest.name.localeCompare(right.manifest.name))
}

async function loadWorkflowPrograms(projectId: string, refresh = false) {
  if (!refresh && selectedProjectId.value === projectId && workflowPrograms.value.length) {
    return workflowPrograms.value
  }
  const programs = await api.listWorkflowPrograms(projectId)
  if (selectedProjectId.value === projectId) workflowPrograms.value = programs
  return programs
}

async function inspectWorkflow(workflowId: string, quiet = false) {
  if (!quiet) workflowLoading.value = true
  try {
    const view = await api.getWorkflow(workflowId)
    if (
      selectedSessionId.value &&
      (view.workflow.started_from_session_id === selectedSessionId.value ||
        view.participants.some((participant) => participant.session_id === selectedSessionId.value))
    ) {
      workflowView.value = view
    }
  } catch (error) {
    if (!quiet) globalError.value = messageOf(error)
  } finally {
    if (!quiet) workflowLoading.value = false
  }
}

function openSessionDialog(projectId: string) {
  workflowLibraryOpen.value = false
  dialogProjectId.value = projectId
  dialogError.value = ''
  sessionDialogOpen.value = true
  void ensureProjectSkills(projectId).catch((error) => {
    dialogError.value = messageOf(error)
  })
}

function openSessionWorkflowDialog() {
  const session = sessionView.value?.session
  if (session) void openWorkflowDialog(session.project_id, session)
}

function openProjectWorkflowDialog() {
  const projectId = projectOverview.value?.project.id ?? selectedProjectId.value
  if (projectId) void openWorkflowDialog(projectId, null)
}

async function openWorkflowDialog(projectId: string, origin: Session | null) {
  workflowDialogProjectId.value = projectId
  workflowDialogOriginSession.value = origin
  dialogError.value = ''
  workflowDialogOpen.value = true
  try {
    await Promise.all([
      ensureProjectSkills(projectId),
      loadWorkflowPrograms(projectId),
    ])
  } catch (error) {
    dialogError.value = messageOf(error)
  }
}

function closeDialogs() {
  if (dialogBusy.value) return
  projectDialogOpen.value = false
  sessionDialogOpen.value = false
  workflowDialogOpen.value = false
  workflowDialogProjectId.value = null
  workflowDialogOriginSession.value = null
  dialogError.value = ''
}

async function createProject(input: { name: string; workspacePath: string }) {
  dialogBusy.value = true
  dialogError.value = ''
  try {
    const project = await api.createProject(input.name, input.workspacePath)
    projects.value = [project, ...projects.value]
    sessionsByProject[project.id] = []
    skillsByProject[project.id] = []
    projectDialogOpen.value = false
    await selectProject(project.id)
  } catch (error) {
    dialogError.value = messageOf(error)
  } finally {
    dialogBusy.value = false
  }
}

function openRelocateProjectDialog(projectId: string) {
  projectPathDialogProjectId.value = projectId
  projectPathDialogOpen.value = true
  dialogError.value = ''
}

function closeProjectPathDialog() {
  if (dialogBusy.value) return
  projectPathDialogOpen.value = false
  projectPathDialogProjectId.value = null
  dialogError.value = ''
}

async function submitProjectPath(workspacePath: string) {
  dialogBusy.value = true
  dialogError.value = ''
  try {
    const entry = await api.relocateProject(
      projectPathDialogProjectId.value ?? '',
      workspacePath,
    )
    const existing = projects.value.findIndex((project) => project.id === entry.id)
    if (existing >= 0) projects.value[existing] = entry
    else projects.value = [entry, ...projects.value]
    projectPathDialogOpen.value = false
    projectPathDialogProjectId.value = null
    await selectProject(entry.id)
  } catch (error) {
    dialogError.value = messageOf(error)
  } finally {
    dialogBusy.value = false
  }
}

async function removeProject(projectId: string) {
  const project = projects.value.find((item) => item.id === projectId)
  if (!project || !window.confirm(t('project.removeConfirm', { name: project.name }))) return
  try {
    await api.removeProject(projectId)
    projects.value = projects.value.filter((item) => item.id !== projectId)
    delete sessionsByProject[projectId]
    delete skillsByProject[projectId]
    if (selectedProjectId.value === projectId) {
      selectedProjectId.value = null
      selectedSessionId.value = null
      projectOverview.value = null
      sessionView.value = null
      window.history.replaceState(null, '', window.location.pathname)
      const next = projects.value[0]
      if (next) await selectProject(next.id)
    }
  } catch (error) {
    globalError.value = messageOf(error)
  }
}

async function createSession(input: CreateSessionInput) {
  const project = dialogProject.value
  if (!project) return
  dialogBusy.value = true
  dialogError.value = ''
  try {
    const title = input.title.trim() || t('dialog.newSessionPlaceholder')
    const workflow = await api.createWorkflow(project.id, {
      program_slug: 'interactive-agent',
      instructions: '',
      params: {
        session_title: title,
        agent_system_prompt: input.system_prompt.trim(),
        agent_access: input.access,
      },
      model: input.model,
      access: input.access,
      enabled_skills: input.enabled_skills,
    })
    const session = await waitForWorkflowSession(workflow.id)
    sessionsByProject[project.id] = [session, ...(sessionsByProject[project.id] ?? [])]
    sessionDialogOpen.value = false
    await selectSession(session.id)
  } catch (error) {
    dialogError.value = messageOf(error)
  } finally {
    dialogBusy.value = false
  }
}

async function waitForWorkflowSession(workflowId: string): Promise<Session> {
  for (let attempt = 0; attempt < 100; attempt += 1) {
    const view = await api.getWorkflow(workflowId)
    const participant = view.participants[0]
    const session = participant
      ? view.sessions.find((candidate) => candidate.id === participant.session_id)
      : undefined
    if (session) return session
    if (['failed', 'cancelled'].includes(view.workflow.status)) {
      throw new Error(t('app.sessionWorkflowFailed', { error: view.workflow.error ?? view.workflow.status }))
    }
    await new Promise((resolve) => window.setTimeout(resolve, 50))
  }
  throw new Error(t('app.sessionWorkflowTimeout'))
}

async function sendSessionMessage(input: string) {
  const view = sessionView.value
  if (!view) return
  try {
    const humanRequest = view.human_requests.find(
      (request) =>
        request.status === 'open' &&
        request.session_id === view.session.id &&
        (!request.response_schema.type || request.response_schema.type === 'string'),
    )
    if (!humanRequest) return
    const answered = await api.answerHumanRequest(humanRequest.id, input)
    if (sessionView.value?.session.id === view.session.id) {
      sessionView.value = {
        ...sessionView.value,
        human_requests: sessionView.value.human_requests.map((request) =>
          request.id === answered.id ? answered : request,
        ),
      }
    }
    await inspectWorkflow(humanRequest.workflow_id, true)
  } catch (error) {
    globalError.value = messageOf(error)
  }
}

async function closeSession() {
  const view = sessionView.value
  if (!view || !window.confirm(t('session.closeConfirm', { title: view.session.title }))) return
  try {
    await api.closeSession(view.session.id)
    const projectId = view.session.project_id
    sessionsByProject[projectId] = (sessionsByProject[projectId] ?? []).filter(
      (session) => session.id !== view.session.id,
    )
    await selectProject(projectId)
  } catch (error) {
    globalError.value = messageOf(error)
  }
}

async function cancelTurn(turnId: string) {
  try {
    await api.cancelTurn(turnId)
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

async function updateSessionAccess(access: AccessPreset) {
  const view = sessionView.value
  if (!view || accessBusy.value) return
  accessBusy.value = true
  try {
    const session = await api.updateSessionAccess(view.session.id, access)
    if (sessionView.value?.session.id === session.id) sessionView.value.session = session
    const sessions = sessionsByProject[session.project_id] ?? []
    sessionsByProject[session.project_id] = [
      session,
      ...sessions.filter((candidate) => candidate.id !== session.id),
    ].sort((left, right) => right.updated_at.localeCompare(left.updated_at))
  } catch (error) {
    globalError.value = messageOf(error)
  } finally {
    accessBusy.value = false
  }
}

async function updateSessionSystemPrompt(systemPrompt: string) {
  const view = sessionView.value
  if (!view || promptBusy.value) return
  promptBusy.value = true
  try {
    const session = await api.updateSessionSystemPrompt(view.session.id, systemPrompt)
    if (sessionView.value?.session.id === session.id) sessionView.value.session = session
  } catch (error) {
    globalError.value = messageOf(error)
  } finally {
    promptBusy.value = false
  }
}

async function runProjectSummary(input: {
  instructions: string
  intervalMinutes: number
  replaceWorkflowId?: string
}) {
  const overview = projectOverview.value
  const model = health.value?.default_model
  if (!overview || !model || summaryBusy.value) return
  summaryBusy.value = true
  globalError.value = ''
  try {
    const workflow = await api.createWorkflow(overview.project.id, {
      program_slug: 'project-summary',
      request:
        input.intervalMinutes > 0
          ? `Refresh the Project home page every ${input.intervalMinutes} minutes.`
          : 'Refresh the Project home page now.',
      instructions: input.instructions.trim(),
      params: {
        interval_minutes: input.intervalMinutes,
        max_sessions: 50,
        turns_per_session: 12,
        max_artifacts: 50,
      },
      model,
      access: 'model_only',
      enabled_skills: [],
    })
    if (input.replaceWorkflowId && input.replaceWorkflowId !== workflow.id) {
      await api.cancelWorkflow(input.replaceWorkflowId)
    }
    await waitForProjectSummary(overview.project.id, workflow.id)
  } catch (error) {
    globalError.value = messageOf(error)
  } finally {
    summaryBusy.value = false
  }
}

async function waitForProjectSummary(projectId: string, workflowId: string) {
  for (let attempt = 0; attempt < 240; attempt += 1) {
    const workflow = await api.getWorkflowState(workflowId)
    if (workflow.status === 'failed' || workflow.status === 'cancelled') {
      throw new Error(workflow.error ?? workflow.status)
    }
    if (['completed', 'waiting_for_timer'].includes(workflow.status)) {
      const overview = await refreshProjectIndex(projectId)
      if (overview.project_home) return
    }
    await new Promise((resolve) => window.setTimeout(resolve, 500))
  }
  throw new Error(t('project.summaryTimeout'))
}

async function createWorkflow(input: {
  workflow: WorkflowProgram
  request: string
  instructions: string
  params: Record<string, unknown>
  contextMode: WorkflowContextMode
  model: string
  access: AccessPreset
  enabledSkills: string[]
  agentAccessOverrides: Record<string, AccessPreset>
}) {
  const project = workflowDialogProject.value
  const origin = workflowDialogOriginSession.value
  if (!project) return
  dialogBusy.value = true
  dialogError.value = ''
  try {
    const workflow = await api.createWorkflow(project.id, {
      program_slug: input.workflow.manifest.slug,
      ...(input.workflow.manifest.request_mode === 'required' ? { request: input.request } : {}),
      instructions: input.instructions,
      params: input.params,
      ...(origin ? { started_from_session_id: origin.id } : {}),
      model: input.model,
      access: input.access,
      enabled_skills: input.enabledSkills,
      context_mode: input.contextMode,
      agent_access_overrides: input.agentAccessOverrides,
    })
    workflowDialogOpen.value = false
    workflowDialogProjectId.value = null
    workflowDialogOriginSession.value = null
    if (origin && sessionView.value?.session.id === origin.id) {
      sessionView.value = {
        ...sessionView.value,
        workflows: [workflow, ...sessionView.value.workflows.filter((candidate) => candidate.id !== workflow.id)],
      }
      await inspectWorkflow(workflow.id)
    } else {
      const session = await waitForWorkflowSessionOrTerminal(workflow.id)
      await refreshProjectIndex(project.id)
      if (session) await selectSession(session.id)
      else if (selectedProjectId.value === project.id) await selectProject(project.id)
    }
  } catch (error) {
    dialogError.value = messageOf(error)
    if (!workflowDialogOpen.value) globalError.value = dialogError.value
  } finally {
    dialogBusy.value = false
  }
}

async function waitForWorkflowSessionOrTerminal(workflowId: string): Promise<Session | null> {
  for (let attempt = 0; attempt < 100; attempt += 1) {
    const view = await api.getWorkflow(workflowId)
    const participant = view.participants[0]
    const session = participant
      ? view.sessions.find((candidate) => candidate.id === participant.session_id)
      : undefined
    if (session) return session
    if (view.workflow.status === 'failed') throw new Error(view.workflow.error ?? view.workflow.status)
    if (['completed', 'cancelled'].includes(view.workflow.status) || view.workflow.attention_required) return null
    await new Promise((resolve) => window.setTimeout(resolve, 50))
  }
  return null
}

async function cancelWorkflow(workflowId: string) {
  try {
    await api.cancelWorkflow(workflowId)
  } catch (error) {
    globalError.value = messageOf(error)
  }
}

async function pauseWorkflow(workflowId: string) {
  try {
    await api.pauseWorkflow(workflowId)
  } catch (error) {
    globalError.value = messageOf(error)
  }
}

async function resumeWorkflow(workflowId: string) {
  try {
    await api.resumeWorkflow(workflowId)
  } catch (error) {
    globalError.value = messageOf(error)
  }
}

async function sendWorkflowControl(input: {
  workflowId: string
  sessionId: string
  kind: 'guide' | 'interrupt' | 'finish'
  content: string
  actionInvocationId?: string
}) {
  try {
    await api.sendControl(
      input.workflowId,
      input.sessionId,
      input.kind,
      input.content,
      input.actionInvocationId,
    )
    await inspectWorkflow(input.workflowId, true)
  } catch (error) {
    globalError.value = messageOf(error)
  }
}

async function answerHumanRequest(input: { requestId: string; answer: unknown; workflowId: string }) {
  try {
    await api.answerHumanRequest(input.requestId, input.answer)
    await inspectWorkflow(input.workflowId, true)
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

function writeRoute(kind: 'project' | 'session' | 'workflows', id?: string) {
  const hash = id ? `#${kind}/${encodeURIComponent(id)}` : `#${kind}`
  if (window.location.hash !== hash) window.history.replaceState(null, '', hash)
}

function readRoute(): { kind: 'project' | 'session' | 'workflows'; id: string } | null {
  const [kind, encodedId] = window.location.hash.slice(1).split('/', 2)
  if ((kind === 'project' || kind === 'session' || kind === 'workflows') && encodedId) {
    return { kind, id: decodeURIComponent(encodedId) }
  }
  return null
}
</script>
