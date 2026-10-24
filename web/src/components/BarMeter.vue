<script setup lang="ts">
import { computed } from 'vue'
import { pct } from '../format'

const props = withDefaults(
  defineProps<{
    /** Ratio in 0..1. */
    value: number
    /** Text shown next to the bar; defaults to the percentage. */
    label?: string
    /** Colour thresholds applied when no explicit tone is given. */
    good?: number
    bad?: number
    tone?: '' | 'ok' | 'warn' | 'danger'
  }>(),
  { label: '', good: 0.95, bad: 0.8, tone: '' },
)

const clamped = computed(() => {
  if (!Number.isFinite(props.value)) return 0
  return Math.min(1, Math.max(0, props.value))
})

const fillTone = computed(() => {
  if (props.tone !== '') return props.tone
  if (clamped.value >= props.good) return 'ok'
  if (clamped.value >= props.bad) return 'warn'
  return 'danger'
})

const text = computed(() => (props.label !== '' ? props.label : pct(props.value)))
</script>

<template>
  <div class="bar-row">
    <div class="bar" :title="text">
      <div class="bar-fill" :class="fillTone" :style="{ width: `${clamped * 100}%` }" />
    </div>
    <span class="bar-value">{{ text }}</span>
  </div>
</template>
