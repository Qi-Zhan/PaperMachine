<template>
  <div class="workflow-library">
    <header class="page-header workflow-page-header">
      <div class="page-leading">
        <button class="icon-button mobile-only" type="button" :title="t('common.openSidebar')" :aria-label="t('common.openSidebar')" @click="$emit('open-sidebar')">
          <PanelLeft :size="18" />
        </button>
        <div>
          <p class="eyebrow">{{ t('workflow.runtime') }}</p>
          <h1>{{ t('workflow.title') }}</h1>
        </div>
      </div>
      <button class="primary-button" type="button" @click="newDraft">
        <Plus :size="15" />
        {{ t('workflow.new') }}
      </button>
    </header>

    <div class="workflow-library-layout">
      <aside class="workflow-catalog-panel">
        <div class="workflow-catalog-heading">
          <span>{{ t('workflow.catalog') }}</span>
          <span>{{ workflows.length }}</span>
        </div>
        <button
          v-for="workflow in workflows"
          :key="keyOf(workflow)"
          class="workflow-catalog-row"
          :data-active="selectedKey === keyOf(workflow)"
          type="button"
          @click="loadWorkflow(workflow)"
        >
          <component :is="workflow.source === 'builtin' ? Boxes : UserRound" :size="15" />
          <span>
            <strong>{{ workflow.manifest.name }}</strong>
            <small>{{ workflow.manifest.slug }} · {{ sourceLabel(workflow.source) }}</small>
          </span>
          <ChevronRight :size="14" />
        </button>
        <p v-if="workflows.length === 0" class="workflow-catalog-empty">{{ t('workflow.empty') }}</p>
      </aside>

      <main class="workflow-studio">
        <section class="workflow-generator-band">
          <div class="workflow-generator-fields">
            <label>
              <span>{{ t('common.name') }}</span>
              <input v-model="generation.name" class="text-input" autocomplete="off" maxlength="120" />
            </label>
            <label>
              <span>Slug</span>
              <input v-model="generation.slug" class="text-input" autocomplete="off" maxlength="64" />
            </label>
          </div>
          <label class="workflow-prompt-field">
            <span>{{ t('workflow.describeSessions') }}</span>
            <textarea v-model="generation.description" class="text-area" maxlength="12000" />
          </label>
          <div class="workflow-generator-actions">
            <span v-if="selectedRegistration" class="workflow-source-line">
              <component :is="selectedRegistration.source === 'builtin' ? Boxes : UserRound" :size="13" />
              {{ sourceLabel(selectedRegistration.source) }} · {{ selectedRegistration.manifest.slug }}
            </span>
            <span v-else class="workflow-source-line">{{ t('workflow.sourceHint') }}</span>
            <button class="secondary-button" type="button" :disabled="Boolean(busyAction) || !generation.description.trim()" @click="generate">
              <LoaderCircle v-if="busyAction === 'generate'" class="spin" :size="14" />
              <WandSparkles v-else :size="14" />
              {{ t('workflow.generateDraft') }}
            </button>
          </div>
        </section>

        <section class="workflow-program-panel">
          <header class="workflow-editor-toolbar">
            <div>
              <p class="eyebrow">{{ t('workflow.protocol') }}</p>
              <h2>{{ editorTitle }}</h2>
            </div>
            <span v-if="status.message" class="workflow-editor-status" :data-kind="status.kind">
              <CheckCircle2 v-if="status.kind === 'success'" :size="14" />
              <AlertCircle v-else :size="14" />
              {{ status.message }}
            </span>
          </header>

          <div class="workflow-program-content">
            <div v-if="busyAction === 'load'" class="workflow-program-empty">
              <LoaderCircle class="spin" :size="18" />
              <span>{{ t('workflow.reading') }}</span>
            </div>
            <div v-else-if="!sourceText.trim()" class="workflow-program-empty">
              <Workflow :size="22" />
              <strong>{{ t('workflow.describeProtocol') }}</strong>
              <span>{{ t('workflow.draftHelp') }}</span>
            </div>
            <template v-else>
              <section v-if="manifest" class="workflow-protocol-intro">
                <p>{{ manifest.description }}</p>
                <dl>
                  <div><dt>{{ t('workflow.entrypoint') }}</dt><dd>{{ manifest.entrypoint }}</dd></div>
                  <div><dt>{{ t('workflow.params') }}</dt><dd>{{ schemaKeys(manifest.params_schema) }}</dd></div>
                </dl>
              </section>

              <div v-if="validation" class="workflow-structure-grid">
                <section class="workflow-structure-section">
                  <header><Users :size="15" /><h3>{{ t('workflow.agentSessions') }}</h3><span>{{ validation.agents.length }}</span></header>
                  <div v-if="validation.agents.length" class="workflow-agent-declarations">
                    <div v-for="agent in validation.agents" :key="agent.class_name" class="workflow-agent-declaration">
                      <strong>{{ agent.class_name }}</strong>
                      <small>{{ accessLabel(agent.access) }}</small>
                      <span v-if="agent.actions.length">{{ agent.actions.join(' · ') }}</span>
                      <span v-else>{{ t('workflow.noActions') }}</span>
                    </div>
                  </div>
                  <p v-else class="workflow-structure-empty">{{ t('workflow.noAgentClasses') }}</p>
                </section>

                <section class="workflow-structure-section">
                  <header><Network :size="15" /><h3>{{ t('workflow.coordination') }}</h3></header>
                  <dl class="workflow-feature-list">
                    <div><dt>{{ t('workflow.parallelBlocks') }}</dt><dd>{{ validation.features.parallel_blocks }}</dd></div>
                    <div><dt>{{ t('workflow.teams') }}</dt><dd>{{ namedFeature(validation.features.teams) }}</dd></div>
                    <div><dt>{{ t('workflow.relations') }}</dt><dd>{{ validation.features.relations }}</dd></div>
                    <div><dt>{{ t('workflow.taskScopes') }}</dt><dd>{{ namedFeature(validation.features.scopes) }}</dd></div>
                    <div><dt>{{ t('workflow.channels') }}</dt><dd>{{ namedFeature(validation.features.channels) }}</dd></div>
                  </dl>
                </section>

                <section class="workflow-structure-section">
                  <header><Clock3 :size="15" /><h3>{{ t('workflow.longRunning') }}</h3></header>
                  <dl class="workflow-feature-list">
                    <div><dt>{{ t('workflow.timers') }}</dt><dd>{{ timerSummary }}</dd></div>
                    <div><dt>{{ t('workflow.backgroundTasks') }}</dt><dd>{{ validation.features.background_tasks }}</dd></div>
                    <div><dt>{{ t('workflow.humanCheckpoints') }}</dt><dd>{{ validation.features.human_checkpoints }}</dd></div>
                    <div><dt>{{ t('workflow.projectSnapshots') }}</dt><dd>{{ validation.features.project_snapshots }}</dd></div>
                    <div><dt>{{ t('workflow.publishedArtifacts') }}</dt><dd>{{ validation.features.artifacts }}</dd></div>
                  </dl>
                </section>
              </div>

              <section v-if="validation?.diagnostics.length" class="workflow-diagnostics">
                <header><AlertCircle :size="15" /><h3>{{ t('workflow.diagnostics') }}</h3></header>
                <p v-for="(diagnostic, index) in validation.diagnostics" :key="`${diagnostic.line}-${index}`" :data-severity="diagnostic.severity">
                  <span>{{ diagnostic.line ? t('workflow.line', { line: diagnostic.line }) : 'Workflow' }}</span>
                  {{ diagnostic.message }}
                </p>
              </section>

              <details class="workflow-source-details">
                <summary><Code2 :size="15" />{{ t('workflow.advancedSource') }}</summary>
                <textarea
                  v-model="sourceText"
                  class="workflow-code-editor"
                  :aria-label="t('workflow.sourceAria')"
                  autocomplete="off"
                  autocapitalize="off"
                  spellcheck="false"
                  wrap="off"
                  @input="sourceChanged"
                />
              </details>
            </template>
          </div>

          <footer class="workflow-editor-actions">
            <span>{{ sourceStats }}</span>
            <div>
              <button class="secondary-button" type="button" :disabled="Boolean(busyAction) || !sourceText.trim()" @click="validate">
                <LoaderCircle v-if="busyAction === 'validate'" class="spin" :size="14" />
                <BadgeCheck v-else :size="14" />
                {{ t('workflow.validate') }}
              </button>
              <button class="primary-button" type="button" :disabled="Boolean(busyAction) || !canSave" @click="save">
                <LoaderCircle v-if="busyAction === 'save'" class="spin" :size="14" />
                <Save v-else :size="14" />
                {{ t('workflow.save') }}
              </button>
            </div>
          </footer>
        </section>
      </main>
    </div>
  </div>
