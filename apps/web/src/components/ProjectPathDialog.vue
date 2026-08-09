<template>
  <Teleport to="body">
    <div v-if="open" class="dialog-backdrop" @mousedown.self="$emit('close')">
      <form class="dialog-panel dialog-panel--compact" @submit.prevent="submit">
        <header class="dialog-header">
          <div>
            <p class="eyebrow">Project</p>
            <h2>{{ t('dialog.relocateProject') }}</h2>
          </div>
          <button class="icon-button" type="button" :title="t('common.close')" :aria-label="t('common.close')" @click="$emit('close')">
            <X :size="17" />
          </button>
        </header>
        <p class="field-note project-path-intro">{{ t('dialog.relocateProjectDescription', { name: projectName ?? 'Project' }) }}</p>
        <label class="field-label" for="existing-project-root">{{ t('dialog.projectWorkspace') }}</label>
        <div class="workspace-path-picker">
          <input
            id="existing-project-root"
            ref="pathInput"
            v-model="rootPath"
            class="text-input code-input"
            autocomplete="off"
            :placeholder="t('dialog.projectWorkspacePlaceholder')"
            required
          />
          <button class="secondary-button" type="button" :disabled="busy || pickerBusy" @click="pickDirectory">
            <LoaderCircle v-if="pickerBusy" class="spin" :size="15" />
            <FolderOpen v-else :size="15" />
            {{ t('dialog.chooseWorkspace') }}
          </button>
        </div>
        <p v-if="pickerError || error" class="form-error">{{ pickerError || error }}</p>
        <footer class="dialog-actions">
          <button class="text-button" type="button" @click="$emit('close')">{{ t('common.cancel') }}</button>
          <button class="primary-button" type="submit" :disabled="busy || pickerBusy || !rootPath.trim()">
            <LoaderCircle v-if="busy" class="spin" :size="16" />
            <FolderOpen v-else :size="16" />
            {{ t('dialog.relocateProjectAction') }}
          </button>
        </footer>
      </form>
    </div>
  </Teleport>
</template>

<script setup lang="ts">
import { FolderOpen, LoaderCircle, X } from '@lucide/vue'
import { nextTick, ref, watch } from 'vue'
import { api } from '../api'
import { useAppI18n } from '../i18n'

const props = defineProps<{
  open: boolean
  busy: boolean
  error?: string
  projectName?: string
  initialPath?: string
}>()
const emit = defineEmits<{
  close: []
  submit: [rootPath: string]
}>()

const { t } = useAppI18n()
const rootPath = ref('')
const pickerBusy = ref(false)
const pickerError = ref('')
const pathInput = ref<HTMLInputElement | null>(null)

watch(
  () => props.open,
  async (open) => {
    if (!open) return
    rootPath.value = props.initialPath ?? ''
    pickerError.value = ''
    await nextTick()
    pathInput.value?.focus()
    pathInput.value?.select()
  },
)

function submit() {
  if (!rootPath.value.trim() || props.busy || pickerBusy.value) return
  emit('submit', rootPath.value.trim())
}

async function pickDirectory() {
  if (props.busy || pickerBusy.value) return
  pickerBusy.value = true
  pickerError.value = ''
  try {
    const selection = await api.pickWorkspaceDirectory()
    if (selection.path) rootPath.value = selection.path
  } catch (error) {
    pickerError.value = error instanceof Error ? error.message : String(error)
  } finally {
    pickerBusy.value = false
  }
}
</script>
