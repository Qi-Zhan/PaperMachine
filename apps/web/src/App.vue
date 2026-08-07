<template>
  <div class="app-layout">
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

    <section class="main-region">
      <div v-if="initialLoading" class="full-loading">
        <ScanSearch :size="22" />
        <LoaderCircle class="spin" :size="18" />
      </div>

      <WorkflowLibrary
        v-else-if="workflowLibraryOpen && selectedProjectId"
        :project-id="selectedProjectId"
        :workflows="workflowPrograms"
        @open-sidebar="mobileSidebarOpen = true"
        @saved="workflowSaved"
      />

      <div v-else-if="projects.length === 0 || projects.every((project) => !project.available)" class="zero-state">
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
        :view="sessionView"
        :events="sessionEvents"
        :skills="projectSkills"
        :workflow-view="workflowView"
        :workflow-loading="workflowLoading"
        :stream-connected="streamConnected"
        :skills-busy="skillsBusy"
        :access-busy="accessBusy"
        :prompt-busy="promptBusy"
        :hosted-web-search="sessionHostedWebSearch"
        @open-sidebar="mobileSidebarOpen = true"
        @select-project="selectProject"
        @select-session="selectSession"
        @close-session="closeSession"
        @send="createTurn"
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
        :skills="projectSkills"
        :prompt-busy="promptBusy"
        :summary-busy="summaryBusy"
        @open-sidebar="mobileSidebarOpen = true"
        @new-session="openSessionDialog(projectOverview.project.id)"
        @new-skill="skillDialogOpen = true"
        @select-session="selectSession"
        @open-artifact="selectedArtifact = $event"
        @update-system-prompt="updateProjectSystemPrompt"
        @run-workflow="openProjectWorkflowDialog"
        @run-summary="runProjectSummary"
        @stop-summary="stopProjectSummary"
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
      :initial-path="projectPathDialogProject?.workspace_path"
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
    <NewSkillDialog
      :open="skillDialogOpen"
      :busy="dialogBusy"
      :error="dialogError"
      :project-name="selectedProject?.name ?? ''"
      @close="closeDialogs"
      @submit="createSkill"
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
import AppSidebar from './components/AppSidebar.vue'
import ArtifactDialog from './components/ArtifactDialog.vue'
import NewProjectDialog from './components/NewProjectDialog.vue'
import ProjectPathDialog from './components/ProjectPathDialog.vue'
import NewSessionDialog from './components/NewSessionDialog.vue'
import NewSkillDialog from './components/NewSkillDialog.vue'
import ProjectOverview from './components/ProjectOverview.vue'
import SessionWorkspace from './components/SessionWorkspace.vue'
import WorkflowLibrary from './components/WorkflowLibrary.vue'
import StartWorkflowDialog from './components/StartWorkflowDialog.vue'
import WorkflowOutputDialog from './components/WorkflowOutputDialog.vue'
import type {
  AgentAccessProfile,
  Artifact,
  CreateSessionInput,
  Health,
  ProjectLibraryEntry,
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

const projects = ref<ProjectLibraryEntry[]>([])
const { t } = useAppI18n()
const workflowPrograms = ref<WorkflowProgram[]>([])
const sessionsByProject = reactive<Record<string, Session[]>>({})
const skillsByProject = reactive<Record<string, ProjectSkill[]>>({})
const selectedProjectId = ref<string | null>(null)
const selectedSessionId = ref<string | null>(null)
const projectOverview = ref<ProjectOverviewType | null>(null)
const sessionView = ref<SessionView | null>(null)
const sessionEvents = ref<SessionEvent[]>([])
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
const skillDialogOpen = ref(false)
const workflowDialogOpen = ref(false)
const workflowDialogProjectId = ref<string | null>(null)
const workflowDialogOriginSession = ref<Session | null>(null)
const dialogProjectId = ref<string | null>(null)
const mobileSidebarOpen = ref(false)
const selectedArtifact = ref<Artifact | null>(null)
const selectedWorkflowOutput = ref<Workflow | null>(null)
const workflowLibraryOpen = ref(false)

let sessionEventSource: EventSource | null = null
let refreshTimer: number | null = null
let pollTimer: number | null = null

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
  if (!profile) return false
  return health.value?.model_providers.find(
    (provider) => provider.id === profile.provider,
  )?.hosted_web_search ?? false
})

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
    const firstAvailable = projectResult.find((project) => project.available)
    if (!restored && firstAvailable) await selectProject(firstAvailable.id)
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

async function refreshProjectIndex(projectId: string) {
  const overview = await api.getProject(projectId)
  sessionsByProject[projectId] = overview.sessions
  const index = projects.value.findIndex((project) => project.id === projectId)
  if (index >= 0) {
    projects.value[index] = {
      ...overview.project,
      available: true,
      workspace_available: projects.value[index].workspace_available,
    }
  }
  if (selectedProjectId.value === projectId && !selectedSessionId.value) projectOverview.value = overview
  return overview
}

