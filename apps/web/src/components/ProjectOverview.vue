<template>
  <div class="project-overview">
    <header class="page-header project-page-header">
      <div class="page-leading">
        <button
          class="icon-button sidebar-toggle"
          type="button"
          :title="t('common.toggleSidebar')"
          :aria-label="t('common.toggleSidebar')"
          @click="$emit('toggle-sidebar')"
        >
          <PanelLeft :size="18" />
        </button>
        <div>
          <p class="eyebrow">Project</p>
          <h1>{{ overview.project.name }}</h1>
        </div>
      </div>
      <div class="project-header-actions">
        <button
          class="secondary-button"
          type="button"
          :disabled="summaryBusy || !workspaceAvailable"
          @click="refreshSummary"
        >
          <LoaderCircle v-if="summaryBusy" class="spin" :size="14" />
          <RefreshCw v-else :size="14" />
          {{ t('project.summaryRefreshNow') }}
        </button>
        <button class="secondary-button" type="button" :disabled="!workspaceAvailable" @click="$emit('run-workflow')">
          <GitBranch :size="15" />
          {{ t('project.runWorkflow') }}
        </button>
        <button class="primary-button" type="button" :disabled="!workspaceAvailable" @click="$emit('new-session')">
          <MessageSquarePlus :size="16" />
          {{ t('sidebar.newSession') }}
        </button>
      </div>
    </header>

    <main class="project-home">
      <div v-if="summaryLoading" class="project-home-state" role="status" aria-live="polite">
        <LoaderCircle class="spin" :size="20" />
        <span>{{ t('project.summaryLoading') }}</span>
      </div>

      <iframe
        v-else-if="summaryDocument"
        class="project-home-document"
        :title="overview.project.name"
        :srcdoc="summaryDocument"
        :sandbox="PROJECT_HOME_SANDBOX"
        :csp="PROJECT_HOME_CSP"
        referrerpolicy="no-referrer"
      />

      <section v-else class="project-home-state">
        <p v-if="summaryLoadFailed" class="project-home-error" role="alert">
          {{ t('project.summaryLoadFailed') }}
        </p>
        <button
          :class="summaryLoadFailed ? 'secondary-button' : 'primary-button'"
          type="button"
          :disabled="summaryBusy || !workspaceAvailable"
          @click="summaryLoadFailed ? loadLatestSummary() : refreshSummary()"
        >
          <LoaderCircle v-if="summaryBusy" class="spin" :size="14" />
          <RefreshCw v-else :size="14" />
          {{ t(summaryLoadFailed ? 'project.summaryRetry' : 'project.summaryGenerate') }}
        </button>
      </section>
    </main>
  </div>
</template>

<script setup lang="ts">
import { GitBranch, LoaderCircle, MessageSquarePlus, PanelLeft, RefreshCw } from '@lucide/vue'
import { computed, ref, watch } from 'vue'
import { api } from '../api'
import { useAppI18n } from '../i18n'
import { PROJECT_HOME_CSP, PROJECT_HOME_SANDBOX } from '../projectHome'
import type { ProjectOverview } from '../types'

const props = defineProps<{
  overview: ProjectOverview
  summaryBusy: boolean
  workspaceAvailable: boolean
}>()

const emit = defineEmits<{
  'toggle-sidebar': []
  'new-session': []
  'run-workflow': []
  'run-summary': [input: { instructions: string; intervalMinutes: number }]
}>()

const { t } = useAppI18n()
const summaryInstructions = computed(
  () => props.overview.summary_session?.instructions ?? '',
)
const latestSummaryArtifact = computed(() => props.overview.project_home_artifact ?? undefined)
const summaryDocument = ref('')
const summaryLoading = ref(false)
const summaryLoadFailed = ref(false)
let summaryLoadGeneration = 0

watch(
  () => [props.overview.project.id, latestSummaryArtifact.value?.id] as const,
  () => void loadLatestSummary(),
  { immediate: true },
)

function refreshSummary() {
  emit('run-summary', {
    instructions: summaryInstructions.value,
    intervalMinutes: 0,
  })
}

async function loadLatestSummary() {
  const artifact = latestSummaryArtifact.value
  const generation = ++summaryLoadGeneration
  summaryLoadFailed.value = false
  if (!artifact) {
    summaryDocument.value = ''
    summaryLoading.value = false
    return
  }
  summaryLoading.value = true
  try {
    const source = await api.readArtifact(artifact)
    const document = source.trim()
    if (!document) throw new Error('Project summary is empty')
    if (generation === summaryLoadGeneration) summaryDocument.value = document
  } catch {
    if (generation === summaryLoadGeneration) {
      summaryDocument.value = ''
      summaryLoadFailed.value = true
    }
  } finally {
    if (generation === summaryLoadGeneration) summaryLoading.value = false
  }
}
</script>
