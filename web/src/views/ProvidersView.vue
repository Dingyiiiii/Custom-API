<script setup lang="ts">
import { computed, ref } from 'vue'

import { api, errorText } from '../api'
import { useAsync } from '../async'
import Badge from '../components/Badge.vue'
import BarMeter from '../components/BarMeter.vue'
import Panel from '../components/Panel.vue'
import StateBlock from '../components/StateBlock.vue'
import { compact, currencySymbol, dash, num, pct } from '../format'
import { protocolName, protocolTone, trust } from '../labels'
import { useAppStore } from '../store'
import type { ChannelHealth, Provider, ProviderChannel, ProviderKey } from '../types'

const store = useAppStore()
const providers = useAsync(api.providers)
const refreshing = ref(false)
const refreshNote = ref('')

/** One row of the per-provider channel table: key + discovery + health. */
interface ChannelRow {
  channel: string
  key: string
  hasKey: boolean
  enabled: boolean
  weight: number
  protocols: string[]
  protocol: string
  modelCount: number
  discovered: string
  stale: boolean
  discoverError: string
  health: ChannelHealth | null
}

interface ProviderCard {
  name: string
  baseUrl: string
  protocols: string[]
  trust: string
  enabled: boolean
  weight: number
  groups: string[]
  models: string[]
  translation: string[]
  channels: ChannelRow[]
}

function healthFor(provider: Provider, channel: string): ChannelHealth | null {
  return (provider.health ?? []).find((h) => h.channel === channel) ?? null
}

function toRow(ch: ProviderChannel, health: ChannelHealth | null): ChannelRow {
  return {
    channel: ch.channel,
    key: ch.key ?? '',
    hasKey: ch.has_key,
    enabled: ch.enabled,
    weight: ch.weight ?? 1,
    protocols: ch.protocols && ch.protocols.length > 0 ? ch.protocols : [ch.protocol],
    protocol: ch.protocol,
    modelCount: ch.model_count ?? 0,
    discovered: ch.discovered ?? '',
    stale: ch.stale === true,
    discoverError: ch.discover_error ?? '',
    health,
  }
}

/** channel name → key summary, for the prefix/limits/note columns. */
const keyByChannel = computed(() => {
  const map = new Map<string, ProviderKey>()
  const data = providers.data.value
  if (data === null) return map
  for (const p of data.providers) {
    for (const k of p.keys) {
      if (k.channel) map.set(k.channel, k)
      // The Go view always fills channel, but fall back to provider/key so an
      // older gateway still lines up.
      else map.set(`${p.name}/${k.name}`, k)
    }
  }
  return map
})

/** Per-channel health, flattened across providers. */
const healthByChannel = computed(() => {
  const map = new Map<string, ChannelHealth>()
  const data = providers.data.value
  if (data === null) return map
  for (const p of data.providers) {
    for (const h of p.health ?? []) map.set(h.channel, h)
  }
  return map
})

const cards = computed<ProviderCard[]>(() => {
  const data = providers.data.value
  if (data === null) return []
  return data.providers.map((p) => ({
    name: p.name,
    baseUrl: p.base_url,
    protocols: p.protocols && p.protocols.length > 0 ? p.protocols : [p.protocol],
    trust: p.trust,
    enabled: p.enabled,
    weight: p.weight,
    groups: p.groups ?? [],
    // The server sends the count as `models` and the names as `model_list`.
    models: p.model_list ?? [],
    translation: p.supports_translation ?? [],
    channels: p.channels.map((ch) => toRow(ch, healthFor(p, ch.channel))),
  }))
})

const directRows = computed<ChannelRow[]>(() => {
  const data = providers.data.value
  if (data === null) return []
  return data.direct_channels.map((ch) => toRow(ch, healthByChannel.value.get(ch.channel) ?? null))
})

async function refreshModels(): Promise<void> {
  refreshing.value = true
  refreshNote.value = ''
  try {
    const res = await api.refreshModels()
    refreshNote.value = `重新发现完成：目录中共 ${num(res.models)} 个模型。`
    store.notify(refreshNote.value, 'ok')
    await providers.load()
  } catch (err) {
    refreshNote.value = `刷新失败：${errorText(err)}`
    store.notify(refreshNote.value, 'error')
  } finally {
    refreshing.value = false
  }
}

function healthTone(health: ChannelHealth | null): 'ok' | 'warn' | 'danger' | 'muted' {
  if (health === null) return 'muted'
  if (!health.healthy) return 'danger'
  if ((health.failures ?? 0) > 0) return 'warn'
  return 'ok'
}

function healthText(health: ChannelHealth | null): string {
  if (health === null) return '无健康数据'
  const failures = health.failures ?? 0
  const base = health.healthy ? '健康' : '熔断中'
  return failures > 0 ? `${base} · 失败 ${failures}` : base
}

function prefixOf(row: ChannelRow): string {
  const key = keyByChannel.value.get(row.channel)
  if (key === undefined) return row.key !== '' ? row.key : '—'
  if (key.prefix && key.prefix !== '') return key.prefix
  return key.has_key ? '已配置（不显示）' : '未配置'
}

