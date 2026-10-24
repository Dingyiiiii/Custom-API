// Wire types for the gateway's admin API.
//
// These mirror the JSON the Go handlers actually emit (internal/gateway), not
// only the documented contract. Two details matter:
//
//   * Go's `omitempty` means a zero counter is simply absent, so almost every
//     numeric field is optional here and must be read through the helpers in
//     format.ts rather than used directly.
//   * /admin/providers shadows the embedded ProviderSummary.Models (a count)
//     with a []string of model names, so the array is what arrives on the wire.

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

/** Counters from GET /admin/stats and GET /admin/healthz. */
export interface Counters {
  requests: number
  blocked_outbound: number
  blocked_inbound: number
  upstream_errors: number
  streamed_requests: number
  over_billed: number
  fingerprint_miss: number
  degraded: number
  bytes_in: number
  bytes_out: number
}

export interface StatsResponse {
  uptime_s: number
  stats: Counters
  channels: number
  users: number
  audit_seq: number
  audit_head: string
  audit_path: string
  strategy: string
  outbound_mode: string
  inbound_mode: string
  fail_mode: string
}

export interface VersionResponse {
  version: string
  commit: string
  built: string
}

export interface HealthResponse {
  status: string
  version: string
  uptime_s: number
  channels: number
  users: number
  outbound_rules: number
  inbound_rules: number
  fail_mode: string
  audit_seq: number
  stats: Counters
}

export interface ServerConfig {
  listen: string
  admin_listen: string
  route_strategy: string
  fail_mode: string
  upstream_timeout: string
}

export interface PolicyConfig {
  outbound_mode: string
  inbound_mode: string
  tool_policy: string
  tolerance: number
  outbound_rules: number
  inbound_rules: number
}

/** One channel as reported by GET /admin/config (secrets are never echoed). */
export interface ConfigChannel {
  name: string
  base_url: string
  protocol: string
  trust: string
  enabled: boolean
  weight: number
  has_key: boolean
  models?: string[]
}

export interface ConfigResponse {
  server: ServerConfig
  policy: PolicyConfig
  channels: ConfigChannel[]
}

// ---------------------------------------------------------------------------
// Providers, keys, models
// ---------------------------------------------------------------------------

export interface KeyLimits {
  rpm?: number
  tpm?: number
  monthly_budget?: number
  currency?: string
}

export interface ProviderKey {
  name: string
  has_key: boolean
  prefix?: string
  weight?: number
  enabled: boolean
  limits?: KeyLimits
  note?: string
  /** Expanded channel name, "<provider>/<key>". */
  channel?: string
}

/** One expanded channel: a provider key plus its discovery state. */
export interface ProviderChannel {
  channel: string
  key?: string
  protocol: string
  protocols?: string[]
  has_key: boolean
  enabled: boolean
  weight?: number
  groups?: string[]
  model_count?: number
  discovered?: string
  stale?: boolean
  discover_error?: string
}

export interface ChannelHealth {
  channel: string
  failures?: number
  healthy: boolean
  requests?: number
  errors?: number
  usage_integrity?: number
}

export interface Provider {
  name: string
  base_url: string
  protocol: string
  protocols: string[]
  trust: string
  enabled: boolean
  weight: number
  groups?: string[]
  /** Number of models the provider serves. */
  models?: number
  /** Model names this provider serves, from the catalogue. */
  model_list?: string[]
  keys: ProviderKey[]
  channels: ProviderChannel[]
  health?: ChannelHealth[]
  supports_translation?: string[]
  can_translate?: boolean
  channel_prefix?: string
  metadata?: Record<string, string>
}

export interface ProtocolInfo {
  id: string
  name: string
  endpoint: string
}

export interface RoutingConfig {
  strategy: string
  prefer_same_protocol: boolean
  allow_translation: boolean
  max_attempts: number
  retry_on: string[]
}

