<template>
  <Teleport to="body">
    <div v-if="open" class="dialog-backdrop" @mousedown.self="$emit('close')">
      <form class="dialog-panel" @submit.prevent="submit">
        <header class="dialog-header">
          <div>
            <p class="eyebrow">{{ session?.title }}</p>
            <h2>{{ t('dialog.startWorkflow') }}</h2>
          </div>
          <button class="icon-button" type="button" :title="t('common.close')" :aria-label="t('common.close')" @click="$emit('close')">
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

        <label class="field-label" for="workflow-objective">{{ t('dialog.objective') }}</label>
        <textarea
          id="workflow-objective"
          ref="objectiveInput"
          v-model="objective"
          class="text-area text-area--small"
          required
        />

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
                class="select-input"
                @change="setTextValue(field.key, $event)"
              >
                <option value="">{{ t('common.default') }}</option>
                <option v-for="option in field.options" :key="option" :value="option">{{ option }}</option>
              </select>
              <input
                v-else
                :id="`workflow-field-${field.key}`"
                :value="String(formValues[field.key] ?? '')"
                class="text-input"
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
import { computed, nextTick, ref, watch } from 'vue'
import { useAppI18n } from '../i18n'
import type { Session, WorkflowProgram } from '../types'

const props = defineProps<{
  open: boolean
  busy: boolean
  error?: string
  session: Session | null
  workflows: WorkflowProgram[]
}>()
const emit = defineEmits<{
  close: []
  submit: [input: { workflow: WorkflowProgram; objective: string; input: Record<string, unknown> }]
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
  multiline: boolean
  advanced: boolean
  order: number
}

const workflowKey = ref('')
const { t } = useAppI18n()
const objective = ref('')
const formValues = ref<Record<string, FormValue>>({})
const advancedVisible = ref(false)
const localError = ref('')
const objectiveInput = ref<HTMLTextAreaElement | null>(null)

const keyOf = (workflow: WorkflowProgram) => workflow.manifest.slug
const selectedWorkflow = computed(() => props.workflows.find((workflow) => keyOf(workflow) === workflowKey.value))
const schemaFields = computed<SchemaField[]>(() => {
  const properties = (selectedWorkflow.value?.manifest.input_schema as { properties?: unknown } | undefined)?.properties
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
        multiline: key.includes('claim') || key.includes('result') || key.includes('description'),
        advanced: key === 'model',
        order: property['x-ui-order'] ?? Number.MAX_SAFE_INTEGER,
      }
    })
    .sort((left, right) => left.order - right.order || left.label.localeCompare(right.label))
})
const advancedFields = computed(() => schemaFields.value.filter((field) => field.advanced))
const visibleFields = computed(() => schemaFields.value.filter((field) => !field.advanced || advancedVisible.value))
const canSubmit = computed(() => Boolean(selectedWorkflow.value && objective.value.trim()))
const displayError = computed(() => localError.value || props.error)

watch(
  () => props.open,
  async (open) => {
    if (!open) return
    workflowKey.value = props.workflows[0] ? keyOf(props.workflows[0]) : ''
    objective.value = ''
    advancedVisible.value = false
    localError.value = ''
    initializeValues()
    await nextTick()
    objectiveInput.value?.focus()
  },
)
watch(workflowKey, () => {
  advancedVisible.value = false
  localError.value = ''
  initializeValues()
})

function initializeValues() {
  const values: Record<string, FormValue> = {}
  for (const field of schemaFields.value) {
    if (field.defaultValue !== undefined) {
      if (field.type === 'object' || field.type === 'array') values[field.key] = JSON.stringify(field.defaultValue, null, 2)
      else if (['string', 'number', 'boolean'].includes(typeof field.defaultValue)) {
        values[field.key] = field.defaultValue as FormValue
      } else values[field.key] = ''
    } else if (field.type === 'boolean') values[field.key] = false
    else if (field.type === 'integer') values[field.key] = field.minimum ?? 0
    else values[field.key] = ''
  }
  formValues.value = values
}

function submit() {
  if (!selectedWorkflow.value || !objective.value.trim() || props.busy) return
  const input: Record<string, unknown> = {}
  for (const field of schemaFields.value) {
    const value = formValues.value[field.key]
    if (value === '' || value === undefined || value === null) continue
    if (field.type === 'object' || field.type === 'array') {
      try {
        input[field.key] = typeof value === 'string' ? JSON.parse(value) : value
      } catch {
        localError.value = t('dialog.invalidJson', { field: field.label })
        return
      }
    } else input[field.key] = value
  }
  localError.value = ''
  emit('submit', { workflow: selectedWorkflow.value, objective: objective.value.trim(), input })
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
function setBooleanValue(key: string, event: Event) {
  formValues.value[key] = (event.target as HTMLInputElement).checked
}
function humanize(value: string): string {
  const text = value.replaceAll('_', ' ')
  return text.charAt(0).toUpperCase() + text.slice(1)
}
</script>
