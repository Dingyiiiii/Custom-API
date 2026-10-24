// Dev-only fixture: a stand-in for the gateway's admin listener.
//
// The real console talks to the Go gateway; this script exists so the front end
// can be developed and smoke-tested without a running gateway, and so the JSON
// shapes stay pinned to the ones documented in the contract. It is never part
// of the build (vite only bundles src/).
//
//   node scripts/mock-admin.mjs            # listens on 127.0.0.1:8081
//   npm run dev                            # vite proxies /admin to it
//
// Note the shapes copied from the Go handlers rather than from the prose
// contract: user counters are omitempty (a 0 quota is *absent*), provider
// `models` is a list of names, and every audit latency field carries its unit
// in the name (`*_ms`).

import { createServer } from 'node:http'

const PORT = Number(process.env.MOCK_ADMIN_PORT ?? 8081)
const NOW = Date.now()
const iso = (offsetMs) => new Date(NOW - offsetMs).toISOString()

const stats = {
  requests: 12345,
  blocked_outbound: 7,
  blocked_inbound: 3,
  upstream_errors: 12,
  streamed_requests: 4210,
  over_billed: 2,
  fingerprint_miss: 1,
  degraded: 4,
  bytes_in: 918273645,
  bytes_out: 1827364554,
}

const providers = [
  {
    name: 'openai',
    base_url: 'https://api.openai.com',
    protocol: 'openai',
    protocols: ['openai', 'responses'],
    trust: 'trusted',
    enabled: true,
    weight: 2,
    groups: ['premium'],
    models: ['gpt-4o', 'gpt-4o-mini', 'o3-mini'],
    keys: [
      {
        name: 'k1',
        has_key: true,
        prefix: 'sk-abc…xy',
        weight: 2,
        enabled: true,
        limits: { rpm: 500, tpm: 1000000, monthly_budget: 100, currency: 'CNY' },
        note: '主线路',
        channel: 'openai/k1',
      },
      {
        name: 'k2',
        has_key: true,
        prefix: 'sk-def…uv',
        weight: 1,
        enabled: false,
        limits: { rpm: 60, currency: 'CNY' },
        note: '',
        channel: 'openai/k2',
      },
    ],
    channels: [
      {
        channel: 'openai/k1',
        key: 'k1',
        protocol: 'openai',
        protocols: ['openai', 'responses'],
        has_key: true,
        enabled: true,
        weight: 2,
        groups: ['premium'],
        model_count: 3,
        discovered: '2026-09-22 15:04:05',
        stale: false,
        discover_error: '',
      },
      {
        channel: 'openai/k2',
        key: 'k2',
        protocol: 'openai',
        protocols: ['openai', 'responses'],
        has_key: true,
        enabled: false,
        weight: 1,
        groups: ['premium'],
        model_count: 0,
        discovered: '2026-09-21 09:00:00',
        stale: true,
        discover_error: 'dial tcp 1.2.3.4:443: i/o timeout',
      },
    ],
    health: [
      { channel: 'openai/k1', failures: 0, healthy: true, requests: 1204, errors: 3, usage_integrity: 0.99 },
      { channel: 'openai/k2', failures: 4, healthy: false, requests: 90, errors: 12, usage_integrity: 0.62 },
    ],
    supports_translation: ['anthropic'],
    can_translate: true,
  },
  {
    name: 'deepseek',
    base_url: 'https://api.deepseek.com/v1',
    protocol: 'openai',
    protocols: ['openai'],
    trust: 'untrusted',
    enabled: true,
    weight: 1,
    groups: ['cheap'],
    models: ['deepseek-chat', 'gpt-4o-mini'],
    keys: [
      {
        name: 'main',
        has_key: true,
        prefix: 'sk-zzz…11',
        weight: 3,
        enabled: true,
        limits: { tpm: 200000, monthly_budget: 50, currency: 'CNY' },
        note: '促销中',
        channel: 'deepseek/main',
      },
    ],
    channels: [
      {
        channel: 'deepseek/main',
        key: 'main',
        protocol: 'openai',
        protocols: ['openai'],
        has_key: true,
        enabled: true,
        weight: 3,
        groups: ['cheap'],
        model_count: 2,
        discovered: '2026-09-22 15:04:11',
        stale: false,
        discover_error: '',
      },
    ],
    health: [
      { channel: 'deepseek/main', failures: 0, healthy: true, requests: 800, errors: 1, usage_integrity: 1 },
    ],
    supports_translation: ['responses', 'anthropic'],
    can_translate: true,
  },
]

