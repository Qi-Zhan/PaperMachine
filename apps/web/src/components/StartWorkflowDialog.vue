<template>
  <Teleport to="body">
    <div v-if="open" class="dialog-backdrop" @mousedown.self="$emit('close')">
      <form class="dialog-panel workflow-launch-dialog" @submit.prevent="submit">
        <header class="dialog-header">
          <div>
            <p class="eyebrow">{{ originLabel }}</p>
            <h2>{{ t('dialog.startWorkflow') }}</h2>
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

        <label class="field-label" for="workflow-select">Workflow</label>
        <select id="workflow-select" v-model="workflowKey" class="select-input">
          <option v-for="workflow in workflows" :key="keyOf(workflow)" :value="keyOf(workflow)">
            {{ workflow.manifest.name }} · {{ workflow.manifest.slug }}
          </option>
        </select>
        <p v-if="selectedWorkflow" class="field-note">{{ selectedWorkflow.manifest.description }}</p>

        <template v-if="requestMode === 'required'">
          <label class="field-label" for="workflow-request">{{ t('dialog.request') }}</label>
          <textarea
            id="workflow-request"
            ref="requestInput"
            v-model="requestText"
            class="text-area text-area--small"
            required
          />
        </template>
        <p v-else class="field-note workflow-prompt-note">{{ t('dialog.workflowNoUserTask') }}</p>

        <label class="field-label" for="workflow-instructions">{{ t('dialog.workflowInstructions') }}</label>
        <textarea
          id="workflow-instructions"
          v-model="instructions"
          class="text-area text-area--small"
          :placeholder="t('dialog.workflowInstructionsPlaceholder')"
        />
        <p class="field-note workflow-prompt-note">
          {{ t(requestMode === 'none' ? 'dialog.workflowPromptStackInteractive' : session ? 'dialog.workflowPromptStackFromSession' : 'dialog.workflowPromptStack') }}
        </p>

        <section v-if="schemaFields.length" class="workflow-launch-section">
          <header class="workflow-launch-section-heading">
            <div>
              <h3>{{ t('workflow.params') }}</h3>
              <p>{{ t('dialog.workflowParamsDescription') }}</p>
            </div>
          </header>
          <div class="schema-fields">
            <template v-for="field in visibleFields" :key="field.key">
              <label v-if="field.type === 'boolean'" class="check-row schema-check-row">
                <input
                  :checked="Boolean(formValues[field.key])"
                  type="checkbox"
                  @change="setBooleanValue(field.key, $event)"
                />
                <span>
                  <strong>{{ field.label }}</strong>
                  <small>{{ field.description }}</small>
                </span>
              </label>
              <template v-else>
                <label class="field-label" :for="`workflow-field-${field.key}`">{{ field.label }}</label>
                <div v-if="field.type === 'integer'" class="stepper schema-stepper">
                  <button type="button" :title="t('common.decrease')" :aria-label="t('common.decrease')" @click="decrement(field)">
                    <Minus :size="14" />
                  </button>
                  <output>{{ Number(formValues[field.key] ?? 0) }}</output>
                  <button type="button" :title="t('common.increase')" :aria-label="t('common.increase')" @click="increment(field)">
                    <Plus :size="14" />
                  </button>
                </div>
                <select
                  v-else-if="field.modelProfile && modelProfiles.length"
                  :id="`workflow-field-${field.key}`"
                  :value="String(formValues[field.key] ?? '')"
                  class="select-input schema-control"
                  @change="setTextValue(field.key, $event)"
                >
                  <option value="">{{ t('dialog.inheritRunModel') }}</option>
                  <option v-for="profile in modelProfiles" :key="profile.id" :value="profile.id">
                    {{ profile.id }} · {{ profile.provider }}/{{ profile.model }}
                  </option>
                </select>
                <textarea
                  v-else-if="field.multiline || field.type === 'object' || field.type === 'array'"
                  :id="`workflow-field-${field.key}`"
                  :value="String(formValues[field.key] ?? '')"
                  class="text-area schema-text-area"
                  @input="setTextValue(field.key, $event)"
                />
                <select
                  v-else-if="field.options"
                  :id="`workflow-field-${field.key}`"
                  :value="String(formValues[field.key] ?? '')"
                  class="select-input schema-control"
                  @change="setTextValue(field.key, $event)"
                >
                  <option value="">{{ t('common.default') }}</option>
                  <option v-for="option in field.options" :key="option" :value="option">{{ option }}</option>
                </select>
                <input
                  v-else-if="field.type === 'number'"
                  :id="`workflow-field-${field.key}`"
                  :value="Number(formValues[field.key] ?? field.minimum ?? 0)"
                  class="text-input schema-control"
                  type="number"
                  :min="field.minimum"
                  :max="field.maximum"
                  step="any"
                  @input="setNumberValue(field.key, $event)"
                />
                <input
                  v-else
                  :id="`workflow-field-${field.key}`"
                  :value="String(formValues[field.key] ?? '')"
                  class="text-input schema-control"
                  autocomplete="off"
                  @input="setTextValue(field.key, $event)"
                />
                <p v-if="field.description" class="field-note schema-field-note">{{ field.description }}</p>
              </template>
            </template>
          </div>

          <button
            v-if="advancedFields.length"
            class="advanced-toggle"
            type="button"
            :aria-expanded="advancedVisible"
            @click="advancedVisible = !advancedVisible"
          >
            <SlidersHorizontal :size="14" />
            {{ t('common.advanced') }}
            <ChevronDown :class="{ 'is-open': advancedVisible }" :size="14" />
          </button>
        </section>

        <section class="workflow-launch-section workflow-run-configuration">
          <header class="workflow-launch-section-heading">
            <div>
              <h3>{{ t('dialog.runConfiguration') }}</h3>
              <p>{{ t('dialog.runConfigurationDescription') }}</p>
            </div>
            <LoaderCircle v-if="programLoading" class="spin" :size="15" />
          </header>

          <div class="workflow-context-options">
            <label :data-active="contextMode === 'project_snapshot'">
              <input v-model="contextMode" type="radio" value="project_snapshot" />
              <span>
                <strong>{{ t('dialog.existingProjectContext') }}</strong>
                <small>{{ t(session ? 'dialog.existingSessionContextDescription' : 'dialog.existingProjectContextDescription') }}</small>
              </span>
            </label>
            <label :data-active="contextMode === 'fresh'">
              <input v-model="contextMode" type="radio" value="fresh" />
              <span>
                <strong>{{ t('dialog.freshContext') }}</strong>
                <small>{{ t('dialog.freshContextDescription') }}</small>
              </span>
            </label>
          </div>
          <p class="workflow-context-note">
            {{ contextMode === 'project_snapshot' ? t('dialog.contextSnapshotNote') : t('dialog.freshContextNote') }}
          </p>

          <div class="workflow-launch-grid">
            <label>
              <span class="field-label">{{ t('common.model') }}</span>
              <select
                v-if="modelProfiles.length"
                v-model="model"
                class="select-input"
                :aria-label="t('common.model')"
              >
                <option value="">{{ t('dialog.serverDefault') }} — {{ defaultModel }}</option>
                <option v-if="unknownSelectedModel" :value="model">{{ model }}</option>
                <option v-for="profile in modelProfiles" :key="profile.id" :value="profile.id">
                  {{ profile.id }} · {{ profile.provider }}/{{ profile.model }}
                </option>
              </select>
              <input
                v-else
                v-model="model"
                class="text-input"
                autocomplete="off"
                :aria-label="t('common.model')"
                :placeholder="t('dialog.serverDefault')"
              />
            </label>
            <label>
              <span class="field-label">{{ t('dialog.permissionCeiling') }}</span>
              <select
                v-model="access"
                class="select-input"
                :aria-label="t('dialog.permissionCeiling')"
              >
                <option v-for="profile in ceilingProfiles" :key="profile" :value="profile">
                  {{ accessLabel(profile) }}
                </option>
              </select>
            </label>
          </div>
          <p class="access-profile-description">
            {{ accessDescription(access) }}
            <template v-if="session"> {{ t('dialog.originPermissionCeiling', { access: accessLabel(session.access) }) }}</template>
          </p>

          <div v-if="validation?.agents.length" class="workflow-agent-access">
            <div class="workflow-agent-access-heading">
              <div>
                <strong>{{ t('dialog.agentPermissions') }}</strong>
                <small>{{ t('dialog.agentPermissionsDescription') }}</small>
              </div>
              <span>{{ validation.agents.length }}</span>
            </div>
            <div v-for="agent in validation.agents" :key="agent.class_name" class="workflow-agent-access-row">
              <div>
                <strong>{{ agent.class_name }}</strong>
                <small>
                  {{ agent.actions.length ? agent.actions.join(' · ') : t('workflow.noActions') }}
                </small>
              </div>
              <select
                v-model="agentOverrides[agent.class_name]"
                class="select-input"
                :aria-label="t('dialog.agentPermissionAria', { agent: agent.class_name })"
              >
                <option value="">{{ declaredAccessLabel(agent.access) }}</option>
                <option v-for="profile in agentOverrideProfiles" :key="profile" :value="profile">
                  {{ accessLabel(profile) }}
                </option>
              </select>
              <span class="workflow-effective-access">
                {{ t('dialog.effectiveAccess', { access: accessLabel(effectiveAgentAccess(agent)) }) }}
              </span>
            </div>
          </div>

          <fieldset v-if="skills.length" class="skill-picker workflow-run-skills">
            <legend>{{ t('dialog.workflowSkills') }}</legend>
            <p>{{ t('dialog.workflowSkillsDescription') }}</p>
            <label v-for="skill in skills" :key="skill.slug" class="check-row">
              <input v-model="enabledSkills" type="checkbox" :value="skill.slug" />
              <span>
                <strong>{{ skill.name }}</strong>
                <small>{{ skill.description || skill.slug }}</small>
              </span>
            </label>
          </fieldset>
        </section>

        <p v-if="displayError" class="form-error">{{ displayError }}</p>
        <footer class="dialog-actions">
          <button class="text-button" type="button" @click="$emit('close')">{{ t('common.cancel') }}</button>
          <button class="primary-button" type="submit" :disabled="busy || !canSubmit">
            <LoaderCircle v-if="busy" class="spin" :size="16" />
            <Play v-else :size="15" fill="currentColor" />
            {{ t('dialog.start') }}
          </button>
        </footer>
      </form>
    </div>
  </Teleport>
