<script setup lang="ts">
import { computed, ref } from 'vue'

import { api, errorText } from '../api'
import { useAsync } from '../async'
import Badge from '../components/Badge.vue'
import Panel from '../components/Panel.vue'
import StateBlock from '../components/StateBlock.vue'
import Tabs from '../components/Tabs.vue'
import type { TabDef } from '../components/Tabs.vue'
import { ageMs, compact, dash, num, olderThan, relative, ts, unitPrice } from '../format'
import { protocolName, protocolTone, source, trust } from '../labels'
import { useAppStore } from '../store'
import type { ChannelModels, ModelView } from '../types'

const store = useAppStore()
const models = useAsync(api.models)
const refreshing = ref(false)

const tab = ref('models')
const search = ref('')
const protocolFilter = ref('')
const kindFilter = ref('')
const expanded = ref<Record<string, boolean>>({})

/** A catalogue is stale once it is an hour old. */
const STALE_MS = 60 * 60 * 1000

const tabs = computed<TabDef[]>(() => [
  { id: 'models', label: '按模型', count: models.data.value?.models.length ?? 0 },
  { id: 'channels', label: '按渠道', count: models.data.value?.channels.length ?? 0 },
])

const updatedAtOld = computed(() => olderThan(models.data.value?.updated_at, STALE_MS))

const providerErrors = computed(() => {
  const errors = models.data.value?.errors ?? {}
  return Object.entries(errors).map(([provider, message]) => ({ provider, message }))
})

const allProtocols = computed(() => {
  const set = new Set<string>()
  for (const m of models.data.value?.models ?? []) {
    for (const p of m.protocols ?? []) set.add(p)
  }
  return [...set].sort()
})

const filteredModels = computed<ModelView[]>(() => {
  const list = models.data.value?.models ?? []
  const needle = search.value.trim().toLowerCase()
  return list.filter((m) => {
    if (needle !== '' && !m.model.toLowerCase().includes(needle)) return false
    if (protocolFilter.value !== '' && !(m.protocols ?? []).includes(protocolFilter.value)) {
      return false
    }
    if (kindFilter.value === 'configured' && !m.configured) return false
    if (kindFilter.value === 'discovered' && m.configured) return false
    return true
  })
})

const filteredChannels = computed<ChannelModels[]>(() => {
  const list = models.data.value?.channels ?? []
  const needle = search.value.trim().toLowerCase()
  return list.filter((c) => {
    if (needle === '' ) return true
    if (c.channel.toLowerCase().includes(needle)) return true
    return c.models.some((m) => m.model.toLowerCase().includes(needle))
  })
})

function toggle(model: string): void {
  expanded.value = { ...expanded.value, [model]: !expanded.value[model] }
}

/** A short preview of the channels serving a model, for the collapsed row. */
function channelPreview(m: ModelView): string {
  const names = m.channels.slice(0, 3).map((c) => c.channel)
  if (m.channels.length > 3) names.push(`…等 ${m.channels.length} 个`)
  return names.join('、')
}

async function refresh(): Promise<void> {
  refreshing.value = true
  try {
    const res = await api.refreshModels()
    store.notify(`模型目录已刷新，共 ${num(res.models)} 个模型。`, 'ok')
    await models.load()
  } catch (err) {
    store.notify(`刷新失败：${errorText(err)}`, 'error')
  } finally {
    refreshing.value = false
  }
}

const updatedAge = computed(() => {
  const ms = ageMs(models.data.value?.updated_at)
  if (!Number.isFinite(ms)) return '未知'
  const minutes = Math.floor(ms / 60000)
  if (minutes < 1) return '刚刚'
  if (minutes < 60) return `${minutes} 分钟前`
  const hours = Math.floor(minutes / 60)
  if (hours < 48) return `${hours} 小时前`
  return `${Math.floor(hours / 24)} 天前`
})
</script>

