// Number, time and text formatting helpers.
//
// Every helper is total: admin JSON omits zero values, so `undefined` is the
// normal case rather than an error, and nothing here may throw or render
// "NaN"/"Invalid Date" into a table cell.

const GROUP = new Intl.NumberFormat('zh-CN')

/** Integer with thousands separators. */
export function num(v: number | undefined | null): string {
  if (v === undefined || v === null || !Number.isFinite(v)) return '0'
  return GROUP.format(Math.round(v))
}

/** Decimal with thousands separators and a fixed precision. */
export function dec(v: number | undefined | null, digits = 2): string {
  if (v === undefined || v === null || !Number.isFinite(v)) return (0).toFixed(digits)
  return v.toLocaleString('zh-CN', { minimumFractionDigits: digits, maximumFractionDigits: digits })
}

/** 1234 → "1.2K", 1_234_567 → "1.2M", 3_400_000_000 → "3.4B". */
export function compact(v: number | undefined | null): string {
  if (v === undefined || v === null || !Number.isFinite(v)) return '0'
  const abs = Math.abs(v)
  if (abs >= 1e9) return trimZero(v / 1e9) + 'B'
  if (abs >= 1e6) return trimZero(v / 1e6) + 'M'
  if (abs >= 1e3) return trimZero(v / 1e3) + 'K'
  return String(Math.round(v))
}

function trimZero(v: number): string {
  return v.toFixed(1).replace(/\.0$/, '')
}

const SYMBOLS: Record<string, string> = { CNY: '¥', USD: '$', EUR: '€', JPY: '¥' }

/** Currency prefix: "¥" for CNY, "$" for USD, otherwise the code itself. */
export function currencySymbol(currency: string | undefined): string {
  if (!currency) return '¥'
  return SYMBOLS[currency.toUpperCase()] ?? `${currency.toUpperCase()} `
}

/** Money, six decimals by default: 0.000738 → "¥0.000738". */
export function money(v: number | undefined | null, currency?: string, digits = 6): string {
  const value = v === undefined || v === null || !Number.isFinite(v) ? 0 : v
  return currencySymbol(currency) + value.toFixed(digits)
}

/** Unit prices are per million tokens and read better with four decimals. */
export function unitPrice(v: number | undefined | null, currency?: string): string {
  return money(v, currency, 4)
}

/** 0.9876 → "98.8%" (input is a 0..1 ratio unless `alreadyPercent`). */
export function pct(v: number | undefined | null, digits = 1, alreadyPercent = false): string {
  if (v === undefined || v === null || !Number.isFinite(v)) return '—'
  const value = alreadyPercent ? v : v * 100
  return `${value.toFixed(digits)}%`
}

/** 1536 → "1.5 KB". */
export function bytes(v: number | undefined | null): string {
  if (v === undefined || v === null || !Number.isFinite(v)) return '0 B'
  const units = ['B', 'KB', 'MB', 'GB', 'TB']
  let value = Math.abs(v)
  let i = 0
  while (value >= 1024 && i < units.length - 1) {
    value /= 1024
    i++
  }
  return `${i === 0 ? Math.round(value) : value.toFixed(1)} ${units[i]}`
}

/** Seconds → "3天 4小时 5分 6秒"; the two largest non-zero units. */
export function duration(seconds: number | undefined | null): string {
  if (seconds === undefined || seconds === null || !Number.isFinite(seconds) || seconds < 0) {
    return '—'
  }
  const s = Math.floor(seconds)
  const days = Math.floor(s / 86400)
  const hours = Math.floor((s % 86400) / 3600)
  const mins = Math.floor((s % 3600) / 60)
  const secs = s % 60
  const parts: string[] = []
  if (days > 0) parts.push(`${days}天`)
  if (hours > 0) parts.push(`${hours}小时`)
  if (mins > 0) parts.push(`${mins}分`)
  if (parts.length < 2) parts.push(`${secs}秒`)
  return parts.slice(0, 2).join(' ')
}

/** Milliseconds → "38 ms" / "1.5 s". */
export function millis(v: number | undefined | null): string {
  if (v === undefined || v === null || !Number.isFinite(v)) return '—'
  if (v >= 1000) return `${(v / 1000).toFixed(2)} s`
  return `${Math.round(v)} ms`
}


/** A signed delta rendered as a percentage, e.g. "-5.0%" / "+0.0%". */
export function signedPct(v: number | undefined | null, digits = 1): string {
  if (v === undefined || v === null || !Number.isFinite(v)) return '—'
  const value = v * 100
  const sign = value > 0 ? '+' : ''
  return `${sign}${value.toFixed(digits)}%`
}

/** True for Go's zero time, which is what an unset time.Time serializes to. */
function isZeroTime(d: Date): boolean {
  return Number.isNaN(d.getTime()) || d.getUTCFullYear() <= 1
}

function parse(iso: string | undefined | null): Date | null {
  if (!iso) return null
  const d = new Date(iso)
  if (isZeroTime(d)) return null
  return d
}

function pad(n: number): string {
  return n < 10 ? `0${n}` : String(n)
}

/** RFC3339 → "2026-09-22 15:04:05" in local time; "—" when unset. */
export function ts(iso: string | undefined | null): string {
  const d = parse(iso)
  if (d === null) return '—'
  return (
    `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ` +
    `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`
  )
}

/** RFC3339 → "09-22 15:04", a compact form for dense tables. */
export function tsShort(iso: string | undefined | null): string {
  const d = parse(iso)
  if (d === null) return '—'
  return `${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`
}

/** RFC3339 → "3 分钟前" / "2 天前"; "—" when unset. */
export function relative(iso: string | undefined | null): string {
  const d = parse(iso)
  if (d === null) return '—'
  const delta = Date.now() - d.getTime()
  if (delta < 0) return '刚刚'
  const s = Math.floor(delta / 1000)
  if (s < 60) return `${s} 秒前`
  const m = Math.floor(s / 60)
  if (m < 60) return `${m} 分钟前`
  const h = Math.floor(m / 60)
  if (h < 24) return `${h} 小时前`
  const days = Math.floor(h / 24)
  if (days < 30) return `${days} 天前`
  return ts(iso)
}

/** Age of a timestamp in milliseconds, or Infinity when unset. */
export function ageMs(iso: string | undefined | null): number {
  const d = parse(iso)
  if (d === null) return Number.POSITIVE_INFINITY
  return Date.now() - d.getTime()
}

/** True when a timestamp is older than `maxAgeMs` (unset counts as old). */
export function olderThan(iso: string | undefined | null, maxAgeMs: number): boolean {
  return ageMs(iso) > maxAgeMs
}

/** Replaces empty/absent strings with an em dash. */
export function dash(v: string | undefined | null): string {
  if (v === undefined || v === null) return '—'
  const s = String(v)
  return s === '' ? '—' : s
}

/** Shortens a hash for a table cell, keeping both ends recognisable. */
export function shortHash(v: string | undefined | null, keep = 8): string {
  if (!v) return '—'
  if (v.length <= keep * 2 + 1) return v
  return `${v.slice(0, keep)}…${v.slice(-keep)}`
}

/** Safe ratio for a progress bar: 0 when the denominator is unusable. */
export function ratio(part: number | undefined, whole: number | undefined): number {
  if (!part || !whole || whole <= 0) return 0
  return Math.min(1, Math.max(0, part / whole))
}

/** Per-million unit price quoted for a token mix; 0 when the mix is empty. */
export function perMillion(cost: number, tokens: number): number {
  if (!tokens) return 0
  return cost / (tokens / 1e6)
}