</template>

<script setup lang="ts">
import {
  AlertCircle,
  BadgeCheck,
  Boxes,
  CheckCircle2,
  ChevronRight,
  Clock3,
  Code2,
  LoaderCircle,
  Network,
  PanelLeft,
  Plus,
  Save,
  UserRound,
  Users,
  WandSparkles,
  Workflow,
} from '@lucide/vue'
import { computed, reactive, ref } from 'vue'
import { api } from '../api'
import { useAppI18n } from '../i18n'
import type {
  AccessPreset,
  WorkflowGenerationInput,
  WorkflowProgramManifest,
  WorkflowProgram,
  WorkflowValidation,
} from '../types'

const props = defineProps<{ projectId: string; workflows: WorkflowProgram[] }>()
const emit = defineEmits<{ 'open-sidebar': []; saved: [workflow: WorkflowProgram] }>()

const selectedKey = ref<string | null>(null)
const sourceText = ref('')
const validation = ref<WorkflowValidation | null>(null)
const busyAction = ref<'' | 'load' | 'generate' | 'validate' | 'save'>('')
const generation = reactive<WorkflowGenerationInput>({ name: '', slug: '', description: '' })
const status = reactive<{ kind: 'success' | 'error'; message: string }>({ kind: 'success', message: '' })
const { t } = useAppI18n()

