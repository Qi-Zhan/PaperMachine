<template>
  <Teleport to="body">
    <div v-if="open" class="dialog-backdrop" @mousedown.self="$emit('close')">
      <form class="dialog-panel dialog-panel--compact" @submit.prevent="submit">
        <header class="dialog-header">
          <div>
            <p class="eyebrow">Project</p>
            <h2>{{ t('dialog.newProject') }}</h2>
          </div>
          <button class="icon-button" type="button" :title="t('common.close')" :aria-label="t('common.close')" @click="$emit('close')">
            <X :size="17" />
          </button>
        </header>
        <label class="field-label" for="project-name">{{ t('common.name') }}</label>
        <input
          id="project-name"
          ref="nameInput"
          v-model="name"
          class="text-input"
          autocomplete="off"
          required
        />
        <label class="field-label" for="project-root">
          {{ t('dialog.projectWorkspace') }}
          <span class="field-optional">{{ t('common.optional') }}</span>
        </label>
        <div class="workspace-path-picker">
          <input
            id="project-root"
            v-model="workspacePath"
            class="text-input code-input"
            autocomplete="off"
            :placeholder="t('dialog.projectWorkspacePlaceholder')"
          />
          <button class="secondary-button" type="button" :disabled="busy || pickerBusy" @click="pickDirectory">
            <LoaderCircle v-if="pickerBusy" class="spin" :size="15" />
            <FolderOpen v-else :size="15" />
            {{ t('dialog.chooseWorkspace') }}
          </button>
        </div>
        <p class="field-note">{{ t('dialog.projectWorkspaceDefault') }}</p>
        <p v-if="pickerError || error" class="form-error">{{ pickerError || error }}</p>
        <footer class="dialog-actions">
          <button class="text-button" type="button" @click="$emit('close')">{{ t('common.cancel') }}</button>
          <button class="primary-button" type="submit" :disabled="busy || pickerBusy || !name.trim()">
            <LoaderCircle v-if="busy" class="spin" :size="16" />
            <FolderPlus v-else :size="16" />
            {{ t('dialog.createProject') }}
          </button>
        </footer>
      </form>
    </div>
  </Teleport>
</template>

<script setup lang="ts">
import { FolderOpen, FolderPlus, LoaderCircle, X } from '@lucide/vue'
import { nextTick, ref, watch } from 'vue'
import { api } from '../api'
import { useAppI18n } from '../i18n'

const props = defineProps<{ open: boolean; busy: boolean; error?: string }>()
const emit = defineEmits<{
  close: []
  submit: [input: { name: string; workspacePath: string }]
}>()

const name = ref('')
const { t } = useAppI18n()
const workspacePath = ref('')
const pickerBusy = ref(false)
const pickerError = ref('')
const nameInput = ref<HTMLInputElement | null>(null)

watch(
  () => props.open,
  async (open) => {
    if (!open) return
    name.value = ''
    workspacePath.value = ''
    pickerError.value = ''
    await nextTick()
    nameInput.value?.focus()
  },
)

function submit() {
  if (!name.value.trim() || props.busy || pickerBusy.value) return
  emit('submit', {
    name: name.value.trim(),
    workspacePath: workspacePath.value.trim(),
  })
}

async function pickDirectory() {
  if (props.busy || pickerBusy.value) return
  pickerBusy.value = true
  pickerError.value = ''
  try {
    const selection = await api.pickWorkspaceDirectory()
    if (selection.path) workspacePath.value = selection.path
  } catch (error) {
    pickerError.value = error instanceof Error ? error.message : String(error)
  } finally {
    pickerBusy.value = false
  }
}
</script>
