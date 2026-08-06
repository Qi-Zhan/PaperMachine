<template>
  <div class="research-overview">
    <header class="page-header research-page-header">
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
          <p class="eyebrow">Research</p>
          <h1>{{ overview.research.name }}</h1>
        </div>
      </div>
      <button class="primary-button" type="button" @click="$emit('new-session')">
        <MessageSquarePlus :size="16" />
        {{ t('sidebar.newSession') }}
      </button>
    </header>

    <main class="research-content">
      <section class="research-summary">
        <p class="research-description">{{ overview.research.description || t('research.noDescription') }}</p>
        <dl class="summary-metrics">
          <div>
            <dt>{{ t('research.sessions') }}</dt>
            <dd>{{ overview.sessions.length }}</dd>
          </div>
          <div>
            <dt>{{ t('research.active') }}</dt>
            <dd>{{ activeSessions }}</dd>
          </div>
          <div>
            <dt>{{ t('research.workflowRuns') }}</dt>
            <dd>{{ overview.workflow_runs.length }}</dd>
          </div>
          <div>
            <dt>{{ t('research.artifacts') }}</dt>
            <dd>{{ overview.artifacts.length }}</dd>
          </div>
        </dl>
      </section>

      <section class="overview-section sessions-index">
        <div class="section-heading">
          <h2>{{ t('research.sessions') }}</h2>
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
          :key="group.run.id"
          class="workflow-session-group"
        >
          <div class="workflow-session-group-heading">
            <GitBranch :size="14" />
            <span>
              <strong>{{ group.run.workflow.manifest.name }}</strong>
              <small>{{ group.run.objective }}</small>
            </span>
            <StatusBadge :status="group.run.status" />
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
          <span>{{ t('research.noSessions') }}</span>
          <button class="text-command" type="button" @click="$emit('new-session')">{{ t('research.createSession') }}</button>
        </div>
      </section>

      <div class="overview-columns">
        <section class="overview-section">
          <div class="section-heading">
            <h2>{{ t('research.workflowActivity') }}</h2>
            <span>{{ overview.workflow_runs.length }}</span>
          </div>
          <div v-if="overview.workflow_runs.length" class="compact-list">
            <button
              v-for="workflowRun in overview.workflow_runs.slice(0, 8)"
              :key="workflowRun.id"
              type="button"
              @click="$emit('select-session', workflowRun.origin_session_id)"
            >
              <GitBranch :size="14" />
              <span>
                <strong>{{ workflowRun.objective }}</strong>
                <small>{{ formatDate(workflowRun.updated_at) }}</small>
              </span>
              <StatusBadge :status="workflowRun.status" />
            </button>
          </div>
          <p v-else class="section-empty">{{ t('research.noWorkflowActivity') }}</p>
        </section>

        <section class="overview-section">
          <div class="section-heading">
            <h2>{{ t('research.skills') }}</h2>
            <button
              class="icon-button"
              type="button"
              :title="t('research.newSkill')"
              :aria-label="t('research.newSkill')"
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
          <p v-else class="section-empty">{{ t('research.noSkills') }}</p>
        </section>
      </div>

      <section class="overview-section artifact-index">
        <div class="section-heading">
          <h2>{{ t('research.artifacts') }}</h2>
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
        <p v-else class="section-empty">{{ t('research.noArtifacts') }}</p>
      </section>
    </main>

    <footer class="overview-composer">
      <button class="composer-shell" type="button" @click="$emit('new-session')">
        <span>{{ t('research.startSessionIn', { name: overview.research.name }) }}</span>
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
  PanelLeft,
  Plus,
  Sparkles,
} from '@lucide/vue'
import { computed } from 'vue'
import { formatDate, formatDateTime } from '../format'
import { useAppI18n } from '../i18n'
import type { Artifact, ResearchOverview, ResearchSkill } from '../types'
import StatusBadge from './StatusBadge.vue'

const props = defineProps<{ overview: ResearchOverview; skills: ResearchSkill[] }>()
const { t } = useAppI18n()
defineEmits<{
  'open-sidebar': []
  'new-session': []
  'new-skill': []
  'select-session': [sessionId: string]
  'open-artifact': [artifact: Artifact]
}>()

const activeSessions = computed(
  () => props.overview.sessions.filter((session) =>
    ['running', 'waiting_for_human', 'paused'].includes(session.status),
  ).length,
)
const standaloneSessions = computed(() =>
  props.overview.sessions.filter((session) => session.origin === 'user'),
)
const workflowGroups = computed(() =>
  props.overview.workflow_runs
    .map((run) => ({
      run,
      sessions: props.overview.workflow_participants
        .filter((participant) => participant.workflow_run_id === run.id)
        .map((participant) => ({
          participant,
          session: props.overview.sessions.find((session) => session.id === participant.session_id),
        }))
        .filter((item): item is { participant: typeof item.participant; session: NonNullable<typeof item.session> } => Boolean(item.session)),
    }))
    .filter((group) => group.sessions.length),
)
</script>