export interface ProvidersResponse {
  providers: Provider[]
  direct_channels: ProviderChannel[]
  groups: string[]
  protocols: ProtocolInfo[]
  routing: RoutingConfig
}

export type ModelSource = 'configured' | 'discovered'

export interface ModelEntry {
  model: string
  channel: string
  provider?: string
  key?: string
  upstream_model?: string
  protocol?: string
  source: ModelSource
  owned_by?: string
  created?: number
  discovered_at?: string
}

export interface ModelChannel {
  channel: string
  provider?: string
  key?: string
  protocol: string
  upstream_model?: string
  source: ModelSource
  trust?: string
  translate_from?: string[]
}

/** Per-channel catalogue entry, including staleness and discovery errors. */
export interface ChannelModels {
  channel: string
  provider?: string
  key?: string
  protocol: string
  models: ModelEntry[]
  discovered?: string
  stale?: boolean
  error?: string
  configured_only?: boolean
}

/** Merged, user-facing catalogue entry. */
export interface ModelView {
  model: string
  channels: ModelChannel[]
  protocols: string[]
  configured: boolean
  cheapest?: string
  unit_price?: number
  currency?: string
}

export interface ModelsResponse {
  updated_at: string
  providers: Record<string, number>
  errors: Record<string, string>
  channels: ChannelModels[]
  models: ModelView[]
}

export interface ModelsRefreshResponse {
  ok: boolean
  models: number
}

// ---------------------------------------------------------------------------
// Users and quota
// ---------------------------------------------------------------------------

export interface UserState {
  name: string
  trust?: string
  period?: string
  /** 0 (or absent) means unlimited. */
  quota?: number
  consumed?: number
  lifetime_consumed?: number
  requests?: number
  spend?: number
  currency?: string
  period_start?: string
  last_seen?: string
  enabled: boolean
  models?: string[]
}

export interface UsersResponse {
  count: number
  users: UserState[]
}

export type UserAction = 'grant' | 'add_quota' | 'suspend' | 'resume' | 'set_period'

export interface UserActionResponse {
  ok: boolean
  action: string
  user: UserState
}

// ---------------------------------------------------------------------------
// Usage, prices, scores
// ---------------------------------------------------------------------------

export interface ModelStat {
  model: string
  requests?: number
  prompt_tokens?: number
  completion_tokens?: number
  total_tokens?: number
  cost?: number
  currency?: string
  blocked?: number
  over_billed?: number
}

export interface ChannelStat {
  channel: string
  requests?: number
  failures?: number
  blocked?: number
  prompt_tokens?: number
  completion_tokens?: number
  total_tokens?: number
  cost?: number
  currency?: string
  usage_integrity?: number
  verifiable?: number
  mean_latency_ms?: number
}

export interface UsageResponse {
  records: number
  models: ModelStat[]
  channels: ChannelStat[]
  users: UserState[]
}

export interface PriceRow {
  channel: string
  model: string
  trust: string
  input_per_m: number
  output_per_m: number
  effective_unit_price: number
  currency: string
  scheme?: string
  source?: string
}

export interface PricesResponse {
  prices: PriceRow[]
}

export interface AppliedPromotion {
  kind: string
  multiplier: number
  note?: string
}

export interface PriceOffer {
  channel: string
  currency: string
  cost: number
  list_cost: number
  discount_pct: number
  unit_price: number
  promotions?: AppliedPromotion[]
  trust: string
}

export interface PriceCompareResponse {
  model: string
  input_tokens: number
  output_tokens: number
  at: string
  cheapest?: string
  offers: PriceOffer[]
}

export interface ScoreValues {
  capability?: number
  fingerprint_match?: number
  reliability?: number
  usage_integrity?: number
  benchmarks?: Record<string, number>
  updated_at?: string
  samples?: number
}

export interface ScoreRow {
  channel: string
  model: string
  scores: ScoreValues
}