<template>
  <StateBlock
    :loading="models.loading.value && models.data.value === null"
    :error="models.error.value"
    :empty="models.data.value !== null && (models.data.value.models.length === 0 && models.data.value.channels.length === 0)"
    empty-text="模型目录为空：既没有手写配置的模型，也没有发现到任何模型。"
    @retry="models.load()"
  >
    <template v-if="models.data.value">
      <div class="banner" :class="updatedAtOld ? 'warn' : 'ok'">
        <span class="banner-icon">{{ updatedAtOld ? '▲' : '✔' }}</span>
        <div>
          <div class="banner-title">
            目录更新时间：{{ ts(models.data.value.updated_at) }}（{{ updatedAge }}）
          </div>
          <div class="banner-text">
            <template v-if="updatedAtOld">
              目录已经超过 1 小时未更新，模型列表可能不是最新的；可点击“刷新模型”重新发现。
            </template>
            <template v-else> 目录较新。共 {{ num(models.data.value.models.length) }} 个模型、{{ num(models.data.value.channels.length) }} 个渠道。 </template>
          </div>
        </div>
        <span class="spacer" />
        <button class="btn btn-primary" type="button" :disabled="refreshing" @click="refresh">
          <span v-if="refreshing" class="spinner" />{{ refreshing ? '刷新中…' : '刷新模型' }}
        </button>
      </div>

      <div v-if="providerErrors.length > 0" class="card">
        <header class="card-head">
          <h3>上游发现错误</h3>
          <span class="hint">{{ providerErrors.length }} 个提供商在最近一次发现中失败</span>
        </header>
        <div class="card-body tight">
          <div class="table-wrap">
            <table class="data compact">
              <thead>
                <tr>
                  <th>提供商</th>
                  <th>错误</th>
                </tr>
              </thead>
              <tbody>
                <tr v-for="e in providerErrors" :key="e.provider">
                  <td class="mono">{{ e.provider }}</td>
                  <td class="wrap">
                    <span class="badge danger">发现失败</span>
                    <span class="small">{{ e.message }}</span>
                  </td>
                </tr>
              </tbody>
            </table>
          </div>
        </div>
      </div>

      <Panel title="模型目录" hint="合并手写配置与自动发现的结果">
        <template #actions>
          <div class="tag-list">
            <Badge
              v-for="(count, provider) in models.data.value.providers"
              :key="provider"
              :text="`${provider}：${num(count)}`"
              tone="muted"
            />
          </div>
        </template>

        <div class="filters" style="margin-bottom: 10px">
          <div class="field">
            <label for="model-search">搜索模型 / 渠道</label>
            <input
              id="model-search"
              v-model="search"
              type="search"
              placeholder="例如 gpt-4o"
              style="width: 220px"
            />
          </div>
          <div class="field">
            <label for="model-protocol">协议</label>
            <select id="model-protocol" v-model="protocolFilter">
              <option value="">全部</option>
              <option v-for="p in allProtocols" :key="p" :value="p">{{ protocolName(p) }}</option>
            </select>
          </div>
          <div class="field">
            <label for="model-kind">来源</label>
            <select id="model-kind" v-model="kindFilter">
              <option value="">全部</option>
              <option value="configured">含手写配置</option>
              <option value="discovered">仅自动发现</option>
            </select>
          </div>
          <div class="small muted">
            命中 {{ tab === 'models' ? num(filteredModels.length) : num(filteredChannels.length) }} 项
          </div>
        </div>

        <Tabs v-model="tab" :tabs="tabs" />

        <div v-if="tab === 'models'" class="table-wrap">
          <table class="data">
            <thead>
              <tr>
                <th style="width: 28px"></th>
                <th>模型</th>
                <th>渠道数</th>
                <th>协议</th>
                <th>来源</th>
                <th>最便宜渠道</th>
                <th>单价 / 百万 token</th>
                <th>渠道</th>
              </tr>
            </thead>
            <tbody>
              <template v-for="m in filteredModels" :key="m.model">
                <tr class="expandable" :class="{ expanded: expanded[m.model] }" @click="toggle(m.model)">
                  <td class="muted">{{ expanded[m.model] ? '−' : '+' }}</td>
                  <td class="mono">{{ m.model }}</td>
                  <td class="mono">{{ num(m.channels.length) }}</td>
                  <td>
                    <span class="tag-list">
                      <Badge
                        v-for="p in m.protocols ?? []"
                        :key="p"
                        :text="protocolName(p)"
                        :tone="protocolTone(p)"
                      />
                      <span v-if="(m.protocols ?? []).length === 0" class="muted">—</span>
                    </span>
                  </td>
                  <td>
                    <Badge
                      :text="m.configured ? source('configured').text : source('discovered').text"
                      :tone="source(m.configured ? 'configured' : 'discovered').tone"
                    />
                  </td>
                  <td class="mono">{{ dash(m.cheapest) }}</td>
                  <td class="mono">
                    <span v-if="m.unit_price !== undefined">
                      {{ unitPrice(m.unit_price, m.currency) }}
                    </span>
                    <span v-else class="muted">—</span>
                  </td>
                  <td class="small muted">{{ channelPreview(m) }}</td>
                </tr>
                <tr v-if="expanded[m.model]" class="detail-row">
                  <td colspan="8">
                    <div class="section-title">服务该模型的所有渠道</div>
                    <table class="data compact">
                      <thead>
                        <tr>
                          <th>渠道</th>
                          <th>上游模型名</th>
                          <th>协议</th>
                          <th>来源</th>
                          <th>可信度</th>
                          <th>可翻译自</th>
                        </tr>
                      </thead>
                      <tbody>
                        <tr v-for="c in m.channels" :key="c.channel">
                          <td class="mono">{{ c.channel }}</td>
                          <td class="mono">{{ dash(c.upstream_model) }}</td>
                          <td>
                            <Badge
                              :text="protocolName(c.protocol)"
                              :tone="protocolTone(c.protocol)"
                            />
                          </td>
                          <td>
                            <Badge
                              :text="source(c.source).text"
                              :tone="source(c.source).tone"
                              :title="c.source"
                            />
                          </td>
                          <td>
                            <Badge
                              v-if="c.trust"
                              :text="trust(c.trust).text"
                              :tone="trust(c.trust).tone"
                            />
                            <span v-else class="muted">—</span>
                          </td>
                          <td class="small">
                            <span class="tag-list">
                              <Badge
                                v-for="p in c.translate_from ?? []"
                                :key="p"
                                :text="protocolName(p)"
                                tone="info"
                              />
                              <span v-if="(c.translate_from ?? []).length === 0" class="muted">—</span>
                            </span>
                          </td>
                        </tr>
                      </tbody>
                    </table>
                  </td>
                </tr>
              </template>
              <tr v-if="filteredModels.length === 0">
                <td colspan="8" class="muted">没有符合筛选条件的模型。</td>
              </tr>
            </tbody>
          </table>
        </div>

        <div v-else class="table-wrap">
          <table class="data">
            <thead>
              <tr>
                <th>渠道</th>
                <th>提供商</th>
                <th>密钥</th>
                <th>协议</th>
                <th>模型数</th>
                <th>最近发现</th>
                <th>状态</th>
                <th>错误</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="c in filteredChannels" :key="c.channel">
                <td class="mono">{{ c.channel }}</td>
                <td>{{ dash(c.provider) }}</td>
                <td class="mono">{{ dash(c.key) }}</td>
                <td>
                  <Badge :text="protocolName(c.protocol)" :tone="protocolTone(c.protocol)" />
                </td>
                <td class="mono">{{ num(c.models.length) }}</td>
                <td class="small nowrap">
                  {{ ts(c.discovered) }}
                  <span class="faint">（{{ relative(c.discovered) }}）</span>
                </td>
                <td>
                  <Badge v-if="c.error" text="发现失败" tone="danger" />
                  <Badge v-else-if="c.stale" text="已过期" tone="warn" />
                  <Badge v-else-if="c.configured_only" text="仅手写配置" tone="accent" />
                  <Badge v-else text="正常" tone="ok" />
                </td>
                <td class="wrap small">{{ dash(c.error) }}</td>
              </tr>
              <tr v-if="filteredChannels.length === 0">
                <td colspan="8" class="muted">没有符合筛选条件的渠道。</td>
              </tr>
            </tbody>
          </table>
        </div>

        <div v-if="tab === 'channels'" class="small muted" style="margin-top: 8px">
          共 {{ compact(models.data.value.models.length) }} 个模型分布在
          {{ num(models.data.value.channels.length) }} 个渠道上。
        </div>
      </Panel>
    </template>
  </StateBlock>
</template>