const users = [
  {
    name: 'alice',
    trust: 'untrusted',
    period: 'monthly',
    quota: 50000000,
    consumed: 738000,
    lifetime_consumed: 3382000,
    requests: 421,
    spend: 0.738,
    currency: 'CNY',
    period_start: iso(86400000 * 12),
    last_seen: iso(120000),
    enabled: true,
  },
  {
    // quota 0 → the Go struct omits the field entirely: unlimited.
    name: 'bob',
    trust: 'trusted',
    period: 'none',
    lifetime_consumed: 128000,
    requests: 12,
    spend: 0.000738,
    currency: 'CNY',
    period_start: iso(86400000 * 90),
    last_seen: iso(3600000 * 5),
    enabled: true,
  },
  {
    name: 'carol',
    trust: 'untrusted',
    period: 'daily',
    quota: 1000000,
    consumed: 999500,
    lifetime_consumed: 15000000,
    requests: 99,
    spend: 1.5,
    currency: 'CNY',
    period_start: iso(3600000 * 20),
    last_seen: iso(86400000 * 2),
    enabled: false,
  },
]

const modelsPayload = {
  updated_at: iso(3600000 * 3),
  providers: { openai: 3, deepseek: 2 },
  errors: { deepseek: 'dial tcp: lookup api.deepseek.com: no such host' },
  channels: [
    {
      channel: 'openai/k1',
      provider: 'openai',
      key: 'k1',
      protocol: 'openai',
      models: [
        {
          model: 'gpt-4o',
          channel: 'openai/k1',
          provider: 'openai',
          key: 'k1',
          upstream_model: 'gpt-4o-2024-11-20',
          protocol: 'openai',
          source: 'discovered',
          owned_by: 'openai',
          discovered_at: iso(3600000 * 3),
        },
        {
          model: 'gpt-4o-mini',
          channel: 'openai/k1',
          provider: 'openai',
          key: 'k1',
          upstream_model: 'gpt-4o-mini',
          protocol: 'openai',
          source: 'configured',
        },
      ],
      discovered: iso(3600000 * 3),
      stale: false,
      error: '',
    },
    {
      channel: 'deepseek/main',
      provider: 'deepseek',
      key: 'main',
      protocol: 'openai',
      models: [
        {
          model: 'deepseek-chat',
          channel: 'deepseek/main',
          provider: 'deepseek',
          key: 'main',
          protocol: 'openai',
          source: 'configured',
        },
      ],
      discovered: iso(86400000 * 2),
      stale: true,
      error: 'dial tcp: lookup api.deepseek.com: no such host',
      configured_only: true,
    },
  ],
  models: [
    {
      model: 'gpt-4o',
      channels: [
        {
          channel: 'openai/k1',
          provider: 'openai',
          key: 'k1',
          protocol: 'openai',
          upstream_model: 'gpt-4o-2024-11-20',
          source: 'discovered',
          trust: 'trusted',
          translate_from: ['anthropic'],
        },
        {
          channel: 'deepseek/main',
          provider: 'deepseek',
          key: 'main',
          protocol: 'openai',
          upstream_model: 'gpt-4o',
          source: 'configured',
          trust: 'untrusted',
        },
      ],
      protocols: ['openai', 'responses'],
      configured: true,
      cheapest: 'deepseek/main',
      unit_price: 18.5,
      currency: 'CNY',
    },
    {
      model: 'gpt-4o-mini',
      channels: [
        {
          channel: 'openai/k1',
          provider: 'openai',
          key: 'k1',
          protocol: 'openai',
          upstream_model: 'gpt-4o-mini',
          source: 'configured',
          trust: 'trusted',
        },
      ],
      protocols: ['openai'],
      configured: true,
      cheapest: 'openai/k1',
      unit_price: 1.2,
      currency: 'CNY',
    },
    {
      model: 'deepseek-chat',
      channels: [
        {
          channel: 'deepseek/main',
          provider: 'deepseek',
          key: 'main',
          protocol: 'openai',
          source: 'discovered',
          trust: 'untrusted',
        },
      ],
      protocols: ['openai'],
      configured: false,
    },
  ],
}

