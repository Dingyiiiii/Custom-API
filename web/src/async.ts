// Minimal async-data helper: one loader, one loading flag, one error string.
//
// Every view needs the same three states, and the rule "a failed fetch shows
// the error text, never a blank panel" is easier to keep when it lives in one
// place instead of nine.

import { onMounted, ref, shallowRef } from 'vue'
import type { Ref, ShallowRef } from 'vue'
import { errorText } from './api'

export interface AsyncOptions {
  /** Run the loader on mount (default true). */
  immediate?: boolean
  /** Keep the last good value when a refresh fails (used while polling). */
  keepOnError?: boolean
}

export interface AsyncResult<T> {
  data: ShallowRef<T | null>
  loading: Ref<boolean>
  error: Ref<string>
  /** True once a load has finished, successfully or not. */
  loaded: Ref<boolean>
  load: () => Promise<void>
}

export function useAsync<T>(loader: () => Promise<T>, options: AsyncOptions = {}): AsyncResult<T> {
  const data = shallowRef<T | null>(null)
  const loading = ref(false)
  const error = ref('')
  const loaded = ref(false)
  let generation = 0

  async function load(): Promise<void> {
    const mine = ++generation
    loading.value = true
    error.value = ''
    try {
      const result = await loader()
      if (mine !== generation) return
      data.value = result
    } catch (err) {
      if (mine !== generation) return
      error.value = errorText(err)
      if (options.keepOnError !== true) data.value = null
    } finally {
      if (mine === generation) {
        loading.value = false
        loaded.value = true
      }
    }
  }

  if (options.immediate !== false) onMounted(() => void load())

  return { data, loading, error, loaded, load }
}
