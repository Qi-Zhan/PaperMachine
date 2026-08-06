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
        :online="online"
        :workflows-active="workflowLibraryOpen"
        @home="showHome"
        @close-sidebar="mobileSidebarOpen = false"
        @new-project="projectDialogOpen = true"
        @new-session="openSessionDialog"
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

      <div v-else-if="projects.length === 0" class="zero-state">
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
        @open-sidebar="mobileSidebarOpen = true"
        @select-project="selectProject"
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

      <ProjectOverview
        v-else-if="projectOverview"
        :overview="projectOverview"
        :skills="projectSkills"
        @open-sidebar="mobileSidebarOpen = true"
        @new-session="openSessionDialog(projectOverview.project.id)"
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

    <NewProjectDialog
      :open="projectDialogOpen"
      :busy="dialogBusy"
      :error="dialogError"
      @close="closeDialogs"
      @submit="createProject"
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
      :session="sessionView?.session ?? null"
      :workflows="workflowPrograms"
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
  Project,
  ProjectOverview as ProjectOverviewType,
  ProjectSkill,
  Session,
  SessionEvent,
  SessionView,
  WorkflowProgram,
  Workflow,
  WorkflowView,
} from './types'

const projects = ref<Project[]>([])
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
const projectDialogOpen = ref(false)
const sessionDialogOpen = ref(false)
const skillDialogOpen = ref(false)
const workflowDialogOpen = ref(false)
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
    await Promise.all(projectResult.map((project) => refreshProjectIndex(project.id)))
    const initialRoute = readRoute()
    let restored = false
    if (initialRoute?.kind === 'session') restored = await selectSession(initialRoute.id)
    else if (initialRoute?.kind === 'project') restored = await selectProject(initialRoute.id)
    else if (initialRoute?.kind === 'workflows') restored = await openWorkflowLibrary(initialRoute.id)
    if (!restored && projectResult[0]) await selectProject(projectResult[0].id)
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
  if (index >= 0) projects.value[index] = overview.project
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

function hasActiveWork(view: SessionView): boolean {
  return (
    view.turns.some((turn) => turn.status === 'queued' || turn.status === 'running') ||
    view.workflows.some((workflow) =>
      ['created', 'running', 'waiting_for_user', 'waiting_for_timer', 'waiting_for_signal', 'paused'].includes(workflow.status),
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
  else if (projects.value[0]) void selectProject(projects.value[0].id)
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

function closeDialogs() {
  if (dialogBusy.value) return
  projectDialogOpen.value = false
  sessionDialogOpen.value = false
  skillDialogOpen.value = false
  workflowDialogOpen.value = false
  dialogError.value = ''
}

async function createProject(input: { name: string; description: string; rootPath: string }) {
  dialogBusy.value = true
  dialogError.value = ''
  try {
    const project = await api.createProject(input.name, input.description, input.rootPath)
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

async function createSession(input: CreateSessionInput) {
  const project = dialogProject.value
  if (!project) return
  dialogBusy.value = true
  dialogError.value = ''
  try {
    const session = await api.createSession(project.id, input)
    sessionsByProject[project.id] = [session, ...(sessionsByProject[project.id] ?? [])]
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
  objective: string
  input: Record<string, unknown>
}) {
  const view = sessionView.value
  if (!view) return
  dialogBusy.value = true
  dialogError.value = ''
  try {
    const workflow = await api.createWorkflow(view.session.project_id, {
      program_slug: input.workflow.manifest.slug,
      objective: input.objective,
      input: input.input,
      started_from_session_id: view.session.id,
      model: view.session.model,
      access: view.session.access,
      enabled_skills: view.session.enabled_skills,
    })
    if (sessionView.value?.session.id === view.session.id) {
      sessionView.value = {
        ...sessionView.value,
        workflows: [workflow, ...sessionView.value.workflows.filter((candidate) => candidate.id !== workflow.id)],
      }
    }
    workflowDialogOpen.value = false
    await inspectWorkflow(workflow.id)
    syncPoll()
  } catch (error) {
    dialogError.value = messageOf(error)
  } finally {
    dialogBusy.value = false
  }
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