const selectedRegistration = computed(
  () => props.workflows.find((workflow) => keyOf(workflow) === selectedKey.value) ?? null,
)
const manifest = computed<WorkflowProgramManifest | null>(
  () => validation.value?.manifest ?? selectedRegistration.value?.manifest ?? null,
)
const canSave = computed(() => Boolean(validation.value?.valid && validation.value.manifest))
const editorTitle = computed(() => manifest.value?.name || generation.name?.trim() || t('workflow.draft'))
const sourceStats = computed(() => {
  if (!sourceText.value.trim()) return t('workflow.noDraft')
  const lineCount = sourceText.value.split('\n').length
  const lines = t('workflow.lines', { count: lineCount })
  if (!validation.value) return `${lines} · ${t('workflow.validationRequired')}`
  return `${lines} · ${validation.value.valid ? t('workflow.readyToPublish') : t('workflow.needsChanges')}`
})
const timerSummary = computed(() => {
  const timers = validation.value?.features.timers ?? []
  if (!timers.length) return t('common.none')
  return timers.map((timer) => `${timer.callback}${timer.seconds ? ` · ${timer.seconds}s` : ''}`).join(', ')
})

function accessLabel(access: AccessPreset): string {
  if (access === 'model_only') return t('access.modelOnly')
  if (access === 'read_only') return t('access.readOnly')
  if (access === 'workspace') return t('access.workspace')
  if (access === 'research') return t('access.research')
  return t('access.fullAccess')
}

