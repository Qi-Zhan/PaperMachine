<template>
  <div class="project-overview">
    <header class="page-header project-page-header">
      <div class="page-leading">
        <button
          class="icon-button mobile-only"
          type="button"
          :title="t('common.openSidebar')"
          :aria-label="t('common.openSidebar')"
          @click="$emit('open-sidebar')"
        >
          <PanelLeft :size="18" />
        </button>
        <div>
          <p class="eyebrow">Project</p>
          <h1>{{ overview.project.name }}</h1>
        </div>
      </div>
      <div class="project-header-actions">
        <button class="secondary-button" type="button" @click="$emit('run-workflow')">
          <GitBranch :size="15" />
          {{ t('project.runWorkflow') }}
        </button>
        <button class="primary-button" type="button" @click="$emit('new-session')">
          <MessageSquarePlus :size="16" />
          {{ t('sidebar.newSession') }}
        </button>
      </div>
    </header>

    <main class="project-content">
      <section class="project-summary">
        <p class="project-description">{{ overview.project.description || t('project.noDescription') }}</p>
        <dl class="summary-metrics">
          <div>
            <dt>{{ t('project.sessions') }}</dt>
            <dd>{{ overview.sessions.length }}</dd>
          </div>
          <div>
            <dt>{{ t('project.active') }}</dt>
            <dd>{{ activeSessions }}</dd>
          </div>
          <div>
            <dt>{{ t('project.workflowRuns') }}</dt>
            <dd>{{ overview.workflows.length }}</dd>
          </div>
          <div>
            <dt>{{ t('project.artifacts') }}</dt>
            <dd>{{ overview.artifacts.length }}</dd>
          </div>
        </dl>
      </section>

      <section class="overview-section project-progress-panel">
        <div class="project-progress-heading">
          <div>
            <p class="eyebrow">{{ t('project.summaryEyebrow') }}</p>
            <h2>{{ t('project.summaryTitle') }}</h2>
            <p>{{ t('project.summaryDescription') }}</p>
          </div>
          <div class="project-progress-actions">
            <button
              class="secondary-button"
              type="button"
              :disabled="summaryBusy"
              @click="runSummary(0)"
            >
              <LoaderCircle v-if="summaryBusy" class="spin" :size="14" />
              <RefreshCw v-else :size="14" />
              {{ t('project.summaryRefreshNow') }}
            </button>
            <button
              class="primary-button"
              type="button"
              :disabled="summaryBusy || summaryIntervalDraft <= 0"
              @click="runSummary(summaryIntervalDraft)"
            >
              <Clock3 :size="14" />
              {{ activeSummaryWorkflow ? t('project.summaryUpdateSchedule') : t('project.summaryStartSchedule') }}
            </button>
            <button
              v-if="activeSummaryWorkflow"
              class="text-command"
              type="button"
              :disabled="summaryBusy"
              @click="$emit('stop-summary', activeSummaryWorkflow.id)"
            >
              {{ t('project.summaryStopSchedule') }}
            </button>
          </div>
        </div>

        <div class="project-progress-config">
          <label>
            <span>{{ t('project.summaryInterval') }}</span>
            <span class="summary-interval-input">
              <input v-model.number="summaryIntervalDraft" type="number" min="0.1" step="0.1" />
              <small>{{ t('project.minutes') }}</small>
            </span>
          </label>
          <label class="summary-prompt-field">
            <span>{{ t('project.summaryInstructions') }}</span>
            <textarea
              v-model="summaryInstructionsDraft"
              class="text-area"
              :placeholder="t('project.summaryInstructionsPlaceholder')"
            />
          </label>
        </div>

        <div v-if="latestSummaryArtifact" class="project-progress-frame">
          <div class="project-progress-frame-meta">
            <span>
              <span class="status-pin" :data-status="activeSummaryWorkflow ? 'waiting_for_timer' : 'completed'" />
              {{ t('project.summaryUpdated', { date: formatDateTime(latestSummaryArtifact.created_at) }) }}
            </span>
            <button class="text-command" type="button" @click="$emit('open-artifact', latestSummaryArtifact)">
              {{ t('project.summaryOpenArtifact') }}
            </button>
          </div>
          <iframe
            :src="api.artifactUrl(latestSummaryArtifact)"
            :title="t('project.summaryFrameTitle')"
            sandbox=""
          />
        </div>
        <div v-else class="project-progress-empty">
          <FileText :size="20" />
          <div>
            <strong>{{ t('project.summaryEmptyTitle') }}</strong>
            <p>{{ t('project.summaryEmptyDescription') }}</p>
          </div>
        </div>
      </section>

      <section class="overview-section sessions-index">
        <div class="section-heading">
          <h2>{{ t('project.sessions') }}</h2>
          <span>{{ overview.sessions.length }}</span>
        </div>
        <div v-if="standaloneSessions.length" class="data-list" role="list">
          <button
            v-for="session in standaloneSessions"
            :key="session.id"
            class="data-row session-index-row"
            type="button"
            @click="$emit('select-session', session.id)"
          >
            <span class="status-pin" :data-status="session.status" />
            <span class="data-row-copy">
              <strong>{{ session.title }}</strong>
              <small>{{ session.model }} · {{ formatDateTime(session.updated_at) }}</small>
            </span>
            <span v-if="session.enabled_skills.length" class="row-meta">
              <Sparkles :size="12" /> {{ session.enabled_skills.length }}
            </span>
            <StatusBadge :status="session.status" />
            <ChevronRight :size="15" />
          </button>
        </div>
        <div
          v-for="group in workflowGroups"
          :key="group.workflow.id"
          class="workflow-session-group"
        >
          <div class="workflow-session-group-heading">
            <GitBranch :size="14" />
            <span>
              <strong>{{ group.workflow.program.manifest.name }}</strong>
              <small v-if="group.workflow.request">{{ group.workflow.request }}</small>
            </span>
            <StatusBadge :status="group.workflow.status" />
          </div>
          <div class="data-list" role="list">
            <button
              v-for="item in group.sessions"
              :key="item.session.id"
              class="data-row session-index-row"
              type="button"
              @click="$emit('select-session', item.session.id)"
            >
              <span class="status-pin" :data-status="item.session.status" />
              <span class="data-row-copy">
                <strong>{{ item.session.title }}</strong>
                <small>{{ item.participant.role }} · {{ formatDateTime(item.session.updated_at) }}</small>
              </span>
              <StatusBadge :status="item.session.status" />
              <ChevronRight :size="15" />
            </button>
          </div>
        </div>
        <div v-if="!standaloneSessions.length && !workflowGroups.length" class="empty-band">
          <MessageSquare :size="18" />
          <span>{{ t('project.noSessions') }}</span>
          <button class="text-command" type="button" @click="$emit('new-session')">{{ t('project.createSession') }}</button>
        </div>
      </section>

      <div class="overview-columns">
        <section class="overview-section">
          <div class="section-heading">
            <h2>{{ t('project.workflowActivity') }}</h2>
            <span>{{ overview.workflows.length }}</span>
          </div>
          <div v-if="overview.workflows.length" class="compact-list">
            <button
              v-for="workflow in overview.workflows.slice(0, 8)"
              :key="workflow.id"
              type="button"
              :disabled="!workflowSessionId(workflow)"
              @click="openWorkflowSession(workflow)"
            >
              <GitBranch :size="14" />
              <span>
                <strong>{{ workflowTitle(workflow) }}</strong>
                <small>{{ workflow.program.manifest.name }} · {{ formatDate(workflow.updated_at) }}</small>
              </span>
              <StatusBadge :status="workflow.status" />
            </button>
          </div>
          <p v-else class="section-empty">{{ t('project.noWorkflowActivity') }}</p>
        </section>

        <section class="overview-section">
          <div class="section-heading">
            <h2>{{ t('project.skills') }}</h2>
            <button
              class="icon-button"
              type="button"
              :title="t('project.newSkill')"
              :aria-label="t('project.newSkill')"
              @click="$emit('new-skill')"
            >
              <Plus :size="15" />
            </button>
          </div>
          <div v-if="skills.length" class="compact-list skill-overview-list">
            <div v-for="skill in skills" :key="skill.slug">
              <FileCode2 :size="14" />
              <span>
                <strong>{{ skill.name }}</strong>
                <small>{{ skill.description || skill.slug }}</small>
              </span>
              <code>{{ skill.slug }}</code>
            </div>
          </div>
          <p v-else class="section-empty">{{ t('project.noSkills') }}</p>
        </section>
      </div>

      <section class="overview-section project-prompt-editor">
        <div class="section-heading">
          <div>
            <h2>{{ t('prompt.projectSystemPrompt') }}</h2>
            <small>{{ overview.system_prompt.relative_path }}</small>
          </div>
          <button
            class="secondary-button"
            type="button"
            :disabled="promptBusy || !projectPromptChanged"
            @click="$emit('update-system-prompt', projectPromptDraft)"
          >
            <LoaderCircle v-if="promptBusy" class="spin" :size="14" />
            <Save v-else :size="14" />
            {{ t('common.save') }}
          </button>
        </div>
        <textarea
          v-model="projectPromptDraft"
          class="text-area project-system-prompt-input"
          :placeholder="t('prompt.projectPlaceholder')"
          :disabled="promptBusy"
        />
        <p class="field-note">{{ t('prompt.futureTurns') }}</p>
      </section>

      <section class="overview-section artifact-index">
        <div class="section-heading">
          <h2>{{ t('project.artifacts') }}</h2>
          <span>{{ researchArtifacts.length }}</span>
        </div>
        <div v-if="researchArtifacts.length" class="artifact-grid">
          <button
            v-for="artifact in researchArtifacts.slice(0, 12)"
            :key="artifact.id"
            type="button"
            @click="$emit('open-artifact', artifact)"
          >
            <FileText :size="15" />
            <span>
              <strong>{{ artifact.name }}</strong>
              <small>{{ artifact.kind }} · {{ formatDate(artifact.created_at) }}</small>
            </span>
            <ExternalLink :size="13" />
          </button>
        </div>
        <p v-else class="section-empty">{{ t('project.noArtifacts') }}</p>
      </section>
    </main>

    <footer class="overview-composer">
      <button class="composer-shell" type="button" @click="$emit('new-session')">
        <span>{{ t('project.startSessionIn', { name: overview.project.name }) }}</span>
        <span class="composer-submit"><ArrowUp :size="17" /></span>
      </button>
    </footer>
  </div>
