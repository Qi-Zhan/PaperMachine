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
      <button class="primary-button" type="button" @click="$emit('new-session')">
        <MessageSquarePlus :size="16" />
        {{ t('sidebar.newSession') }}
      </button>
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
              <small>{{ group.workflow.objective }}</small>
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
                <strong>{{ workflow.objective }}</strong>
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
          <span>{{ overview.artifacts.length }}</span>
        </div>
        <div v-if="overview.artifacts.length" class="artifact-grid">
          <button
            v-for="artifact in overview.artifacts.slice(0, 12)"
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
  Save,
  Sparkles,
} from '@lucide/vue'
import { computed, ref, watch } from 'vue'
import { formatDate, formatDateTime } from '../format'
import { useAppI18n } from '../i18n'
import type { Artifact, ProjectOverview, ProjectSkill, Workflow } from '../types'
import StatusBadge from './StatusBadge.vue'

const props = defineProps<{ overview: ProjectOverview; skills: ProjectSkill[]; promptBusy: boolean }>()
const { t } = useAppI18n()
const emit = defineEmits<{
  'open-sidebar': []
  'new-session': []
  'new-skill': []
  'select-session': [sessionId: string]
  'open-artifact': [artifact: Artifact]
  'update-system-prompt': [systemPrompt: string]
}>()

const projectPromptDraft = ref(props.overview.system_prompt.content)
const projectPromptChanged = computed(
  () => projectPromptDraft.value !== props.overview.system_prompt.content,
)

watch(
  () => [props.overview.project.id, props.overview.system_prompt.content] as const,
  () => {
    projectPromptDraft.value = props.overview.system_prompt.content
  },
)

const activeSessions = computed(
  () => props.overview.sessions.filter((session) =>
    ['running', 'waiting_for_human', 'paused'].includes(session.status),
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
</script>
