<template>
  <Teleport to="body">
    <div v-if="open" class="dialog-backdrop" @mousedown.self="$emit('close')">
      <form class="dialog-panel" @submit.prevent="submit">
        <header class="dialog-header">
          <div>
            <p class="eyebrow">{{ researchName }}</p>
            <h2>{{ t('dialog.newSkill') }}</h2>
          </div>
          <button class="icon-button" type="button" :title="t('common.close')" :aria-label="t('common.close')" @click="$emit('close')">
            <X :size="17" />
          </button>
        </header>
        <div class="field-grid">
          <label>
            <span class="field-label">{{ t('common.name') }}</span>
            <input ref="nameInput" v-model="name" class="text-input" autocomplete="off" required />
          </label>
          <label>
            <span class="field-label">Slug</span>
            <input v-model="slug" class="text-input code-input" autocomplete="off" required />
          </label>
        </div>
        <label class="field-label" for="skill-description">{{ t('common.description') }}</label>
        <input id="skill-description" v-model="description" class="text-input" autocomplete="off" />
        <label class="field-label" for="skill-instructions">{{ t('common.instructions') }}</label>
        <textarea id="skill-instructions" v-model="instructions" class="text-area" required />
        <p v-if="error" class="form-error">{{ error }}</p>
        <footer class="dialog-actions">
          <button class="text-button" type="button" @click="$emit('close')">{{ t('common.cancel') }}</button>
          <button class="primary-button" type="submit" :disabled="busy || !canSubmit">
            <LoaderCircle v-if="busy" class="spin" :size="16" />
            <FilePlus2 v-else :size="16" />
            {{ t('dialog.createSkill') }}
          </button>
        </footer>
      </form>
    </div>
  </Teleport>
</template>

<script setup lang="ts">
import { FilePlus2, LoaderCircle, X } from '@lucide/vue'
import { computed, nextTick, ref, watch } from 'vue'
import { useAppI18n } from '../i18n'

const props = defineProps<{
  open: boolean
  busy: boolean
  error?: string
  researchName: string
}>()
const emit = defineEmits<{
  close: []
  submit: [input: { slug: string; name: string; description: string; instructions: string }]
}>()

const name = ref('')
const { t } = useAppI18n()
const slug = ref('')
const description = ref('')
const instructions = ref('')
const nameInput = ref<HTMLInputElement | null>(null)
const canSubmit = computed(() => Boolean(name.value.trim() && slug.value.trim() && instructions.value.trim()))

watch(name, (value) => {
  if (!slug.value || slug.value === slugify(name.value.slice(0, -1))) slug.value = slugify(value)
})

watch(
  () => props.open,
  async (open) => {
    if (!open) return
    name.value = ''
    slug.value = ''
    description.value = ''
    instructions.value = ''
    await nextTick()
    nameInput.value?.focus()
  },
)

function submit() {
  if (!canSubmit.value || props.busy) return
  emit('submit', {
    slug: slug.value.trim(),
    name: name.value.trim(),
    description: description.value.trim(),
    instructions: instructions.value.trim(),
  })
}

function slugify(value: string): string {
  return value
    .trim()
    .toLocaleLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-|-$/g, '')
}
</script>