function auditRecord(seq, overrides = {}) {
  return {
    seq,
    prev_hash: 'a'.repeat(64),
    entry_hash: `${seq}`.padStart(64, 'b'),
    id: `req_${seq}`,
    time: iso(60000 * seq),
    end_time: iso(60000 * seq - 38),
    actor: 'alice',
    channel: 'openai/k1',
    provider: 'openai',
    key: 'k1',
    model: 'gpt-4o',
    upstream_model: 'gpt-4o-2024-11-20',
    protocol: 'openai',
    upstream_protocol: 'openai',
    stream: false,
    endpoint: '/v1/chat/completions',
    decision: 'allow',
    status_code: 200,
    usage: {
      prompt_tokens: 20,
      completion_tokens: 7,
      total_tokens: 27,
      upstream_prompt_tokens: 19,
      upstream_completion_tokens: 7,
      upstream_total_tokens: 26,
      upstream_reported: true,
      tokenizer_family: 'o200k',
      method: 'upstream',
      prompt_delta: -0.05,
      verdict: 'consistent',
      tolerance: 0.25,
      min_absolute_delta: 8,
      worst_abs: 1,
    },
    cost: {
      currency: 'CNY',
      input_cost: 0.0002,
      output_cost: 0.0001,
      total_cost: 0.0003,
      list_cost: 0.0003,
      basis: 'catalogue',
      price_model: 'gpt-4o',
    },
    fingerprint: { checked: false },
    latency_ms: { total_ms: 38, outbound_audit_ms: 11, inbound_audit_ms: 3, upstream_ms: 30, ttfb_ms: 35 },
    request_hash: 'c'.repeat(64),
    response_hash: 'd'.repeat(64),
    route_reason: 'strategy=value picked openai/k1 (score 0.94, price 18.5)',
    attempts: 1,
    quota_after: 49999262,
    ...overrides,
  }
}

const auditRecords = [
  auditRecord(60, {
    translated: true,
    protocol: 'anthropic',
    upstream_protocol: 'openai',
    endpoint: '/v1/messages',
    model: 'gpt-4o',
    server_state: true,
    stream: true,
    outbound_findings: [
      {
        rule: 'pii.email',
        severity: 'medium',
        field: 'messages[0].content',
        excerpt_hash: 'e'.repeat(64),
        trust_exempt: false,
      },
    ],
    usage: {
      prompt_tokens: 1000,
      completion_tokens: 500,
      total_tokens: 1500,
      upstream_prompt_tokens: 1400,
      upstream_completion_tokens: 500,
      upstream_total_tokens: 1900,
      upstream_reported: true,
      tokenizer_family: 'cl100k',
      method: 'upstream',
      prompt_delta: 0.4,
      completion_delta: 0,
      verdict: 'over_billed',
      tolerance: 0.25,
      min_absolute_delta: 8,
      worst_abs: 400,
    },
    cost: {
      currency: 'CNY',
      input_cost: 0.0252,
      output_cost: 0.036,
      total_cost: 0.0612,
      list_cost: 0.068,
      discount_pct: 10,
      basis: 'catalogue',
      price_model: 'gpt-4o',
    },
    fingerprint: { checked: true, score: 0.05, threshold: 0.3, verdict: 'match', baseline_source: 'builtin' },
  }),
  auditRecord(59, {
    decision: 'block_user',
    status_code: 403,
    error: 'request blocked by outbound rule secrets.aws_key',
    outbound_findings: [
      { rule: 'secrets.aws_key', severity: 'high', field: 'messages[1].content', excerpt_hash: 'f'.repeat(64) },
    ],
    usage: { prompt_tokens: 30, completion_tokens: 0, total_tokens: 30, verdict: 'not_reported' },
    cost: {},
    latency_ms: { total_ms: 4, outbound_audit_ms: 3 },
    route_reason: '',
    attempts: 0,
    failover_note: '',
  }),
  auditRecord(58, {
    actor: 'bob',
    channel: 'deepseek/main',
    provider: 'deepseek',
    key: 'main',
    decision: 'upstream_error',
    status_code: 502,
    error: 'upstream returned 502 Bad Gateway',
    attempts: 3,
    failover_note: 'openai/k1 circuit open after 4 failures; retried deepseek/main',
    usage: { prompt_tokens: 12, completion_tokens: 0, total_tokens: 12, verdict: 'not_verifiable' },
    latency_ms: { total_ms: 1500, upstream_ms: 1480, ttfb_ms: 1480, inbound_audit_ms: 19 },
    quota_after: 0,
  }),
  auditRecord(57, {
    actor: 'carol',
    model: 'gpt-4o-mini',
    stream: true,
    usage: {
      prompt_tokens: 900,
      completion_tokens: 300,
      total_tokens: 1200,
      upstream_reported: false,
      verdict: 'not_reported',
    },
  }),
]