</template>

<script setup lang="ts">
import { ChevronDown, LoaderCircle, Minus, Play, Plus, SlidersHorizontal, X } from '@lucide/vue'
import { computed, nextTick, reactive, ref, watch } from 'vue'
import { api } from '../api'
import { useAppI18n } from '../i18n'
import { ACCESS_PRESETS } from '../types'
import type {
  AccessPreset,
  ModelProfile,
  Project,
  ProjectSkill,
  Session,
  WorkflowAgentDeclaration,
  WorkflowContextMode,
  WorkflowProgram,
  WorkflowValidation,
} from '../types'

const props = defineProps<{
  open: boolean
  busy: boolean
  error?: string
  project: Project | null
  session: Session | null
  workflows: WorkflowProgram[]
  skills: ProjectSkill[]
  modelProfiles: ModelProfile[]
  defaultModel: string
}>()
const emit = defineEmits<{
  close: []
  submit: [input: {
    workflow: WorkflowProgram
    request: string
    instructions: string
    params: Record<string, unknown>
    contextMode: WorkflowContextMode
    model: string
    access: AccessPreset
    enabledSkills: string[]
    agentAccessOverrides: Record<string, AccessPreset>
  }]
}>()

type FormValue = string | number | boolean
interface SchemaProperty {
  type?: string
  title?: string
  description?: string
  default?: unknown
  minimum?: number
  maximum?: number
  enum?: string[]
  format?: string
  'x-ui-order'?: number
}
interface SchemaField {
  key: string
  label: string
  description?: string
  type: string
  minimum?: number
  maximum?: number
  defaultValue?: unknown
  options?: string[]
  modelProfile: boolean
  multiline: boolean
  advanced: boolean
  order: number
}