async function ensureProjectSkills(projectId: string, refresh = false) {
  if (!refresh && skillsByProject[projectId]) return skillsByProject[projectId]
  const skills = await api.listProjectSkills(projectId)
  skillsByProject[projectId] = skills
  return skills
}

async function selectProject(projectId: string): Promise<boolean> {
  const libraryEntry = projects.value.find((project) => project.id === projectId)
  if (!libraryEntry?.available) {
    openRelocateProjectDialog(projectId)
    return false
  }
  closeSessionStream()
  clearPoll()
  selectedProjectId.value = projectId
  selectedSessionId.value = null
  sessionView.value = null
  sessionEvents.value = []
  workflowView.value = null
  workflowLibraryOpen.value = false
  mobileSidebarOpen.value = false
  try {
    const [overview] = await Promise.all([
      refreshProjectIndex(projectId),
      ensureProjectSkills(projectId),
      loadWorkflowPrograms(projectId),
    ])
    if (selectedProjectId.value === projectId && !selectedSessionId.value) {
      projectOverview.value = overview
      writeRoute('project', projectId)
      syncProjectPoll()
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
  workflowView.value = null
  workflowLibraryOpen.value = false
  mobileSidebarOpen.value = false
  closeSessionStream()
  clearPoll()
  try {
    const [view, events] = await Promise.all([api.getSession(sessionId), api.listSessionEvents(sessionId)])
    if (selectedSessionId.value !== sessionId) return false
    selectedProjectId.value = view.session.project_id
    sessionView.value = view
    sessionEvents.value = events
    await Promise.all([
      ensureProjectSkills(view.session.project_id),
      refreshProjectIndex(view.session.project_id),
      loadWorkflowPrograms(view.session.project_id),
    ])
    connectSessionStream(sessionId, events.at(-1)?.sequence ?? 0)
    const latestWorkflow = view.workflows[0]
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
    const sessions = sessionsByProject[view.session.project_id] ?? []
    sessionsByProject[view.session.project_id] = [
      view.session,
      ...sessions.filter((session) => session.id !== view.session.id),
    ].sort((left, right) => right.updated_at.localeCompare(left.updated_at))
    await refreshProjectIndex(view.session.project_id)
    if (workflowView.value) {
      const current = view.workflows.find((workflow) => workflow.id === workflowView.value?.workflow.id)
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

function syncProjectPoll() {
  clearPoll()
  const overview = projectOverview.value
  if (!overview || selectedSessionId.value || workflowLibraryOpen.value) return
  const delay = projectPollDelay(overview)
  if (delay === null) return
  const projectId = overview.project.id
  pollTimer = window.setTimeout(async () => {
    pollTimer = null
    if (
      selectedProjectId.value !== projectId ||
      selectedSessionId.value ||
      workflowLibraryOpen.value
    ) return
    try {
      await refreshProjectIndex(projectId)
    } catch (error) {
      globalError.value = messageOf(error)
    }
    if (selectedProjectId.value === projectId && !selectedSessionId.value) syncProjectPoll()
  }, delay)
}

function projectPollDelay(overview: ProjectOverviewType): number | null {
  const active = overview.workflows.filter(
    (workflow) => !['completed', 'failed', 'cancelled'].includes(workflow.status),
  )
  if (!active.length) return null
  if (active.some((workflow) => ['created', 'running'].includes(workflow.status))) return 900
  const timerIntervals = active
    .filter((workflow) => workflow.status === 'waiting_for_timer')
    .map((workflow) => Number(workflow.params.interval_minutes ?? 0))
    .filter((minutes) => Number.isFinite(minutes) && minutes > 0)
  if (timerIntervals.length) {
    const shortestIntervalMs = Math.min(...timerIntervals) * 60_000
    return Math.min(30_000, Math.max(1_000, shortestIntervalMs / 2))
  }
  return 5_000
}

function hasActiveWork(view: SessionView): boolean {
  return (
    view.turns.some((turn) => turn.status === 'queued' || turn.status === 'running') ||
    view.workflows.some((workflow) =>
      workflow.status === 'created' ||
      (workflow.status === 'running' && !workflow.attention_required) ||
      ['waiting_for_timer', 'waiting_for_signal'].includes(workflow.status),
    )
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
  if (selectedProjectId.value) void selectProject(selectedProjectId.value)
  else {
    const firstAvailable = projects.value.find((project) => project.available)
    if (firstAvailable) void selectProject(firstAvailable.id)
  }
}

async function openWorkflowLibrary(projectId = selectedProjectId.value ?? projects.value.find((project) => project.available)?.id): Promise<boolean> {
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
  clearPoll()
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
  skillDialogOpen.value = false
  workflowDialogOpen.value = false
  workflowDialogProjectId.value = null
  workflowDialogOriginSession.value = null
  dialogError.value = ''
}

async function createProject(input: { name: string; description: string; workspacePath: string }) {
  dialogBusy.value = true
  dialogError.value = ''
  try {
    const project = await api.createProject(input.name, input.description, input.workspacePath)
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
      const next = projects.value.find((item) => item.available)
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

async function createTurn(input: string) {
  const view = sessionView.value
  if (!view) return
  try {
    const humanRequest = view.human_requests.find(
      (request) =>
        request.status === 'open' &&
        request.session_id === view.session.id &&
        (!request.response_schema.type || request.response_schema.type === 'string'),
    )
    if (humanRequest) {
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
      scheduleSessionRefresh(view.session.id)
      syncPoll()
      return
    }
    const turn = await api.createTurn(view.session.id, input)
    if (sessionView.value?.session.id === view.session.id) {
      sessionView.value = { ...sessionView.value, turns: [...sessionView.value.turns, turn] }
      syncPoll()
    }
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
    const sessions = sessionsByProject[session.project_id] ?? []
    sessionsByProject[session.project_id] = [
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

async function updateProjectSystemPrompt(systemPrompt: string) {
  const overview = projectOverview.value
  if (!overview || promptBusy.value) return
  promptBusy.value = true
  try {
    const prompt = await api.updateProjectSystemPrompt(overview.project.id, systemPrompt)
    if (projectOverview.value?.project.id === overview.project.id) {
      projectOverview.value = { ...projectOverview.value, system_prompt: prompt }
    }
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
  if (!overview || summaryBusy.value) return
  summaryBusy.value = true
  globalError.value = ''
  try {
    const workflow = await api.createWorkflow(overview.project.id, {
      program_slug: 'project-summary',
      request:
        input.intervalMinutes > 0
          ? `Refresh the Project progress page every ${input.intervalMinutes} minutes.`
          : 'Refresh the Project progress page now.',
      instructions: input.instructions.trim(),
      params: {
        interval_minutes: input.intervalMinutes,
        max_sessions: 50,
        turns_per_session: 12,
        max_artifacts: 50,
      },
      model: '',
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

async function stopProjectSummary(workflowId: string) {
  const overview = projectOverview.value
  if (!overview || summaryBusy.value) return
  summaryBusy.value = true
  try {
    await api.cancelWorkflow(workflowId)
    await refreshProjectIndex(overview.project.id)
    syncProjectPoll()
  } catch (error) {
    globalError.value = messageOf(error)
  } finally {
    summaryBusy.value = false
  }
}

async function waitForProjectSummary(projectId: string, workflowId: string) {
  for (let attempt = 0; attempt < 240; attempt += 1) {
    const overview = await refreshProjectIndex(projectId)
    const workflow = overview.workflows.find((candidate) => candidate.id === workflowId)
    const artifact = overview.artifacts.find(
      (candidate) => candidate.workflow_id === workflowId && candidate.metadata.role === 'project_summary',
    )
    if (workflow?.status === 'failed' || workflow?.status === 'cancelled') {
      throw new Error(workflow.error ?? workflow.status)
    }
    if (artifact && workflow && ['completed', 'waiting_for_timer'].includes(workflow.status)) {
      syncProjectPoll()
      return
    }
    await new Promise((resolve) => window.setTimeout(resolve, 500))
  }
  throw new Error(t('project.summaryTimeout'))
}

async function createSkill(input: { slug: string; name: string; description: string; instructions: string }) {
  const projectId = selectedProjectId.value
  if (!projectId) return
  dialogBusy.value = true
  dialogError.value = ''
  try {
    const skill = await api.createProjectSkill(projectId, input)
    skillsByProject[projectId] = [...(skillsByProject[projectId] ?? []), skill].sort((left, right) =>
      left.name.localeCompare(right.name),
    )
    skillDialogOpen.value = false
  } catch (error) {
    dialogError.value = messageOf(error)
  } finally {
    dialogBusy.value = false
  }
}

async function createWorkflow(input: {
  workflow: WorkflowProgram
  request: string
  instructions: string
  params: Record<string, unknown>
  contextMode: WorkflowContextMode
  model: string
  access: AgentAccessProfile
  enabledSkills: string[]
  agentAccessOverrides: Record<string, AgentAccessProfile>
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
      syncPoll()
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
    if (selectedSessionId.value) scheduleSessionRefresh(selectedSessionId.value)
  } catch (error) {
    globalError.value = messageOf(error)
  }
}

async function pauseWorkflow(workflowId: string) {
  try {
    await api.pauseWorkflow(workflowId)
    if (selectedSessionId.value) scheduleSessionRefresh(selectedSessionId.value)
  } catch (error) {
    globalError.value = messageOf(error)
  }
}

async function resumeWorkflow(workflowId: string) {
  try {
    await api.resumeWorkflow(workflowId)
    if (selectedSessionId.value) scheduleSessionRefresh(selectedSessionId.value)
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