const routingPayload = {
  strategy: 'value',
  prefer_same_protocol: true,
  allow_translation: true,
  max_attempts: 3,
  retry_on: ['5xx', '429', 'timeout', 'network'],
}

const prices = [
  {
    channel: 'openai/k1',
    model: 'gpt-4o',
    trust: 'trusted',
    input_per_m: 18,
    output_per_m: 72,
    effective_unit_price: 45,
    currency: 'CNY',
    scheme: 'multiplier',
    source: 'catalogue',
  },
  {
    channel: 'deepseek/main',
    model: 'gpt-4o',
    trust: 'untrusted',
    input_per_m: 10,
    output_per_m: 40,
    effective_unit_price: 25,
    currency: 'CNY',
    scheme: 'multiplier',
    source: 'catalogue',
  },
]

function usagePayload() {
  return {
    records: auditRecords.length,
    models: [
      {
        model: 'gpt-4o',
        requests: 1204,
        prompt_tokens: 1200000,
        completion_tokens: 500000,
        total_tokens: 1700000,
        cost: 76.5,
        currency: 'CNY',
        blocked: 3,
        over_billed: 1,
      },
      {
        model: 'deepseek-chat',
        requests: 800,
        prompt_tokens: 400000,
        completion_tokens: 100000,
        total_tokens: 500000,
        cost: 8.4,
        currency: 'CNY',
        blocked: 0,
        over_billed: 0,
      },
    ],
    channels: [
      {
        channel: 'openai/k1',
        requests: 1204,
        failures: 3,
        blocked: 3,
        prompt_tokens: 1200000,
        completion_tokens: 500000,
        total_tokens: 1700000,
        cost: 76.5,
        currency: 'CNY',
        usage_integrity: 0.75,
        verifiable: 1100,
        mean_latency_ms: 150,
      },
      {
        channel: 'deepseek/main',
        requests: 800,
        failures: 1,
        blocked: 0,
        prompt_tokens: 400000,
        completion_tokens: 100000,
        total_tokens: 500000,
        cost: 8.4,
        currency: 'CNY',
        usage_integrity: 1,
        verifiable: 780,
        mean_latency_ms: 320.5,
      },
    ],
    users,
  }
}

const scoresPayload = {
  scores: [
    {
      channel: 'openai/k1',
      model: 'gpt-4o',
      scores: {
        capability: 0.95,
        fingerprint_match: 1,
        reliability: 0.99,
        usage_integrity: 1,
        benchmarks: { math: 1, code: 0.8, level_E: 1, chinese: 0.72, reasoning: 0.91 },
        updated_at: iso(3600000 * 2),
        samples: 3,
      },
    },
    {
      channel: 'deepseek/main',
      model: 'deepseek-chat',
      scores: {
        capability: 0.62,
        fingerprint_match: 0.4,
        reliability: 0.81,
        usage_integrity: 0.55,
        benchmarks: { math: 0.5, code: 0.44 },
        updated_at: iso(86400000 * 4),
        samples: 12,
      },
    },
  ],
}

function configPayload() {
  return {
    server: {
      listen: '127.0.0.1:8080',
      admin_listen: '127.0.0.1:8081',
      route_strategy: 'value',
      fail_mode: 'closed',
      upstream_timeout: '300s0ms',
    },
    policy: {
      outbound_mode: 'enforce',
      inbound_mode: 'enforce',
      tool_policy: 'block',
      tolerance: 0.25,
      outbound_rules: 6,
      inbound_rules: 7,
    },
    channels: [
      {
        name: 'openai/k1',
        base_url: 'https://api.openai.com',
        protocol: 'openai',
        trust: 'trusted',
        enabled: true,
        weight: 2,
        has_key: true,
        models: ['gpt-4o', 'gpt-4o-mini'],
      },
      {
        name: 'deepseek/main',
        base_url: 'https://api.deepseek.com/v1',
        protocol: 'openai',
        trust: 'untrusted',
        enabled: true,
        weight: 3,
        has_key: true,
        models: ['deepseek-chat'],
      },
      {
        name: 'legacy',
        base_url: 'http://10.0.0.9:8000',
        protocol: 'anthropic',
        trust: 'untrusted',
        enabled: false,
        weight: 1,
        has_key: false,
      },
    ],
  }
}

