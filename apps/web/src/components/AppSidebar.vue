<template>
  <aside class="sidebar-shell">
    <header class="sidebar-toolbar">
      <button class="brand-button" type="button" title="PaperMachine" @click="$emit('home')">
        <span class="brand-mark" aria-hidden="true"><ScanSearch :size="16" /></span>
        <span>PaperMachine</span>
      </button>
      <div class="toolbar-actions">
        <button
          class="icon-button"
          type="button"
          :title="t('sidebar.searchResearch')"
          :aria-label="t('sidebar.searchResearch')"
          @click="searchVisible = !searchVisible"
        >
          <Search :size="16" />
        </button>
        <button
          class="icon-button"
          type="button"
          :title="t('sidebar.newResearch')"
          :aria-label="t('sidebar.newResearch')"
          @click="$emit('new-research')"
        >
          <FolderPlus :size="16" />
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

    <div v-if="searchVisible" class="sidebar-search">
      <Search :size="14" />
      <input v-model="query" :aria-label="t('sidebar.filter')" :placeholder="t('sidebar.filter')" />
      <button v-if="query" type="button" :title="t('sidebar.clearSearch')" :aria-label="t('sidebar.clearSearch')" @click="query = ''">
        <X :size="14" />
      </button>
    </div>

    <div class="sidebar-primary-nav">
      <button type="button" :data-active="workflowsActive" @click="$emit('open-workflows')">
        <GitBranch :size="15" />
        <span>{{ t('sidebar.workflow') }}</span>
      </button>
    </div>

    <nav class="research-tree" :aria-label="t('sidebar.research')">
      <p class="sidebar-section-label">{{ t('sidebar.research') }}</p>
      <p v-if="researches.length === 0" class="sidebar-empty">{{ t('sidebar.noResearch') }}</p>
      <section v-for="research in filteredResearches" :key="research.id" class="research-group">
        <div
          class="research-row"
          :data-active="research.id === selectedResearchId && !selectedSessionId"
        >
          <button class="research-select" type="button" @click="$emit('select-research', research.id)">
            <ChevronDown :size="14" />
            <Folder :size="15" />
            <span>{{ research.name }}</span>
          </button>
          <button
            class="row-icon-button"
            type="button"
            :title="t('sidebar.newSession')"
            :aria-label="t('sidebar.newSession')"
            @click="$emit('new-session', research.id)"
          >
            <Plus :size="14" />
          </button>
        </div>
        <div class="session-list">
          <button
            v-for="session in visibleSessions(research.id)"
            :key="session.id"
            class="session-row"
            :data-active="session.id === selectedSessionId"
            type="button"
            @click="$emit('select-session', session.id)"
          >
            <span class="status-pin" :data-status="session.status" />
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
      <span>{{ mode === 'demo' ? t('sidebar.demoModel') : 'OpenAI' }}</span>
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
import { ChevronDown, Folder, FolderPlus, GitBranch, Plus, ScanSearch, Search, X } from '@lucide/vue'
import { computed, ref } from 'vue'
import { formatDate } from '../format'
import { useAppI18n } from '../i18n'
import type { Research, Session } from '../types'

const props = defineProps<{
  researches: Research[]
  sessionsByResearch: Record<string, Session[]>
  selectedResearchId: string | null
  selectedSessionId: string | null
  mode: string
  online: boolean
  workflowsActive: boolean
}>()

defineEmits<{
  home: []
  'close-sidebar': []
  'new-research': []
  'new-session': [researchId: string]
  'open-workflows': []
  'select-research': [researchId: string]
  'select-session': [sessionId: string]
}>()

const searchVisible = ref(false)
const query = ref('')
const { locale, setLocale, t } = useAppI18n()
const normalizedQuery = computed(() => query.value.trim().toLocaleLowerCase())
const filteredResearches = computed(() => {
  if (!normalizedQuery.value) return props.researches
  return props.researches.filter((research) => {
    if (research.name.toLocaleLowerCase().includes(normalizedQuery.value)) return true
    return (props.sessionsByResearch[research.id] ?? []).some((session) =>
      session.title.toLocaleLowerCase().includes(normalizedQuery.value),
    )
  })
})

function visibleSessions(researchId: string): Session[] {
  const sessions = props.sessionsByResearch[researchId] ?? []
  if (!normalizedQuery.value) return sessions
  return sessions.filter((session) =>
    session.title.toLocaleLowerCase().includes(normalizedQuery.value),
  )
}
</script>