export interface ScoresResponse {
  scores: ScoreRow[]
}

export interface FingerprintRow {
  channel: string
  model: string
  checked: number
  mismatch: number
  mean_score: number
}

export interface FingerprintsResponse {
  fingerprints: FingerprintRow[]
}

// ---------------------------------------------------------------------------
// Audit ledger
// ---------------------------------------------------------------------------

export type AuditDecision =
  | 'allow'
  | 'report'
  | 'block_user'
  | 'block_upstream'
  | 'upstream_error'

export type UsageVerdict =
  | 'consistent'
  | 'over_billed'
  | 'under_billed'
  | 'not_reported'
  | 'not_verifiable'

export interface FindingRef {
  rule: string
  severity: string
  field?: string
  excerpt_hash?: string
  trust_exempt?: boolean
}

export interface UsageSummary {
  prompt_tokens?: number
  completion_tokens?: number
  total_tokens?: number
  upstream_prompt_tokens?: number
  upstream_completion_tokens?: number
  upstream_total_tokens?: number
  upstream_reported?: boolean
  tokenizer_family?: string
  method?: string
  prompt_delta?: number
  completion_delta?: number
  verdict: UsageVerdict
  tolerance?: number
  min_absolute_delta?: number
  worst_abs?: number
}

export interface CostBreakdown {
  currency?: string
  input_cost?: number
  output_cost?: number
  total_cost?: number
  list_cost?: number
  discount_pct?: number
  basis?: string
  price_model?: string
}

export interface FingerprintEvidence {
  checked: boolean
  score?: number
  threshold?: number
  verdict?: string
  baseline_source?: string
  sample_hash?: string
}

// Every latency field is milliseconds. The names carry the unit because an
// earlier revision mixed milliseconds and nanoseconds in this one object.
export interface LatencyBreakdown {
  total_ms?: number
  outbound_audit_ms?: number
  upstream_ms?: number
  inbound_audit_ms?: number
  ttfb_ms?: number
}

export interface AuditSample {
  request_text?: string
  response_text?: string
}

export interface AuditRecord {
  seq: number
  prev_hash: string
  entry_hash: string
  id: string
  time: string
  end_time?: string
  actor?: string
  channel?: string
  provider?: string
  key?: string
  model?: string
  upstream_model?: string
  protocol?: string
  upstream_protocol?: string
  translated?: boolean
  server_state?: boolean
  stream: boolean
  endpoint?: string
  client_ip?: string
  decision: AuditDecision | string
  status_code?: number
  error?: string
  outbound_findings?: FindingRef[]
  inbound_findings?: FindingRef[]
  usage: UsageSummary
  cost?: CostBreakdown
  fingerprint?: FingerprintEvidence
  latency_ms?: LatencyBreakdown
  request_hash?: string
  response_hash?: string
  route_reason?: string
  attempts?: number
  failover_note?: string
  quota_after?: number
  sample?: AuditSample
}

export interface AuditQuery {
  limit?: number
  offset?: number
  actor?: string
  channel?: string
  model?: string
  decision?: string
  verdict?: string
  include_samples?: boolean
}

export interface AuditResponse {
  records: AuditRecord[]
  total: number
  limit: number
  offset: number
}

export interface AuditViolation {
  kind: string
  line?: number
  seq?: number
  detail?: string
}

/** GET /admin/audit/verify; served with HTTP 409 when ok is false. */
export interface AuditVerifyResult {
  ok: boolean
  records: number
  head_seq: number
  head_hash: string
  violations?: AuditViolation[]
  anchors_checked?: number
  anchors_ignored?: number
}

/** The gateway's uniform error envelope. */
export interface ApiErrorBody {
  error?: {
    message?: string
    type?: string
    code?: string
  }
}

/** A catalogue price row for one channel+model (GET /admin/prices). */
export interface PriceQuery {
  model: string
  input_tokens: number
  output_tokens: number
  at?: string
}