function send(res, status, body) {
  const text = JSON.stringify(body)
  res.writeHead(status, { 'Content-Type': 'application/json', 'Content-Length': Buffer.byteLength(text) })
  res.end(text)
}

function filteredAudit(params) {
  const actor = params.get('actor') ?? ''
  const channel = params.get('channel') ?? ''
  const model = params.get('model') ?? ''
  const decision = params.get('decision') ?? ''
  const verdict = params.get('verdict') ?? ''
  return auditRecords.filter(
    (r) =>
      (actor === '' || r.actor === actor) &&
      (channel === '' || r.channel === channel) &&
      (model === '' || r.model === model) &&
      (decision === '' || r.decision === decision) &&
      (verdict === '' || r.usage.verdict === verdict),
  )
}

const server = createServer((req, res) => {
  const url = new URL(req.url ?? '/', `http://127.0.0.1:${PORT}`)
  const path = url.pathname
  const params = url.searchParams

  // Optional credential gate, mirroring the gateway's server.admin_remote mode.
  // Run with MOCK_ADMIN_TOKEN=secret to develop the console's token prompt:
  //
  //   MOCK_ADMIN_TOKEN=secret node scripts/mock-admin.mjs
  //
  // Without a token the mock answers admin_unauthorized (401), which is what
  // the real listener does when remote access is on and the credential is
  // missing or wrong.
  const requiredToken = process.env.MOCK_ADMIN_TOKEN ?? ''
  if (requiredToken !== '') {
    const header = req.headers.authorization ?? ''
    const presented = header.startsWith('Bearer ') ? header.slice(7) : ''
    if (presented !== requiredToken) {
      res.setHeader('WWW-Authenticate', 'Bearer realm="customapi-admin"')
      return send(res, 401, {
        error: {
          message: 'the management API requires the server.admin_token credential',
          code: 'admin_unauthorized',
        },
      })
    }
  }

  // A deliberately slow 1x1 GIF: it gives headless screenshot tooling something
  // to wait on while the single-page app finishes its own fetches.
  if (path === '/admin/__slow') {
    const ms = Math.min(20000, Number(params.get('ms') ?? 2000))
    const gif = Buffer.from('R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7', 'base64')
    return setTimeout(() => {
      res.writeHead(200, { 'Content-Type': 'image/gif', 'Content-Length': gif.length })
      res.end(gif)
    }, ms)
  }

  if (path === '/admin/stats') return send(res, 200, {
    uptime_s: 98765,
    stats,
    channels: 3,
    users: users.length,
    audit_seq: 60,
    audit_head: 'f'.repeat(64),
    audit_path: '/var/db/customapi/audit.jsonl',
    strategy: 'value',
    outbound_mode: 'enforce',
    inbound_mode: 'enforce',
    fail_mode: 'closed',
  })

  if (path === '/admin/version') return send(res, 200, {
    version: 'v0.4.2-mock',
    commit: '9f2c1ab77de4510c9e2b3a4f5d6e7f8091a2b3c4',
    built: iso(86400000 * 3),
  })

  if (path === '/admin/healthz') return send(res, 200, {
    status: 'ok',
    version: 'v0.4.2-mock',
    uptime_s: 98765,
    channels: 3,
    users: users.length,
    outbound_rules: 6,
    inbound_rules: 7,
    fail_mode: 'closed',
    audit_seq: 60,
    stats,
  })

  if (path === '/admin/config') return send(res, 200, configPayload())

  if (path === '/admin/providers') return send(res, 200, {
    providers,
    direct_channels: [
      {
        channel: 'legacy',
        protocol: 'anthropic',
        has_key: false,
        enabled: false,
        weight: 1,
        model_count: 2,
        stale: true,
        discover_error: 'connection refused',
      },
    ],
    groups: ['premium', 'cheap'],
    protocols: [
      { id: 'openai', name: 'OpenAI Chat Completions', endpoint: '/v1/chat/completions' },
      { id: 'responses', name: 'OpenAI Responses', endpoint: '/v1/responses' },
      { id: 'anthropic', name: 'Anthropic Messages', endpoint: '/v1/messages' },
    ],
    routing: routingPayload,
  })

  if (path === '/admin/models' && req.method === 'GET') return send(res, 200, modelsPayload)

  if (path === '/admin/models/refresh' && req.method === 'POST') {
    return send(res, 200, { ok: true, models: modelsPayload.models.length })
  }

  if (path === '/admin/users' && req.method === 'GET') {
    return send(res, 200, { count: users.length, users })
  }

  if (path === '/admin/users' && req.method === 'POST') {
    let raw = ''
    req.on('data', (chunk) => {
      raw += chunk
    })
    req.on('end', () => {
      let body = {}
      try {
        body = JSON.parse(raw || '{}')
      } catch {
        return send(res, 400, { error: { message: 'could not parse request body', code: 'invalid_body' } })
      }
      const user = users.find((u) => u.name === body.user)
      if (!user) {
        return send(res, 404, {
          error: { message: `user ${body.user} not found`, code: 'user_error' },
        })
      }
      switch (body.action) {
        case 'grant':
        case 'set_quota':
          user.quota = Number(body.quota) || 0
          break
        case 'add_quota':
          user.quota = (user.quota ?? 0) + (Number(body.quota) || 0)
          break
        case 'suspend':
          user.enabled = false
          break
        case 'resume':
          user.enabled = true
          break
        case 'set_period':
          user.period = body.period
          break
        default:
          return send(res, 400, {
            error: { message: `action ${body.action} is not recognised`, code: 'unknown_action' },
          })
      }
      return send(res, 200, { ok: true, action: body.action, user })
    })
    return undefined
  }

  if (path === '/admin/usage') return send(res, 200, usagePayload())
  if (path === '/admin/prices') return send(res, 200, { prices })

  if (path === '/admin/prices/compare') {
    const model = params.get('model') ?? ''
    const input = Number(params.get('input_tokens') ?? 1000)
    const output = Number(params.get('output_tokens') ?? 500)
    const offers = [
      {
        channel: 'deepseek/main',
        currency: 'CNY',
        cost: 0.03,
        list_cost: 0.04,
        discount_pct: 25,
        unit_price: 20,
        promotions: [{ kind: 'off_peak', multiplier: 0.75, note: '低谷时段折扣' }],
        trust: 'untrusted',
      },
      {
        channel: 'openai/k1',
        currency: 'CNY',
        cost: 0.054,
        list_cost: 0.054,
        discount_pct: 0,
        unit_price: 36,
        promotions: [],
        trust: 'trusted',
      },
    ].sort((a, b) => a.cost - b.cost)
    return send(res, 200, {
      model,
      input_tokens: input,
      output_tokens: output,
      at: new Date(NOW).toISOString(),
      cheapest: offers[0].channel,
      offers,
    })
  }

  if (path === '/admin/scores') return send(res, 200, scoresPayload)

  if (path === '/admin/fingerprints') return send(res, 200, {
    fingerprints: [
      { channel: 'openai/k1', model: 'gpt-4o', checked: 12, mismatch: 0, mean_score: 0.05 },
      { channel: 'deepseek/main', model: 'deepseek-chat', checked: 40, mismatch: 9, mean_score: 0.41 },
    ],
  })

  if (path === '/admin/audit/verify') {
    if (params.get('broken') === 'true') {
      return send(res, 409, {
        ok: false,
        records: 60,
        head_seq: 60,
        head_hash: 'f'.repeat(64),
        violations: [
          { kind: 'hash_mismatch', line: 42, seq: 41, detail: 'entry_hash does not match recomputed hash' },
          { kind: 'seq_gap', line: 43, seq: 41, detail: 'expected seq 42, found 43' },
        ],
        anchors_checked: 4,
      })
    }
    return send(res, 200, {
      ok: true,
      records: 60,
      head_seq: 60,
      head_hash: 'f'.repeat(64),
      anchors_checked: 4,
      anchors_ignored: 1,
    })
  }

  if (path === '/admin/audit') {
    const all = filteredAudit(params)
    const limit = Math.min(1000, Number(params.get('limit') ?? 100))
    const offset = Number(params.get('offset') ?? 0)
    const page = all.slice(offset, offset + limit)
    const includeSamples = params.get('include_samples') === 'true'
    return send(res, 200, {
      records: page.map((r) =>
        includeSamples && r.decision === 'allow'
          ? { ...r, sample: { request_text: '{"messages":[{"role":"user","content":"<redacted>"}]}' } }
          : r,
      ),
      total: all.length,
      limit,
      offset,
    })
  }

  return send(res, 404, { error: { message: `unknown endpoint ${path}`, code: 'not_found' } })
})

server.listen(PORT, '127.0.0.1', () => {
  console.log(`mock admin API listening on http://127.0.0.1:${PORT}`)
})