</template>

<script setup lang="ts">
import {
  ArrowUp,
  ChevronRight,
  ExternalLink,
  FileCode2,
  FileText,
  GitBranch,
  MessageSquare,
  MessageSquarePlus,
  LoaderCircle,
  PanelLeft,
  Plus,
  RefreshCw,
  Save,
  Sparkles,
  Clock3,
} from '@lucide/vue'
import { computed, ref, watch } from 'vue'
import { api } from '../api'
import { formatDate, formatDateTime, workflowTitle } from '../format'
import { useAppI18n } from '../i18n'
import type { Artifact, ProjectOverview, ProjectSkill, Workflow } from '../types'
import StatusBadge from './StatusBadge.vue'

const props = defineProps<{
  overview: ProjectOverview
  skills: ProjectSkill[]
  promptBusy: boolean
  summaryBusy: boolean
}>()
const { t } = useAppI18n()
const emit = defineEmits<{
  'open-sidebar': []
  'new-session': []
  'new-skill': []
  'select-session': [sessionId: string]
  'open-artifact': [artifact: Artifact]
  'update-system-prompt': [systemPrompt: string]
  'run-workflow': []
  'run-summary': [input: { instructions: string; intervalMinutes: number; replaceWorkflowId?: string }]
  'stop-summary': [workflowId: string]
}>()

