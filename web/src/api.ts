// Typed client for the gateway's admin API.
//
// Everything is same-origin and relative: the console is served by the admin
// listener itself, so `/admin/...` needs no base URL and no CORS. A loopback
// deployment needs no credential either; when the operator has enabled remote
// management access the stored admin token is attached to every request.

import { FORBIDDEN, UNAUTHORIZED, authHeaders } from './auth'
import type {
  ApiErrorBody,
  AuditQuery,
  AuditResponse,
  AuditVerifyResult,
  ConfigResponse,
  FingerprintsResponse,
  HealthResponse,
  ModelsRefreshResponse,
  ModelsResponse,
  PriceCompareResponse,
  PricesResponse,
  ProvidersResponse,
  ScoresResponse,
  StatsResponse,
  UsageResponse,
  UserAction,
  UserActionResponse,
  UsersResponse,
  VersionResponse,
} from './types'

/** An error from the admin API, or a transport failure (status 0). */
export class ApiError extends Error {
  /** HTTP status, or 0 when the request never reached the server. */
  readonly status: number
  /** Parsed response body, when there was one. */
  readonly body: unknown

  constructor(message: string, status: number, body: unknown) {
    super(message)
    this.name = 'ApiError'
    this.status = status
    this.body = body
  }
}

/** Error-code name the gateway reports when a credential is missing or wrong. */
export const CODE_UNAUTHORIZED = UNAUTHORIZED
/** Error-code name the gateway reports when remote management access is off. */
export const CODE_FORBIDDEN = FORBIDDEN

/**
 * Thrown when the management listener wants a credential.
 *
 * The console reacts by asking for the admin token rather than showing an
 * error, so this is kept distinct from a plain 403 - where no credential would
 * help because the listener refuses remote callers outright.
 */
export class AuthRequiredError extends ApiError {
  constructor(message: string, status: number, body: unknown) {
    super(message, status, body)
    this.name = 'AuthRequiredError'
  }
}

/** True when the failure can be cured by entering the admin token. */
export function needsCredential(err: unknown): boolean {
  return err instanceof AuthRequiredError
}

// A global hook so that *any* management call - not just the identity fetch in
// the store - makes the console ask for a credential. Without it a view that
// happened to load first would show an error block while the dialog stayed
// hidden, which reads like a broken gateway rather than a missing token.
let credentialHandler: (() => void) | null = null

/** Registers the callback invoked when the gateway demands a credential. */
export function onCredentialRequired(fn: () => void): void {
  credentialHandler = fn
}

function requireCredential(): void {
  credentialHandler?.()
}

/** The gateway's error code from a parsed body, if there is one. */
function errorCode(body: unknown): string {
  if (body !== null && typeof body === 'object') {
    const env = body as ApiErrorBody
    const code = env.error?.code
    if (typeof code === 'string') return code
  }
  return ''
}

/** Renders any thrown value as a message fit for the UI. */
export function errorText(err: unknown): string {
  if (err instanceof ApiError) {
    if (err.status === 0) return err.message
    return `${err.message}（HTTP ${err.status}）`
  }
  if (err instanceof Error) return err.message
  return String(err)
}

/** Pulls `{"error":{"message":...}}` out of a body, with sensible fallbacks. */
function extractMessage(body: unknown, status: number, raw: string): string {
  if (body !== null && typeof body === 'object') {
    const env = body as ApiErrorBody
    const msg = env.error?.message
    if (typeof msg === 'string' && msg !== '') return msg
  }
  if (raw.trim() !== '') {
    // Non-JSON payloads (a proxy error page, say) are still worth showing.
    return raw.trim().slice(0, 300)
  }
  // No body at all. The status is appended by errorText(), so it is not
  // repeated here.
  return status === 0 ? '请求未能到达管理接口' : '管理接口返回了空响应'
}

async function parseBody(res: Response): Promise<{ body: unknown; raw: string }> {
  const raw = await res.text()
  if (raw.trim() === '') return { body: null, raw }
  try {
    return { body: JSON.parse(raw) as unknown, raw }
  } catch {
    return { body: null, raw }
  }
}

