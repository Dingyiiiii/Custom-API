// Small application store: gateway identity for the top bar, the management
// credential state, and a toast queue.
//
// Everything route-specific is fetched by the view that needs it — this store
// only holds data that is genuinely global, so a failing endpoint can never
// blank the whole console.

import { computed, ref } from 'vue'
import { defineStore } from 'pinia'
import { ApiError, api, errorText, needsCredential } from './api'
import { clearToken, loadToken, saveToken } from './auth'
import type { HealthResponse, VersionResponse } from './types'

export type ToastKind = 'ok' | 'error' | 'info'

export interface Toast {
  id: number
  kind: ToastKind
  text: string
}

let nextToastId = 1

export const useAppStore = defineStore('app', () => {
  const version = ref<VersionResponse | null>(null)
  const health = ref<HealthResponse | null>(null)
  const metaError = ref('')
  const toasts = ref<Toast[]>([])

  // Credential state. A loopback deployment never needs any of this; a listener
  // exposed with server.admin_remote does, and the console asks for the token
  // rather than rendering nine broken panels.
  const credentialRequired = ref(false)
  const credentialError = ref('')
  const hasCredential = ref(loadToken() !== '')

  const versionLabel = computed(() => {
    const v = version.value
    if (v === null) return '—'
    const commit = v.commit && v.commit !== '' ? v.commit.slice(0, 8) : 'unknown'
    return `${v.version} · ${commit}`
  })

  async function loadMeta(): Promise<void> {
    metaError.value = ''
    try {
      const [v, h] = await Promise.all([api.version(), api.healthz()])
      version.value = v
      health.value = h
      credentialRequired.value = false
      credentialError.value = ''
    } catch (err) {
      if (needsCredential(err)) {
        credentialRequired.value = true
        credentialError.value = ''
        metaError.value = ''
        return
      }
      metaError.value = errorText(err)
    }
  }

  /**
   * Validates a candidate token against the API before adopting it, so a typo
   * is reported in the dialog instead of bouncing the operator back to a
   * console full of failures.
   */
  async function submitCredential(token: string): Promise<boolean> {
    const trimmed = token.trim()
    if (trimmed === '') {
      credentialError.value = '请输入管理令牌'
      return false
    }
    saveToken(trimmed)
    try {
      const [v, h] = await Promise.all([api.version(), api.healthz()])
      version.value = v
      health.value = h
      hasCredential.value = true
      credentialRequired.value = false
      credentialError.value = ''
      return true
    } catch (err) {
      clearToken()
      hasCredential.value = false
      credentialError.value =
        err instanceof ApiError && err.status === 401
          ? '令牌无效，请重新输入'
          : errorText(err)
      return false
    }
  }

  /** Forgets the stored token and returns to the credential prompt. */
  function forgetCredential(): void {
    clearToken()
    hasCredential.value = false
    version.value = null
    health.value = null
    credentialRequired.value = true
    credentialError.value = ''
  }

  function notify(text: string, kind: ToastKind = 'info'): void {
    const id = nextToastId++
    toasts.value = [...toasts.value, { id, kind, text }]
    window.setTimeout(() => dismiss(id), kind === 'error' ? 9000 : 4000)
  }

  function dismiss(id: number): void {
    toasts.value = toasts.value.filter((t) => t.id !== id)
  }

  return {
    version,
    health,
    metaError,
    versionLabel,
    toasts,
    credentialRequired,
    credentialError,
    hasCredential,
    loadMeta,
    submitCredential,
    forgetCredential,
    notify,
    dismiss,
  }
})