const workflowKey = ref('')
const { t } = useAppI18n()
const requestText = ref('')
const instructions = ref('')
const formValues = ref<Record<string, FormValue>>({})
const advancedVisible = ref(false)
const localError = ref('')
const programError = ref('')
const programLoading = ref(false)
const validation = ref<WorkflowValidation | null>(null)
const requestInput = ref<HTMLTextAreaElement | null>(null)
const contextMode = ref<WorkflowContextMode>('project_snapshot')
const model = ref('')
const access = ref<AccessPreset>('research')
const enabledSkills = ref<string[]>([])
const agentOverrides = reactive<Record<string, AccessPreset | ''>>({})
let programRequest = 0

const keyOf = (workflow: WorkflowProgram) => workflow.manifest.slug
const selectedWorkflow = computed(() => props.workflows.find((workflow) => keyOf(workflow) === workflowKey.value))
const requestMode = computed(() => selectedWorkflow.value?.manifest.request_mode ?? 'required')
const originLabel = computed(() => {
  if (!props.project) return 'Project'
  return props.session ? `${props.project.name} · ${props.session.title}` : props.project.name
})
const originAccessIndex = computed(() =>
  props.session ? ACCESS_PRESETS.indexOf(props.session.access) : ACCESS_PRESETS.length - 1,
)
const ceilingProfiles = computed(() => ACCESS_PRESETS.slice(0, originAccessIndex.value + 1))
const agentOverrideProfiles = computed(() => {
  const ceilingIndex = ACCESS_PRESETS.indexOf(access.value)
  return ACCESS_PRESETS.slice(0, ceilingIndex + 1)
})
const unknownSelectedModel = computed(
  () => Boolean(model.value && !props.modelProfiles.some((profile) => profile.id === model.value)),
)
const schemaFields = computed<SchemaField[]>(() => {
  const properties = (selectedWorkflow.value?.manifest.params_schema as { properties?: unknown } | undefined)?.properties
  if (!properties || typeof properties !== 'object' || Array.isArray(properties)) return []
  return Object.entries(properties as Record<string, unknown>)
    .map(([key, raw]) => {
      const property = (raw ?? {}) as SchemaProperty
      return {
        key,
        label: property.title ?? humanize(key),
        description: property.description,
        type: property.type ?? 'string',
        minimum: property.minimum,
        maximum: property.maximum,
        defaultValue: property.default,
        options: property.enum,
        modelProfile: property.format === 'model-profile',
        multiline: key.includes('claim') || key.includes('result') || key.includes('description'),
        advanced: property.format === 'model-profile' || key === 'model',
        order: property['x-ui-order'] ?? Number.MAX_SAFE_INTEGER,
      }
    })
    .sort((left, right) => left.order - right.order || left.label.localeCompare(right.label))
})
const advancedFields = computed(() => schemaFields.value.filter((field) => field.advanced))
const visibleFields = computed(() => schemaFields.value.filter((field) => !field.advanced || advancedVisible.value))
const canSubmit = computed(() => Boolean(
  props.project &&
  selectedWorkflow.value &&
  (requestMode.value === 'none' || requestText.value.trim()) &&
  !programLoading.value &&
  !programError.value,
))
const displayError = computed(() => localError.value || programError.value || props.error)