function keyEnabled(row: ChannelRow): boolean {
  return keyByChannel.value.get(row.channel)?.enabled ?? row.hasKey
}

function limitText(row: ChannelRow): string {
  const limits = keyByChannel.value.get(row.channel)?.limits
  if (limits === undefined) return '未设置'
  const parts: string[] = []
  if (limits.rpm) parts.push(`${num(limits.rpm)} RPM`)
  if (limits.tpm) parts.push(`${compact(limits.tpm)} TPM`)
  if (limits.monthly_budget) {
    parts.push(`预算 ${currencySymbol(limits.currency)}${num(limits.monthly_budget)}`)
  }
  return parts.length > 0 ? parts.join(' · ') : '未设置'
}

/** Discovery timestamps are shown date-only to keep the column narrow. */
function discoveredShort(value: string): string {
  if (value === '') return '—'
  return value.length >= 10 ? value.slice(0, 10) : value
}

function noteOf(row: ChannelRow): string {
  return keyByChannel.value.get(row.channel)?.note ?? ''
}
</script>

<template>
  <Panel title="模型发现" hint="POST /admin/models/refresh，向上游重新拉取模型列表">
    <template #actions>
      <div class="btn-row">
        <span v-if="refreshNote" class="small muted">{{ refreshNote }}</span>
        <button class="btn btn-primary" type="button" :disabled="refreshing" @click="refreshModels">
          <span v-if="refreshing" class="spinner" />{{ refreshing ? '刷新中…' : '刷新模型' }}
        </button>
      </div>
    </template>
    <div class="small muted">
      刷新会依次请求每个渠道的模型列表，通常需要数秒；期间界面保持可用，完成后本页数据会自动更新。
    </div>
  </Panel>

  <StateBlock
    :loading="providers.loading.value && providers.data.value === null"
    :error="providers.error.value"
    :empty="providers.data.value !== null && cards.length === 0"
    empty-text="尚未配置任何提供商。"
    @retry="providers.load()"
  >
    <template v-if="providers.data.value">
      <Panel
        v-for="card in cards"
        :key="card.name"
        :title="card.name"
        :hint="`${card.channels.length} 个渠道 · ${card.models.length} 个模型`"
      >
        <template #actions>
          <div class="tag-list">
            <Badge
              v-for="p in card.protocols"
              :key="p"
              :text="protocolName(p)"
              :tone="protocolTone(p)"
              :title="`协议 ${p}`"
            />
            <Badge
              v-if="card.translation.length > 0"
              :text="`可翻译：${card.translation.map(protocolName).join('、')}`"
              tone="info"
              title="网关可以把这些客户端协议转换成本提供商的协议"
            />
            <Badge :text="trust(card.trust).text" :tone="trust(card.trust).tone" />
            <Badge :text="card.enabled ? '已启用' : '已禁用'" :tone="card.enabled ? 'ok' : 'muted'" />
            <Badge :text="`权重 ${card.weight}`" tone="muted" />
          </div>
        </template>

        <div class="row between" style="margin-bottom: 10px">
          <div class="small muted break">
            上游地址：<span class="mono">{{ dash(card.baseUrl) }}</span>
          </div>
          <div class="tag-list">
            <Badge v-for="g in card.groups" :key="g" :text="`分组 ${g}`" tone="accent" />
            <Badge v-if="card.groups.length === 0" text="默认分组" tone="muted" />
          </div>
        </div>

        <div class="table-wrap">
          <table class="data compact">
            <thead>
              <tr>
                <th>密钥</th>
                <th>渠道</th>
                <th>协议</th>
                <th>权重</th>
                <th>限额</th>
                <th>健康 / 失败</th>
                <th>请求 / 错误</th>
                <th>用量完整性</th>
                <th>模型</th>
                <th>最近发现</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="row in card.channels" :key="row.channel">
                <td>
                  <div class="mono">{{ prefixOf(row) }}</div>
                  <div class="tag-list" style="margin-top: 2px">
                    <Badge
                      :text="keyEnabled(row) ? '启用' : '停用'"
                      :tone="keyEnabled(row) ? 'ok' : 'muted'"
                    />
                    <Badge v-if="!row.hasKey" text="无密钥" tone="warn" title="该渠道没有配置 API Key" />
                  </div>
                </td>
                <td class="mono">{{ row.channel }}</td>
                <td>
                  <span class="tag-list">
                    <Badge
                      v-for="p in row.protocols"
                      :key="p"
                      :text="protocolName(p)"
                      :tone="protocolTone(p)"
                    />
                  </span>
                </td>
                <td class="mono">{{ row.weight }}</td>
                <td class="small">{{ limitText(row) }}</td>
                <td>
                  <Badge
                    :text="healthText(row.health)"
                    :tone="healthTone(row.health)"
                    :title="row.health ? `熔断器状态：${row.health.healthy ? '闭合' : '打开'}` : ''"
                  />
                </td>
                <td class="mono">
                  {{ num(row.health?.requests) }} / {{ num(row.health?.errors) }}
                </td>
                <td>
                  <BarMeter
                    v-if="row.health"
                    :value="row.health.usage_integrity ?? 0"
                    :label="pct(row.health.usage_integrity)"
                  />
                  <span v-else class="muted">—</span>
                </td>
                <td class="mono">{{ num(row.modelCount) }}</td>
                <td class="small nowrap" :title="row.discovered">
                  <Badge v-if="row.discoverError" text="发现失败" tone="danger" :title="row.discoverError" />
                  <template v-else>
                    <Badge v-if="row.stale" text="已过期" tone="warn" />
                    {{ discoveredShort(row.discovered) }}
                  </template>
                </td>
              </tr>
              <tr v-if="card.channels.length === 0">
                <td colspan="10" class="muted">该提供商没有展开出任何渠道（可能没有可用密钥）。</td>
              </tr>
            </tbody>
          </table>
        </div>

        <div
          v-if="card.channels.some((c) => c.discoverError)"
          class="state error"
          style="text-align: left"
        >
          <div v-for="row in card.channels.filter((c) => c.discoverError)" :key="row.channel">
            发现错误（{{ row.channel }}）：{{ row.discoverError }}
          </div>
        </div>

        <div v-if="card.channels.some((c) => noteOf(c))" class="small muted" style="margin-top: 8px">
          <div v-for="row in card.channels.filter((c) => noteOf(c))" :key="row.channel">
            备注（{{ row.channel }}）：{{ noteOf(row) }}
          </div>
        </div>

        <details v-if="card.models.length > 0" class="raw">
          <summary>查看该提供商的 {{ card.models.length }} 个模型</summary>
          <div class="tag-list" style="margin-top: 8px">
            <Badge v-for="m in card.models" :key="m" :text="m" tone="muted" />
          </div>
        </details>
      </Panel>

      <Panel title="直连渠道" hint="没有提供商包装、直接写在配置里的渠道">
        <StateBlock :empty="directRows.length === 0" empty-text="没有直连渠道。">
          <div class="table-wrap">
            <table class="data compact">
              <thead>
                <tr>
                  <th>渠道</th>
                  <th>协议</th>
                  <th>密钥</th>
                  <th>启用</th>
                  <th>权重</th>
                  <th>模型数</th>
                  <th>状态</th>
                </tr>
              </thead>
              <tbody>
                <tr v-for="row in directRows" :key="row.channel">
                  <td class="mono">{{ row.channel }}</td>
                  <td>
                    <Badge :text="protocolName(row.protocol)" :tone="protocolTone(row.protocol)" />
                  </td>
                  <td>
                    <Badge :text="row.hasKey ? '已配置' : '未配置'" :tone="row.hasKey ? 'ok' : 'warn'" />
                  </td>
                  <td>
                    <Badge :text="row.enabled ? '启用' : '停用'" :tone="row.enabled ? 'ok' : 'muted'" />
                  </td>
                  <td class="mono">{{ row.weight }}</td>
                  <td class="mono">{{ num(row.modelCount) }}</td>
                  <td class="small">
                    <Badge
                      v-if="row.discoverError"
                      text="发现失败"
                      tone="danger"
                      :title="row.discoverError"
                    />
                    <Badge v-else-if="row.stale" text="列表已过期" tone="warn" />
                    <Badge v-else text="正常" tone="ok" />
                  </td>
                </tr>
              </tbody>
            </table>
          </div>
          <div v-if="directRows.some((r) => r.discoverError)" class="state error" style="text-align: left">
            <div v-for="row in directRows.filter((r) => r.discoverError)" :key="row.channel">
              {{ row.channel }}：{{ row.discoverError }}
            </div>
          </div>
        </StateBlock>
      </Panel>

      <Panel title="分组与协议" hint="路由使用的分组标签与网关支持的协议">
        <div class="grid grid-2">
          <div>
            <div class="section-title">分组</div>
            <StateBlock
              :empty="(providers.data.value.groups.length ?? 0) === 0"
              empty-text="没有定义分组。"
            >
              <div class="tag-list">
                <Badge v-for="g in providers.data.value.groups" :key="g" :text="g" tone="accent" />
              </div>
            </StateBlock>
          </div>
          <div>
            <div class="section-title">协议</div>
            <div class="table-wrap">
              <table class="data compact">
                <thead>
                  <tr>
                    <th>协议</th>
                    <th>名称</th>
                    <th>入口</th>
                  </tr>
                </thead>
                <tbody>
                  <tr v-for="p in providers.data.value.protocols" :key="p.id">
                    <td>
                      <Badge :text="p.id" :tone="protocolTone(p.id)" />
                    </td>
                    <td>{{ p.name }}</td>
                    <td class="mono">{{ p.endpoint }}</td>
                  </tr>
                </tbody>
              </table>
            </div>
          </div>
        </div>
      </Panel>
    </template>
  </StateBlock>
</template>