const defaultSummaryInstructions =
  'Summarize the current research state for a project collaborator. Prioritize evidence-backed conclusions, active work, blockers, unresolved questions, and concrete next steps. Keep provenance visible and do not hide failed or inconclusive routes.'

const projectPromptDraft = ref(props.overview.system_prompt.content)
const projectPromptChanged = computed(
  () => projectPromptDraft.value !== props.overview.system_prompt.content,
)
const summaryWorkflows = computed(() =>
  props.overview.workflows.filter((workflow) => workflow.program.manifest.slug === 'project-summary'),
)
const activeSummaryWorkflow = computed(() =>
  summaryWorkflows.value.find(
    (workflow) =>
      !['completed', 'failed', 'cancelled'].includes(workflow.status) &&
      Number(workflow.params.interval_minutes ?? 0) > 0,
  ),
)
const summaryInstructionsWorkflow = computed(() => activeSummaryWorkflow.value ?? summaryWorkflows.value[0])
const scheduledSummaryWorkflow = computed(
  () =>
    activeSummaryWorkflow.value ??
    summaryWorkflows.value.find((workflow) => Number(workflow.params.interval_minutes ?? 0) > 0),
)
const summaryInstructionsDraft = ref(
  summaryInstructionsWorkflow.value?.instructions || defaultSummaryInstructions,
)
const summaryIntervalDraft = ref(
  Number(scheduledSummaryWorkflow.value?.params.interval_minutes ?? 60),
)
const latestSummaryArtifact = computed(() =>
  props.overview.artifacts.find((artifact) => artifact.metadata.role === 'project_summary'),
)
const researchArtifacts = computed(() =>
  props.overview.artifacts.filter((artifact) => artifact.metadata.role !== 'project_summary'),
)