/** Performs a request and returns the parsed body, throwing ApiError on failure. */
async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const merged: RequestInit = {
    ...init,
    headers: { ...authHeaders(), ...(init?.headers as Record<string, string> | undefined) },
  }
  let res: Response
  try {
    res = await fetch(path, merged)
  } catch (err) {
    const why = err instanceof Error ? err.message : String(err)
    throw new ApiError(`无法连接管理接口 ${path}：${why}`, 0, null)
  }
  const { body, raw } = await parseBody(res)
  if (!res.ok) {
    const message = extractMessage(body, res.status, raw)
    if (res.status === 401 || errorCode(body) === UNAUTHORIZED) {
      requireCredential()
      throw new AuthRequiredError(message, res.status, body)
    }
    throw new ApiError(message, res.status, body)
  }
  if (body === null) {
    throw new ApiError(`接口 ${path} 返回了空响应`, res.status, null)
  }
  return body as T
}

function query(params: Record<string, string | number | boolean | undefined>): string {
  const q = new URLSearchParams()
  for (const [key, value] of Object.entries(params)) {
    if (value === undefined || value === '') continue
    q.set(key, String(value))
  }
  const s = q.toString()
  return s === '' ? '' : `?${s}`
}

const JSON_POST: RequestInit = {
  method: 'POST',
  headers: { 'Content-Type': 'application/json' },
}

export const api = {
  stats: () => request<StatsResponse>('/admin/stats'),
  version: () => request<VersionResponse>('/admin/version'),
  healthz: () => request<HealthResponse>('/admin/healthz'),
  config: () => request<ConfigResponse>('/admin/config'),

  providers: () => request<ProvidersResponse>('/admin/providers'),
  models: () => request<ModelsResponse>('/admin/models'),
  refreshModels: () => request<ModelsRefreshResponse>('/admin/models/refresh', { method: 'POST' }),

  users: () => request<UsersResponse>('/admin/users'),
  userAction: (user: string, action: UserAction, quota?: number, period?: string) =>
    request<UserActionResponse>('/admin/users', {
      ...JSON_POST,
      body: JSON.stringify({ user, action, quota: quota ?? 0, period: period ?? '' }),
    }),

  usage: () => request<UsageResponse>('/admin/usage'),
  prices: () => request<PricesResponse>('/admin/prices'),
  comparePrices: (model: string, inputTokens: number, outputTokens: number, at?: string) =>
    request<PriceCompareResponse>(
      `/admin/prices/compare${query({
        model,
        input_tokens: inputTokens,
        output_tokens: outputTokens,
        at,
      })}`,
    ),

  scores: () => request<ScoresResponse>('/admin/scores'),
  fingerprints: () => request<FingerprintsResponse>('/admin/fingerprints'),

  audit: (q: AuditQuery = {}) =>
    request<AuditResponse>(
      `/admin/audit${query({
        limit: q.limit,
        offset: q.offset,
        actor: q.actor,
        channel: q.channel,
        model: q.model,
        decision: q.decision,
        verdict: q.verdict,
        include_samples: q.include_samples,
      })}`,
    ),

  /**
   * Verifies the audit hash chain.
   *
   * A broken chain is a *successful* verification with ok=false, and the
   * gateway reports it as HTTP 409 — so the body is the result here, not an
   * error. Any other status still throws.
   */
  async verifyAudit(): Promise<AuditVerifyResult> {
    let res: Response
    try {
      res = await fetch('/admin/audit/verify', { headers: authHeaders() })
    } catch (err) {
      const why = err instanceof Error ? err.message : String(err)
      throw new ApiError(`无法连接审计校验接口：${why}`, 0, null)
    }
    const { body, raw } = await parseBody(res)
    if (res.status === 409 && body !== null) {
      return body as AuditVerifyResult
    }
    if (!res.ok) {
      const message = extractMessage(body, res.status, raw)
      if (res.status === 401 || errorCode(body) === UNAUTHORIZED) {
        requireCredential()
      throw new AuthRequiredError(message, res.status, body)
      }
      throw new ApiError(message, res.status, body)
    }
    if (body === null) {
      throw new ApiError('审计校验接口返回了空响应', res.status, null)
    }
    return body as AuditVerifyResult
  },
}
