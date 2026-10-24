<script setup lang="ts">
import { computed, ref } from 'vue'

import { api } from '../api'
import { useAsync } from '../async'
import Badge from '../components/Badge.vue'
import BarMeter from '../components/BarMeter.vue'
import Panel from '../components/Panel.vue'
import StatCard from '../components/StatCard.vue'
import StateBlock from '../components/StateBlock.vue'
import Tabs from '../components/Tabs.vue'
import type { TabDef } from '../components/Tabs.vue'
import { compact, millis, money, num, pct, ratio, ts } from '../format'
import { period as periodLabel, trust, verdict } from '../labels'
import type { UserState } from '../types'

const usage = useAsync(api.usage)
const tab = ref('models')
const search = ref('')

const tabs = computed<TabDef[]>(() => [
  { id: 'models', label: '模型', count: usage.data.value?.models.length ?? 0 },
  { id: 'channels', label: '渠道', count: usage.data.value?.channels.length ?? 0 },
  { id: 'users', label: '用户', count: usage.data.value?.users.length ?? 0 },
])

function matches(...fields: string[]): boolean {
  const needle = search.value.trim().toLowerCase()
  if (needle === '') return true
  return fields.some((f) => f.toLowerCase().includes(needle))
}

const models = computed(() => (usage.data.value?.models ?? []).filter((m) => matches(m.model)))
const channels = computed(() =>
  (usage.data.value?.channels ?? []).filter((c) => matches(c.channel)),
)
const users = computed(() => (usage.data.value?.users ?? []).filter((u) => matches(u.name)))

const totals = computed(() => {
  const data = usage.data.value
  if (data === null) return { requests: 0, cost: 0, tokens: 0, currency: 'CNY', blocked: 0 }
  let requests = 0
  let cost = 0
  let tokens = 0
  let blocked = 0
  for (const m of data.models) {
    requests += m.requests ?? 0
    cost += m.cost ?? 0
    tokens += m.total_tokens ?? 0
    blocked += m.blocked ?? 0
  }
  return { requests, cost, tokens, currency: data.models[0]?.currency ?? 'CNY', blocked }
})

/** Remaining quota, or -1 for unlimited (0 quota means unlimited). */
function remaining(user: UserState): number {
  const quota = user.quota ?? 0
  if (quota <= 0) return -1
  return Math.max(0, quota - (user.consumed ?? 0))
}

function quotaText(user: UserState): string {
  const quota = user.quota ?? 0
  if (quota <= 0) return '不限'
  return num(quota)
}

function consumedRatio(user: UserState): number {
  const quota = user.quota ?? 0
  if (quota <= 0) return 0
  return ratio(user.consumed, quota)
}
</script>

