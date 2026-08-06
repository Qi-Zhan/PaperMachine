<template>
  <Teleport to="body">
    <div v-if="open" class="dialog-backdrop" @mousedown.self="$emit('close')">
      <form class="dialog-panel" @submit.prevent="submit">
        <header class="dialog-header">
          <div>
            <p class="eyebrow">{{ project?.name }}</p>
            <h2>{{ t('dialog.newSession') }}</h2>
          </div>
          <button class="icon-button" type="button" :title="t('common.close')" :aria-label="t('common.close')" @click="$emit('close')">
            <X :size="17" />
          </button>
        </header>

        <p class="access-profile-description">{{ t('dialog.newSessionWorkflowHint') }}</p>

        <label class="field-label" for="session-title">{{ t('common.title') }}</label>
        <input
          id="session-title"
          ref="titleInput"
          v-model="title"
          class="text-input"
          autocomplete="off"
          :placeholder="t('dialog.newSessionPlaceholder')"
        />

        <label class="field-label" for="session-system-prompt">{{ t('dialog.sessionSystemPrompt') }}</label>
        <textarea
          id="session-system-prompt"
          v-model="systemPrompt"
          class="text-area text-area--small"
          :placeholder="t('dialog.sessionSystemPromptPlaceholder')"
        />

        <label class="field-label" for="session-model">{{ t('common.model') }}</label>
        <select
          v-if="modelProfiles.length"
          id="session-model"
          v-model="model"
          class="select-input"
        >
          <option value="">{{ t('dialog.serverDefault') }} — {{ defaultModel }}</option>
          <option v-for="profile in modelProfiles" :key="profile.id" :value="profile.id">
            {{ profile.id }} · {{ profile.provider }}/{{ profile.model }}
          </option>
        </select>
        <input
          v-else
          id="session-model"
          v-model="model"
          class="text-input"
          autocomplete="off"
          :placeholder="t('dialog.serverDefault')"
        />

        <label class="field-label" for="session-access">{{ t('dialog.sessionAccess') }}</label>
        <select id="session-access" v-model="access" class="select-input">
          <option v-for="profile in accessProfiles" :key="profile.value" :value="profile.value">
            {{ profile.label }}
          </option>
        </select>
        <p class="access-profile-description">{{ selectedAccessDescription }}</p>

        <fieldset v-if="skills.length" class="skill-picker">
          <legend>{{ t('project.skills') }}</legend>
          <label v-for="skill in skills" :key="skill.slug" class="check-row">
            <input v-model="enabledSkills" type="checkbox" :value="skill.slug" />
            <span>
              <strong>{{ skill.name }}</strong>
              <small>{{ skill.description }}</small>
            </span>
          </label>
        </fieldset>

        <p v-if="error" class="form-error">{{ error }}</p>
        <footer class="dialog-actions">
          <button class="text-button" type="button" @click="$emit('close')">{{ t('common.cancel') }}</button>
          <button class="primary-button" type="submit" :disabled="busy">
            <LoaderCircle v-if="busy" class="spin" :size="16" />
            <MessageSquarePlus v-else :size="16" />
            {{ t('dialog.createSession') }}
          </button>
        </footer>
      </form>
    </div>
  </Teleport>
</template>

<script setup lang="ts">
import { LoaderCircle, MessageSquarePlus, X } from '@lucide/vue'
import { computed, nextTick, ref, watch } from 'vue'
import { useAppI18n } from '../i18n'
import { AGENT_ACCESS_PROFILES } from '../types'
import type { AgentAccessProfile, CreateSessionInput, ModelProfile, Project, ProjectSkill } from '../types'

const props = defineProps<{
  open: boolean
  busy: boolean
  error?: string
  project: Project | null
  skills: ProjectSkill[]
  modelProfiles: ModelProfile[]
  defaultModel: string
}>()
const emit = defineEmits<{ close: []; submit: [input: CreateSessionInput] }>()

const title = ref('')
const { t } = useAppI18n()
const systemPrompt = ref('')
const model = ref('')
const access = ref<AgentAccessProfile>('research')
const enabledSkills = ref<string[]>([])
const titleInput = ref<HTMLInputElement | null>(null)
const accessProfiles = computed(() => AGENT_ACCESS_PROFILES.map((value) => ({
  value,
  label: accessLabel(value),
  description: accessDescription(value),
})))
const selectedAccessDescription = computed(
  () => accessProfiles.value.find((profile) => profile.value === access.value)?.description ?? '',
)

watch(
  () => props.open,
  async (open) => {
    if (!open) return
    title.value = ''
    systemPrompt.value = ''
    model.value = ''
    access.value = 'research'
    enabledSkills.value = []
    await nextTick()
    titleInput.value?.focus()
  },
)

function submit() {
  if (props.busy) return
  if (access.value === 'full_access' && !window.confirm(t('session.fullAccessConfirm'))) return
  emit('submit', {
    title: title.value.trim(),
    system_prompt: systemPrompt.value.trim(),
    model: model.value.trim(),
    enabled_skills: [...enabledSkills.value],
    access: access.value,
  })
}

function accessLabel(access: AgentAccessProfile): string {
  if (access === 'model_only') return t('access.modelOnly')
  if (access === 'read_only') return t('access.readOnly')
  if (access === 'workspace') return t('access.workspace')
  if (access === 'research') return t('access.research')
  return t('access.fullAccess')
}

function accessDescription(access: AgentAccessProfile): string {
  if (access === 'model_only') return t('access.modelOnlyDescription')
  if (access === 'read_only') return t('access.readOnlyDescription')
  if (access === 'workspace') return t('access.workspaceDescription')
  if (access === 'research') return t('access.researchDescription')
  return t('access.fullAccessDescription')
}
</script>
