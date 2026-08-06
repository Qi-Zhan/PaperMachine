<template>
  <Teleport to="body">
    <div v-if="run" class="artifact-backdrop" @mousedown.self="$emit('close')">
      <section class="artifact-dialog workflow-output-dialog" role="dialog" aria-modal="true">
        <header class="artifact-dialog-header">
          <div>
            <p class="eyebrow">{{ run.workflow.manifest.name }} · {{ run.status }}</p>
            <h2>{{ t('workflow.output') }}</h2>
          </div>
          <button
            class="icon-button"
            type="button"
            :title="t('common.close')"
            :aria-label="t('common.close')"
            @click="$emit('close')"
          >
            <X :size="17" />
          </button>
        </header>
        <nav v-if="hasReport && hasStructuredData" class="workflow-output-tabs" role="tablist">
          <button
            type="button"
            role="tab"
            :aria-selected="view === 'report'"
            :data-active="view === 'report'"
            @click="view = 'report'"
          >
            <FileText :size="14" /> {{ t('workflow.report') }}
          </button>
          <button
            type="button"
            role="tab"
            :aria-selected="view === 'data'"
            :data-active="view === 'data'"
            @click="view = 'data'"
          >
            <Braces :size="14" /> {{ t('workflow.structuredOutput') }}
          </button>
        </nav>
        <div class="artifact-body workflow-output-body">
          <MarkdownView v-if="view === 'report' && report !== null" :source="report" />
          <pre v-else class="workflow-output-json">{{ formattedOutput }}</pre>
        </div>
      </section>
    </div>
  </Teleport>
</template>

<script setup lang="ts">
import { Braces, FileText, X } from '@lucide/vue'
import { computed, ref, watch } from 'vue'
import { useAppI18n } from '../i18n'
import type { WorkflowRun } from '../types'
import MarkdownView from './MarkdownView.vue'

const props = defineProps<{ run: WorkflowRun | null }>()
defineEmits<{ close: [] }>()

const { t } = useAppI18n()
const view = ref<'report' | 'data'>('data')
const report = computed<string | null>(() => {
  const output = props.run?.output
  if (typeof output === 'string') return output
  if (isRecord(output) && typeof output.report === 'string') return output.report
  return null
})
const hasReport = computed(() => report.value !== null)
const hasStructuredData = computed(() => {
  const output = props.run?.output
  return typeof output !== 'string' && output !== null && output !== undefined
})
const formattedOutput = computed(() => {
  try {
    return JSON.stringify(props.run?.output ?? null, null, 2)
  } catch {
    return String(props.run?.output ?? '')
  }
})

watch(
  () => props.run,
  () => {
    view.value = report.value !== null ? 'report' : 'data'
  },
  { immediate: true },
)

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}
</script>