<template>
  <StateBlock
    :loading="usage.loading.value && usage.data.value === null"
    :error="usage.error.value"
    :empty="usage.data.value === null"
    empty-text="管理接口没有返回用量数据。"
    @retry="usage.load()"
  >
    <template v-if="usage.data.value">
      <div class="stats">
        <StatCard
          label="审计记录"
          :value="num(usage.data.value.records)"
          tone="accent"
          foot="聚合的数据来源"
        />
        <StatCard label="请求次数" :value="num(totals.requests)" />
        <StatCard label="总 token" :value="compact(totals.tokens)" :foot="num(totals.tokens)" />
        <StatCard label="总成本" :value="money(totals.cost, totals.currency)" />
        <StatCard
          label="被拦截"
          :value="num(totals.blocked)"
          :tone="totals.blocked > 0 ? 'warn' : 'muted'"
        />
      </div>

      <Panel title="用量聚合" hint="数据来自审计账本，可随时从记录重算">
        <template #actions>
          <input v-model="search" type="search" placeholder="筛选名称" style="width: 180px" />
          <button class="btn btn-sm" type="button" @click="usage.load()">重新加载</button>
        </template>

        <Tabs v-model="tab" :tabs="tabs" />

        <div v-if="tab === 'models'" class="table-wrap">
          <table class="data">
            <thead>
              <tr>
                <th>模型</th>
                <th>请求</th>
                <th>输入 tokens</th>
                <th>输出 tokens</th>
                <th>总 tokens</th>
                <th>成本</th>
                <th>被拦截</th>
                <th>超收次数</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="m in models" :key="m.model">
                <td class="mono">{{ m.model }}</td>
                <td class="mono">{{ num(m.requests) }}</td>
                <td class="mono">{{ num(m.prompt_tokens) }}</td>
                <td class="mono">{{ num(m.completion_tokens) }}</td>
                <td class="mono">{{ compact(m.total_tokens) }}</td>
                <td class="mono">{{ money(m.cost, m.currency) }}</td>
                <td class="mono">{{ num(m.blocked) }}</td>
                <td>
                  <Badge
                    v-if="(m.over_billed ?? 0) > 0"
                    :text="num(m.over_billed)"
                    tone="danger"
                  />
                  <span v-else class="muted">0</span>
                </td>
              </tr>
              <tr v-if="models.length === 0">
                <td colspan="8" class="muted">没有符合筛选条件的模型用量。</td>
              </tr>
            </tbody>
          </table>
        </div>

        <div v-else-if="tab === 'channels'" class="table-wrap">
          <table class="data">
            <thead>
              <tr>
                <th>渠道</th>
                <th>请求</th>
                <th>失败</th>
                <th>被拦截</th>
                <th>总 tokens</th>
                <th>成本</th>
                <th>用量完整性</th>
                <th>可核验次数</th>
                <th>平均延迟</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="c in channels" :key="c.channel">
                <td class="mono">{{ c.channel }}</td>
                <td class="mono">{{ num(c.requests) }}</td>
                <td class="mono">{{ num(c.failures) }}</td>
                <td class="mono">{{ num(c.blocked) }}</td>
                <td class="mono">{{ compact(c.total_tokens) }}</td>
                <td class="mono">{{ money(c.cost, c.currency) }}</td>
                <td>
                  <BarMeter
                    :value="c.usage_integrity ?? 0"
                    :label="pct(c.usage_integrity)"
                    :good="0.95"
                    :bad="0.8"
                  />
                </td>
                <td class="mono">{{ num(c.verifiable) }}</td>
                <td class="mono">{{ millis(c.mean_latency_ms) }}</td>
              </tr>
              <tr v-if="channels.length === 0">
                <td colspan="9" class="muted">没有符合筛选条件的渠道用量。</td>
              </tr>
            </tbody>
          </table>
        </div>

        <div v-else class="table-wrap">
          <table class="data compact">
            <thead>
              <tr>
                <th>用户</th>
                <th>周期</th>
                <th>配额</th>
                <th>已用</th>
                <th>剩余</th>
                <th>使用率</th>
                <th>请求</th>
                <th>花费</th>
                <th>最近活动</th>
                <th>状态</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="u in users" :key="u.name">
                <td>
                  <div class="mono">{{ u.name }}</div>
                  <div class="tag-list" style="margin-top: 2px">
                    <Badge :text="trust(u.trust).text" :tone="trust(u.trust).tone" />
                  </div>
                </td>
                <td>
                  <Badge :text="periodLabel(u.period).text" :tone="periodLabel(u.period).tone" />
                </td>
                <td class="mono">{{ quotaText(u) }}</td>
                <td class="mono" :title="`累计消费 ${num(u.lifetime_consumed)}`">
                  {{ compact(u.consumed) }}
                </td>
                <td class="mono">
                  <span v-if="remaining(u) < 0" class="badge muted">不限</span>
                  <span v-else>{{ compact(remaining(u)) }}</span>
                </td>
                <td>
                  <BarMeter
                    v-if="(u.quota ?? 0) > 0"
                    :value="consumedRatio(u)"
                    :label="pct(consumedRatio(u))"
                  />
                  <span v-else class="muted">不限</span>
                </td>
                <td class="mono">{{ num(u.requests) }}</td>
                <td class="mono">{{ money(u.spend, u.currency) }}</td>
                <td class="small nowrap">{{ ts(u.last_seen) }}</td>
                <td>
                  <Badge :text="u.enabled ? '启用' : '暂停'" :tone="u.enabled ? 'ok' : 'warn'" />
                </td>
              </tr>
              <tr v-if="users.length === 0">
                <td colspan="10" class="muted">没有符合筛选条件的用户。</td>
              </tr>
            </tbody>
          </table>
        </div>

        <div class="small muted" style="margin-top: 8px">
          用量判定口径：{{ verdict('consistent').text }} 表示上游上报与本地统计一致；
          {{ verdict('over_billed').text }} 表示上游多报，会计入风险统计。
        </div>
      </Panel>
    </template>
  </StateBlock>
</template>
