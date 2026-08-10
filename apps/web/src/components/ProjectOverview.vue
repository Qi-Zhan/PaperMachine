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

      <div v-else-if="summaryHtml" class="project-home-summary" v-html="summaryHtml" />

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
import DOMPurify from 'dompurify'
import { GitBranch, LoaderCircle, MessageSquarePlus, PanelLeft, RefreshCw } from '@lucide/vue'
import { computed, ref, watch } from 'vue'
import { api } from '../api'
import { useAppI18n } from '../i18n'
import type { ProjectOverview } from '../types'

const MAX_PROJECT_HOME_IMAGE_BYTES = 2 * 1024 * 1024

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
const defaultSummaryInstructions =
  'Keep the Project home page accurate, useful, and current. Prioritize evidence-backed conclusions, consequential decisions, unresolved contradictions, blockers, and concrete next actions.'
const summaryInstructions = computed(
  () => props.overview.summary_session?.instructions || defaultSummaryInstructions,
)
const latestSummaryArtifact = computed(() => props.overview.project_home_artifact ?? undefined)
const summaryHtml = ref('')
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
    summaryHtml.value = ''
    summaryLoading.value = false
    return
  }
  summaryLoading.value = true
  try {
    const source = await api.readArtifact(artifact)
    const html = sanitizeProjectHome(source)
    if (!html) throw new Error('Project summary is empty')
    if (generation === summaryLoadGeneration) summaryHtml.value = html
  } catch {
    if (generation === summaryLoadGeneration) {
      summaryHtml.value = ''
      summaryLoadFailed.value = true
    }
  } finally {
    if (generation === summaryLoadGeneration) summaryLoading.value = false
  }
}

function sanitizeProjectHome(source: string): string {
  const parsed = new DOMParser().parseFromString(source, 'text/html')
  const body = parsed.body.innerHTML.trim() || source
  const sanitized = DOMPurify.sanitize(body, {
    USE_PROFILES: { html: true },
    ADD_DATA_URI_TAGS: ['img'],
    ALLOW_DATA_ATTR: false,
    FORBID_ATTR: ['style', 'srcdoc'],
    FORBID_TAGS: [
      'audio',
      'base',
      'button',
      'canvas',
      'embed',
      'form',
      'iframe',
      'input',
      'link',
      'math',
      'meta',
      'object',
      'option',
      'script',
      'select',
      'style',
      'svg',
      'template',
      'textarea',
      'video',
    ],
  })
  const clean = new DOMParser().parseFromString(sanitized, 'text/html')
  clean.body.querySelectorAll('img').forEach((image) => {
    const src = image.getAttribute('src') ?? ''
    const match = /^data:image\/(?:png|jpeg|webp|gif);base64,([a-z0-9+/=]+)$/i.exec(src)
    const encoded = match?.[1] ?? ''
    const padding = encoded.endsWith('==') ? 2 : encoded.endsWith('=') ? 1 : 0
    const bytes = Math.floor((encoded.length * 3) / 4) - padding
    if (!match || bytes <= 0 || bytes > MAX_PROJECT_HOME_IMAGE_BYTES) {
      image.remove()
      return
    }
    image.removeAttribute('srcset')
    image.setAttribute('loading', 'lazy')
    image.setAttribute('decoding', 'async')
    if (!image.hasAttribute('alt')) image.setAttribute('alt', '')
  })
  clean.body.querySelectorAll('a[href]').forEach((anchor) => {
    const href = anchor.getAttribute('href') ?? ''
    if (/^https?:\/\//i.test(href)) {
      anchor.setAttribute('target', '_blank')
      anchor.setAttribute('rel', 'noopener noreferrer')
    } else if (!href.startsWith('#')) {
      anchor.removeAttribute('href')
    }
  })
  return clean.body.innerHTML.trim()
}
</script>