watch(
  () => [props.overview.project.id, props.overview.system_prompt.content] as const,
  () => {
    projectPromptDraft.value = props.overview.system_prompt.content
  },
)

watch(
  () => [
    props.overview.project.id,
    summaryInstructionsWorkflow.value?.id,
    summaryInstructionsWorkflow.value?.instructions,
  ] as const,
  () => {
    summaryInstructionsDraft.value =
      summaryInstructionsWorkflow.value?.instructions || defaultSummaryInstructions
  },
)

watch(
  () => [
    props.overview.project.id,
    scheduledSummaryWorkflow.value?.id,
    scheduledSummaryWorkflow.value?.params.interval_minutes,
  ] as const,
  () => {
    summaryIntervalDraft.value = Number(
      scheduledSummaryWorkflow.value?.params.interval_minutes ?? 60,
    )
  },
)

const activeSessions = computed(
  () => props.overview.sessions.filter((session) =>
    ['running', 'paused'].includes(session.status),
  ).length,
)
const standaloneSessions = computed(() =>
  props.overview.sessions.filter((session) => session.origin === 'user'),
)
const workflowGroups = computed(() =>
  props.overview.workflows
    .map((workflow) => ({
      workflow,
      sessions: props.overview.workflow_participants
        .filter((participant) => participant.workflow_id === workflow.id)
        .map((participant) => ({
          participant,
          session: props.overview.sessions.find((session) => session.id === participant.session_id),
        }))
        .filter((item): item is { participant: typeof item.participant; session: NonNullable<typeof item.session> } => Boolean(item.session)),
    }))
    .filter((group) => group.sessions.length),
)

function workflowSessionId(workflow: Workflow): string | null {
  if (workflow.started_from_session_id) return workflow.started_from_session_id
  return props.overview.workflow_participants.find((participant) => participant.workflow_id === workflow.id)?.session_id ?? null
}

function openWorkflowSession(workflow: Workflow) {
  const sessionId = workflowSessionId(workflow)
  if (sessionId) emit('select-session', sessionId)
}

function runSummary(intervalMinutes: number) {
  emit('run-summary', {
    instructions: summaryInstructionsDraft.value,
    intervalMinutes,
    replaceWorkflowId:
      intervalMinutes > 0 ? activeSummaryWorkflow.value?.id : undefined,
  })
}
</script>