watch(
  () => props.open,
  async (open) => {
    if (!open) return
    workflowKey.value = props.workflows[0] ? keyOf(props.workflows[0]) : ''
    requestText.value = ''
    instructions.value = ''
    advancedVisible.value = false
    localError.value = ''
    programError.value = ''
    contextMode.value = 'project_snapshot'
    model.value = props.session?.model ?? ''
    access.value = props.session?.access ?? 'research'
    enabledSkills.value = [...(props.session?.enabled_skills ?? [])]
    initializeValues()
    resetAgentOverrides()
    await loadSelectedProgram()
    await nextTick()
    requestInput.value?.focus()
  },
)
watch(workflowKey, async () => {
  if (!props.open) return
  advancedVisible.value = false
  localError.value = ''
  initializeValues()
  resetAgentOverrides()
  await loadSelectedProgram()
})
watch(access, (next) => {
  const ceilingIndex = ACCESS_PRESETS.indexOf(next)
  for (const [className, value] of Object.entries(agentOverrides)) {
    if (value && ACCESS_PRESETS.indexOf(value) > ceilingIndex) agentOverrides[className] = ''
  }
})

async function loadSelectedProgram() {
  const projectId = props.project?.id
  const slug = selectedWorkflow.value?.manifest.slug
  const request = ++programRequest
  validation.value = null
  programError.value = ''
  if (!projectId || !slug) return
  programLoading.value = true
  try {
    const loaded = await api.getWorkflowProgram(projectId, slug)
    if (request !== programRequest) return
    validation.value = loaded.validation
    resetAgentOverrides()
  } catch (error) {
    if (request === programRequest) {
      programError.value = error instanceof Error ? error.message : String(error)
    }
  } finally {
    if (request === programRequest) programLoading.value = false
  }
}

function initializeValues() {
  const values: Record<string, FormValue> = {}
  for (const field of schemaFields.value) {
    if (field.defaultValue !== undefined) {
      if (field.type === 'object' || field.type === 'array') values[field.key] = JSON.stringify(field.defaultValue, null, 2)
      else if (['string', 'number', 'boolean'].includes(typeof field.defaultValue)) {
        values[field.key] = field.defaultValue as FormValue
      } else values[field.key] = ''
    } else if (field.type === 'boolean') values[field.key] = false
    else if (field.type === 'integer' || field.type === 'number') values[field.key] = field.minimum ?? 0
    else values[field.key] = ''
  }
  formValues.value = values
}

