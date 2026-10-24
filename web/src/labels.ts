// Enum → Simplified Chinese label and badge tone.
//
// Every lookup falls back to the raw wire value, so an enum the gateway grows
// later degrades to something readable instead of an empty cell.

export type Tone = 'ok' | 'warn' | 'danger' | 'info' | 'muted' | 'accent'

export interface Labelled {
  text: string
  tone: Tone
  /** Original wire value, shown as a tooltip. */
  raw: string
}

function make(map: Record<string, [string, Tone]>, raw: string | undefined): Labelled {
  const value = raw ?? ''
  const hit = map[value]
  if (hit) return { text: hit[0], tone: hit[1], raw: value }
  return { text: value === '' ? '—' : value, tone: 'muted', raw: value }
}

const DECISIONS: Record<string, [string, Tone]> = {
  allow: ['放行', 'ok'],
  report: ['仅记录', 'info'],
  block_user: ['拦截用户', 'danger'],
  block_upstream: ['拦截上游', 'warn'],
  upstream_error: ['上游错误', 'danger'],
}

export function decision(raw: string | undefined): Labelled {
  return make(DECISIONS, raw)
}

const VERDICTS: Record<string, [string, Tone]> = {
  consistent: ['计量一致', 'ok'],
  over_billed: ['上游超收', 'danger'],
  under_billed: ['上游少报', 'warn'],
  not_reported: ['上游未上报', 'muted'],
  not_verifiable: ['无法核验', 'muted'],
}

export function verdict(raw: string | undefined): Labelled {
  return make(VERDICTS, raw)
}

/** Enforcement modes shared by the policy engine and the metering engine. */
const MODES: Record<string, [string, Tone]> = {
  enforce: ['强制', 'ok'],
  report: ['观察', 'warn'],
  off: ['关闭', 'muted'],
  disabled: ['关闭', 'muted'],
  block: ['阻断', 'ok'],
  observe: ['观察', 'warn'],
}

export function mode(raw: string | undefined): Labelled {
  return make(MODES, raw)
}

const TRUST: Record<string, [string, Tone]> = {
  trusted: ['可信', 'ok'],
  untrusted: ['不可信', 'warn'],
}

export function trust(raw: string | undefined): Labelled {
  return make(TRUST, raw)
}

const PERIODS: Record<string, [string, Tone]> = {
  daily: ['每日', 'info'],
  weekly: ['每周', 'info'],
  monthly: ['每月', 'info'],
  none: ['不重置', 'muted'],
}

export function period(raw: string | undefined): Labelled {
  return make(PERIODS, raw)
}

const SOURCES: Record<string, [string, Tone]> = {
  configured: ['手写配置', 'accent'],
  discovered: ['自动发现', 'info'],
}

export function source(raw: string | undefined): Labelled {
  return make(SOURCES, raw)
}

const SEVERITIES: Record<string, [string, Tone]> = {
  low: ['低', 'muted'],
  info: ['提示', 'info'],
  medium: ['中', 'warn'],
  high: ['高', 'danger'],
  critical: ['严重', 'danger'],
}

export function severity(raw: string | undefined): Labelled {
  return make(SEVERITIES, raw)
}

const STRATEGIES: Record<string, [string, Tone]> = {
  value: ['性价比优先', 'accent'],
  cost: ['价格优先', 'accent'],
  cheap: ['最低价', 'accent'],
  capability: ['能力优先', 'info'],
  score: ['评分优先', 'info'],
  reliability: ['可靠性优先', 'info'],
  latency: ['延迟优先', 'info'],
  round_robin: ['轮询', 'muted'],
  weighted: ['加权随机', 'muted'],
  random: ['随机', 'muted'],
  fixed: ['固定', 'muted'],
}

export function strategy(raw: string | undefined): Labelled {
  return make(STRATEGIES, raw)
}

const FAIL_MODES: Record<string, [string, Tone]> = {
  closed: ['拒绝（closed）', 'ok'],
  open: ['放过（open）', 'warn'],
}

export function failMode(raw: string | undefined): Labelled {
  return make(FAIL_MODES, raw)
}

const PROMOTIONS: Record<string, string> = {
  off_peak: '低谷折扣',
  peak: '高峰加价',
  discount: '折扣',
  multiplier: '系数',
  promo: '促销',
}

export function promotionKind(raw: string | undefined): string {
  if (!raw) return '—'
  return PROMOTIONS[raw] ?? raw
}

const PROTOCOLS: Record<string, string> = {
  openai: 'OpenAI Chat',
  responses: 'OpenAI Responses',
  anthropic: 'Anthropic Messages',
}

/** Short protocol name for badges; unknown ids are shown verbatim. */
export function protocolName(raw: string | undefined): string {
  if (!raw) return '—'
  return PROTOCOLS[raw] ?? raw
}

/** Tone for a protocol badge, so the three protocols are distinguishable. */
export function protocolTone(raw: string | undefined): Tone {
  switch (raw) {
    case 'openai':
      return 'accent'
    case 'responses':
      return 'info'
    case 'anthropic':
      return 'warn'
    default:
      return 'muted'
  }
}
