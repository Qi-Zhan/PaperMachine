<template>
  <Teleport to="body">
    <div v-if="artifact" class="artifact-backdrop" @mousedown.self="$emit('close')">
      <section class="artifact-dialog">
        <header class="artifact-dialog-header">
          <div>
            <p class="eyebrow">{{ artifact.kind }} · {{ formatBytes(artifact.size_bytes) }}</p>
            <h2>{{ artifact.name }}</h2>
          </div>
          <div class="toolbar-actions">
            <a class="icon-button" :href="api.artifactUrl(artifact)" target="_blank" rel="noreferrer" :title="t('artifact.openRaw')" :aria-label="t('artifact.openRaw')">
              <ExternalLink :size="16" />
            </a>
            <button class="icon-button" type="button" :title="t('common.close')" :aria-label="t('common.close')" @click="$emit('close')">
              <X :size="17" />
            </button>
          </div>
        </header>
        <div class="artifact-body">
          <div v-if="loading" class="artifact-loading"><LoaderCircle class="spin" :size="20" /></div>
          <p v-else-if="error" class="form-error">{{ error }}</p>
          <MarkdownView v-else :source="content" />
        </div>
      </section>
    </div>
  </Teleport>
</template>

<script setup lang="ts">
import { ExternalLink, LoaderCircle, X } from '@lucide/vue'
import { ref, watch } from 'vue'
import { api } from '../api'
import { useAppI18n } from '../i18n'
import type { Artifact } from '../types'
import MarkdownView from './MarkdownView.vue'

const props = defineProps<{ artifact: Artifact | null }>()
defineEmits<{ close: [] }>()

const content = ref('')
const { t } = useAppI18n()
const error = ref('')
const loading = ref(false)

watch(
  () => props.artifact,
  async (artifact) => {
    content.value = ''
    error.value = ''
    if (!artifact) return
    loading.value = true
    try {
      content.value = await api.readArtifact(artifact)
    } catch (value) {
      error.value = value instanceof Error ? value.message : String(value)
    } finally {
      loading.value = false
    }
  },
  { immediate: true },
)

function formatBytes(value: number): string {
  if (value < 1024) return `${value} B`
  return `${(value / 1024).toFixed(1)} KB`
}
</script>