function keyOf(workflow: WorkflowProgram) {
  return workflow.manifest.slug
}

function newDraft() {
  selectedKey.value = null
  sourceText.value = ''
  validation.value = null
  generation.name = ''
  generation.slug = ''
  generation.description = ''
  clearStatus()
}

async function loadWorkflow(workflow: WorkflowProgram) {
  selectedKey.value = keyOf(workflow)
  busyAction.value = 'load'
  clearStatus()
  try {
    const loaded = await api.getWorkflowProgram(props.projectId, workflow.manifest.slug)
    sourceText.value = loaded.source
    generation.name = workflow.manifest.name
    generation.slug = workflow.manifest.slug
    generation.description = workflow.manifest.description
    validation.value = loaded.validation
  } catch (error) {
    setError(error)
  } finally {
    busyAction.value = ''
  }
}

async function generate() {
  if (!generation.description.trim()) return
  busyAction.value = 'generate'
  clearStatus()
  try {
    const generated = await api.generateWorkflow(props.projectId, {
      description: generation.description.trim(),
      ...(generation.name?.trim() ? { name: generation.name.trim() } : {}),
      ...(generation.slug?.trim() ? { slug: generation.slug.trim() } : {}),
    })
    selectedKey.value = null
    sourceText.value = generated.source
    validation.value = generated.validation
    if (generated.validation.manifest) {
      generation.name = generated.validation.manifest.name
      generation.slug = generated.validation.manifest.slug
    }
    setValidationStatus(generated.validation, t('workflow.draftGenerated'))
  } catch (error) {
    setError(error)
  } finally {
    busyAction.value = ''
  }
}

async function validate() {
  if (!sourceText.value.trim()) return
  busyAction.value = 'validate'
  clearStatus()
  try {
    const result = await api.validateWorkflow(props.projectId, sourceText.value)
    validation.value = result
    setValidationStatus(result, t('workflow.valid'))
  } catch (error) {
    setError(error)
  } finally {
    busyAction.value = ''
  }
}

async function save() {
  if (!sourceText.value.trim()) return
  busyAction.value = 'save'
  clearStatus()
  try {
    const result = await api.validateWorkflow(props.projectId, sourceText.value)
    validation.value = result
    if (!result.valid) {
      setValidationStatus(result, '')
      return
    }
    const registration = await api.saveWorkflowProgram(props.projectId, sourceText.value)
    selectedKey.value = keyOf(registration)
    status.kind = 'success'
    status.message = t('workflow.saved')
    emit('saved', registration)
  } catch (error) {
    setError(error)
  } finally {
    busyAction.value = ''
  }
}

function sourceChanged() {
  validation.value = null
  clearStatus()
}

function setValidationStatus(result: WorkflowValidation, successMessage: string) {
  if (result.valid) {
    status.kind = 'success'
    status.message = successMessage
    return
  }
  status.kind = 'error'
  const errors = result.diagnostics.filter((diagnostic) => diagnostic.severity === 'error').length
  status.message = t(errors === 1 ? 'workflow.validationError' : 'workflow.validationErrors', { count: errors })
}

function schemaKeys(schema: Record<string, unknown>): string {
  const properties = schema.properties
  if (!properties || typeof properties !== 'object' || Array.isArray(properties)) return t('common.none')
  const keys = Object.keys(properties)
  return keys.length ? keys.join(', ') : t('common.none')
}

function namedFeature(values: string[]): string {
  return values.length ? values.join(', ') : t('common.none')
}

function sourceLabel(source: 'builtin' | 'user'): string {
  return t(source === 'builtin' ? 'workflow.sourceBuiltin' : 'workflow.sourceUser')
}

function clearStatus() {
  status.message = ''
}

function setError(error: unknown) {
  status.kind = 'error'
  status.message = error instanceof Error ? error.message : String(error)
}
</script>
