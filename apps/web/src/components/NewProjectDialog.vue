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
        <label class="field-label" for="project-description">{{ t('common.description') }}</label>
        <textarea id="project-description" v-model="description" class="text-area text-area--small" />
        <label class="field-label" for="project-root">{{ t('dialog.projectRoot') }}</label>
        <input
          id="project-root"
          v-model="rootPath"
          class="text-input code-input"
          autocomplete="off"
          :placeholder="t('dialog.projectRootPlaceholder')"
          required
        />
        <p class="field-note">{{ t('dialog.projectRootHelp') }}</p>
        <p v-if="error" class="form-error">{{ error }}</p>
        <footer class="dialog-actions">
          <button class="text-button" type="button" @click="$emit('close')">{{ t('common.cancel') }}</button>
          <button class="primary-button" type="submit" :disabled="busy || !name.trim() || !rootPath.trim()">
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
import { FolderPlus, LoaderCircle, X } from '@lucide/vue'
import { nextTick, ref, watch } from 'vue'
import { useAppI18n } from '../i18n'

const props = defineProps<{ open: boolean; busy: boolean; error?: string }>()
const emit = defineEmits<{
  close: []
  submit: [input: { name: string; description: string; rootPath: string }]
}>()

const name = ref('')
const { t } = useAppI18n()
const description = ref('')
const rootPath = ref('')
const nameInput = ref<HTMLInputElement | null>(null)

watch(
  () => props.open,
  async (open) => {
    if (!open) return
    name.value = ''
    description.value = ''
    rootPath.value = ''
    await nextTick()
    nameInput.value?.focus()
  },
)

function submit() {
  if (!name.value.trim() || !rootPath.value.trim() || props.busy) return
  emit('submit', {
    name: name.value.trim(),
    description: description.value.trim(),
    rootPath: rootPath.value.trim(),
  })
}
</script>
