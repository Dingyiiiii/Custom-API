<script setup lang="ts">
// Uniform loading / error / empty handling for every data panel.
//
// A failed fetch must render the server's message: the whole point of the
// console is telling an operator what is wrong.
withDefaults(
  defineProps<{
    loading?: boolean
    error?: string
    empty?: boolean
    loadingText?: string
    emptyText?: string
  }>(),
  {
    loading: false,
    error: '',
    empty: false,
    loadingText: '加载中…',
    emptyText: '暂无数据',
  },
)

defineEmits<{ retry: [] }>()
</script>

<template>
  <div v-if="loading" class="state">
    <span class="spinner" />{{ loadingText }}
  </div>
  <div v-else-if="error" class="state error">
    <div>{{ error }}</div>
    <button class="btn btn-sm retry" type="button" @click="$emit('retry')">重试</button>
  </div>
  <div v-else-if="empty" class="state">{{ emptyText }}</div>
  <slot v-else />
</template>