function resetAgentOverrides() {
  for (const key of Object.keys(agentOverrides)) delete agentOverrides[key]
  for (const agent of validation.value?.agents ?? []) agentOverrides[agent.class_name] = ''
}

function submit() {
  if (!selectedWorkflow.value || props.busy || !canSubmit.value) return
  const params: Record<string, unknown> = {}
  for (const field of schemaFields.value) {
    const value = formValues.value[field.key]
    if (value === '' || value === undefined || value === null) continue
    if (field.type === 'object' || field.type === 'array') {
      try {
        params[field.key] = typeof value === 'string' ? JSON.parse(value) : value
      } catch {
        localError.value = t('dialog.invalidJson', { field: field.label })
        return
      }
    } else params[field.key] = value
  }
  if (access.value === 'full_access' && !window.confirm(t('dialog.workflowFullAccessConfirm'))) return
  const agentAccessOverrides = Object.fromEntries(
    Object.entries(agentOverrides).filter((entry): entry is [string, AccessPreset] => Boolean(entry[1])),
  )
  localError.value = ''
  emit('submit', {
    workflow: selectedWorkflow.value,
    request: requestMode.value === 'required' ? requestText.value.trim() : '',
    instructions: instructions.value.trim(),
    params,
    contextMode: contextMode.value,
    model: model.value.trim(),
    access: access.value,
    enabledSkills: [...enabledSkills.value],
    agentAccessOverrides,
  })
}

function effectiveAgentAccess(agent: WorkflowAgentDeclaration): AccessPreset {
  const requested = agentOverrides[agent.class_name] || agent.access
  const requestedIndex = ACCESS_PRESETS.indexOf(requested)
  const ceilingIndex = ACCESS_PRESETS.indexOf(access.value)
  return ACCESS_PRESETS[Math.min(requestedIndex, ceilingIndex)]
}

function declaredAccessLabel(declared: AccessPreset): string {
  const declaredIndex = ACCESS_PRESETS.indexOf(declared)
  const ceilingIndex = ACCESS_PRESETS.indexOf(access.value)
  if (declaredIndex > ceilingIndex) {
    return t('dialog.declaredAccessCapped', {
      declared: accessLabel(declared),
      effective: accessLabel(access.value),
    })
  }
  return t('dialog.declaredAccess', { access: accessLabel(declared) })
}

function decrement(field: SchemaField) {
  const current = Number(formValues.value[field.key] ?? field.minimum ?? 0)
  formValues.value[field.key] = Math.max(field.minimum ?? Number.MIN_SAFE_INTEGER, current - 1)
}
function increment(field: SchemaField) {
  const current = Number(formValues.value[field.key] ?? field.minimum ?? 0)
  formValues.value[field.key] = Math.min(field.maximum ?? Number.MAX_SAFE_INTEGER, current + 1)
}
function setTextValue(key: string, event: Event) {
  formValues.value[key] = (event.target as HTMLInputElement | HTMLTextAreaElement | HTMLSelectElement).value
}
function setNumberValue(key: string, event: Event) {
  const value = (event.target as HTMLInputElement).valueAsNumber
  formValues.value[key] = Number.isNaN(value) ? '' : value
}
function setBooleanValue(key: string, event: Event) {
  formValues.value[key] = (event.target as HTMLInputElement).checked
}
function humanize(value: string): string {
  const text = value.replaceAll('_', ' ')
  return text.charAt(0).toUpperCase() + text.slice(1)
}
function accessLabel(value: AccessPreset): string {
  if (value === 'model_only') return t('access.modelOnly')
  if (value === 'read_only') return t('access.readOnly')
  if (value === 'workspace') return t('access.workspace')
  if (value === 'research') return t('access.research')
  return t('access.fullAccess')
}
function accessDescription(value: AccessPreset): string {
  if (value === 'model_only') return t('access.modelOnlyDescription')
  if (value === 'read_only') return t('access.readOnlyDescription')
  if (value === 'workspace') return t('access.workspaceDescription')
  if (value === 'research') return t('access.researchDescription')
  return t('access.fullAccessDescription')
}
</script>
