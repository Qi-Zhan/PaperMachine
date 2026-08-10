<template>
  <aside class="sidebar-shell">
    <header class="sidebar-toolbar">
      <button class="brand-button" type="button" title="PaperMachine" @click="$emit('home')">
        <span class="brand-mark" aria-hidden="true"><ScanSearch :size="16" /></span>
        <span>PaperMachine</span>
      </button>
      <div class="toolbar-actions">
        <button
          class="icon-button sidebar-desktop-toggle"
          type="button"
          :title="t('common.toggleSidebar')"
          :aria-label="t('common.toggleSidebar')"
          @click="$emit('toggle-sidebar')"
        >
          <PanelLeft :size="18" />
        </button>
        <button
          class="icon-button sidebar-mobile-close"
          type="button"
          :title="t('common.closeSidebar')"
          :aria-label="t('common.closeSidebar')"
          @click="$emit('close-sidebar')"
        >
          <X :size="16" />
        </button>
      </div>
    </header>

    <div class="sidebar-primary-nav">
      <button type="button" :data-active="workflowsActive" @click="$emit('open-workflows')">
        <GitBranch :size="15" />
        <span>{{ t('sidebar.workflow') }}</span>
      </button>
    </div>

    <nav class="project-tree" :aria-label="t('sidebar.project')">
      <div class="sidebar-section-heading">
        <p class="sidebar-section-label">{{ t('sidebar.project') }}</p>
        <div class="sidebar-section-actions">
          <button
            class="icon-button"
            type="button"
            :title="t('sidebar.searchProject')"
            :aria-label="t('sidebar.searchProject')"
            @click="searchVisible = !searchVisible"
          >
            <Search :size="15" />
          </button>
          <button
            class="icon-button"
            type="button"
            :title="t('sidebar.newProject')"
            :aria-label="t('sidebar.newProject')"
            @click="$emit('new-project')"
          >
            <FolderPlus :size="15" />
          </button>
        </div>
      </div>
      <div v-if="searchVisible" class="sidebar-search">
        <Search :size="14" />
        <input v-model="query" :aria-label="t('sidebar.filter')" :placeholder="t('sidebar.filter')" />
        <button v-if="query" type="button" :title="t('sidebar.clearSearch')" :aria-label="t('sidebar.clearSearch')" @click="query = ''">
          <X :size="14" />
        </button>
      </div>
      <p v-if="projects.length === 0" class="sidebar-empty">{{ t('sidebar.noProject') }}</p>
      <section v-for="project in filteredProjects" :key="project.id" class="project-group">
        <div
          class="project-row"
          :data-active="project.id === selectedProjectId && !selectedSessionId"
          :data-workspace-available="project.workspace_available"
        >
          <button
            class="project-select"
            type="button"
            :title="project.workspace_available ? project.workspace.path : t('sidebar.projectMissing')"
            @click="$emit('select-project', project.id)"
          >
            <ChevronDown :size="14" />
            <Folder v-if="project.workspace_available" :size="15" />
            <AlertTriangle v-else :size="15" />
            <span>{{ project.name }}</span>
          </button>
          <button
            v-if="project.workspace_available"
            class="row-icon-button"
            type="button"
            :title="t('sidebar.newSession')"
            :aria-label="t('sidebar.newSession')"
            @click="$emit('new-session', project.id)"
          >
            <Plus :size="14" />
          </button>
          <button
            v-else
            class="row-icon-button"
            type="button"
            :title="t('sidebar.relocateProject')"
            :aria-label="t('sidebar.relocateProject')"
            @click="$emit('relocate-project', project.id)"
          >
            <MapPin :size="14" />
          </button>
          <details class="project-menu">
            <summary class="row-icon-button" :title="t('sidebar.projectActions')" :aria-label="t('sidebar.projectActions')">
              <MoreHorizontal :size="14" />
            </summary>
            <div class="project-menu-popover">
              <button type="button" @click="$emit('relocate-project', project.id)">
                <MapPin :size="13" />
                <span>{{ t('sidebar.relocateProject') }}</span>
              </button>
              <button class="danger-hover" type="button" @click="$emit('remove-project', project.id)">
                <Trash2 :size="13" />
                <span>{{ t('sidebar.removeProject') }}</span>
              </button>
            </div>
          </details>
        </div>
        <div class="session-list">
          <button
            v-for="session in visibleSessions(project.id)"
            :key="session.id"
            class="session-row"
            :data-active="session.id === selectedSessionId"
            type="button"
            @click="$emit('select-session', project.id, session.id)"
          >
            <span
              class="status-pin"
              :data-status="session.status"
              :title="statusLabel(session.status)"
              :aria-label="statusLabel(session.status)"
              role="img"
            />
            <span class="session-row-copy">
              <span class="session-title">{{ session.title }}</span>
              <span class="session-time">{{ formatDate(session.updated_at) }}</span>
            </span>
          </button>
        </div>
      </section>
    </nav>

    <footer class="sidebar-footer">
      <span class="connection-dot" :data-online="online" />
      <span :title="modelLabel">{{ mode === 'demo' ? t('sidebar.demoModel') : modelLabel }}</span>
      <span class="sidebar-footer-spacer" />
      <div class="locale-switch" role="group" :aria-label="t('common.language')">
        <button type="button" :data-active="locale === 'zh-CN'" @click="setLocale('zh-CN')">中</button>
        <button type="button" :data-active="locale === 'en'" @click="setLocale('en')">EN</button>
      </div>
      <span>{{ online ? t('sidebar.connected') : t('sidebar.offline') }}</span>
    </footer>
  </aside>
</template>

<script setup lang="ts">
import { AlertTriangle, ChevronDown, Folder, FolderPlus, GitBranch, MapPin, MoreHorizontal, PanelLeft, Plus, ScanSearch, Search, Trash2, X } from '@lucide/vue'
import { computed, ref } from 'vue'
import { formatDate, statusLabel } from '../format'
import { useAppI18n } from '../i18n'
import type { ProjectCatalogEntry, Session } from '../types'

const props = defineProps<{
  projects: ProjectCatalogEntry[]
  sessionsByProject: Record<string, Session[]>
  selectedProjectId: string | null
  selectedSessionId: string | null
  mode: string
  modelLabel: string
  online: boolean
  workflowsActive: boolean
}>()

defineEmits<{
  home: []
  'close-sidebar': []
  'toggle-sidebar': []
  'new-project': []
  'new-session': [projectId: string]
  'relocate-project': [projectId: string]
  'remove-project': [projectId: string]
  'open-workflows': []
  'select-project': [projectId: string]
  'select-session': [projectId: string, sessionId: string]
}>()

const searchVisible = ref(false)
const query = ref('')
const { locale, setLocale, t } = useAppI18n()
const normalizedQuery = computed(() => query.value.trim().toLocaleLowerCase())
const filteredProjects = computed(() => {
  if (!normalizedQuery.value) return props.projects
  return props.projects.filter((project) => {
    if (project.name.toLocaleLowerCase().includes(normalizedQuery.value)) return true
    return (props.sessionsByProject[project.id] ?? []).some((session) =>
      session.title.toLocaleLowerCase().includes(normalizedQuery.value),
    )
  })
})

function visibleSessions(projectId: string): Session[] {
  const sessions = (props.sessionsByProject[projectId] ?? []).filter(
    (session) => session.archived_at === null,
  )
  if (!normalizedQuery.value) return sessions
  return sessions.filter((session) =>
    session.title.toLocaleLowerCase().includes(normalizedQuery.value),
  )
}
</script>
