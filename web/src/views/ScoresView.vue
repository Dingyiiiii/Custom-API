<script setup lang="ts">
import { computed, ref } from 'vue'

import { api } from '../api'
import { useAsync } from '../async'
import Badge from '../components/Badge.vue'
import BarMeter from '../components/BarMeter.vue'
import Panel from '../components/Panel.vue'
import StatCard from '../components/StatCard.vue'
import StateBlock from '../components/StateBlock.vue'
import { num, pct, ratio, relative, ts } from '../format'
import type { ScoreRow } from '../types'

const scores = useAsync(api.scores)
const fingerprints = useAsync(api.fingerprints)

const search = ref('')
const expanded = ref<Record<string, boolean>>({})

function key(row: ScoreRow): string {
  return `${row.channel}|${row.model}`
}

function toggle(row: ScoreRow): void {
  const k = key(row)
  expanded.value = { ...expanded.value, [k]: !expanded.value[k] }
}

const rows = computed(() => {
  const list = scores.data.value?.scores ?? []
  const needle = search.value.trim().toLowerCase()
  if (needle === '') return list
  return list.filter(
    (r) => r.channel.toLowerCase().includes(needle) || r.model.toLowerCase().includes(needle),
  )
})

const evidence = computed(() => {
  const list = fingerprints.data.value?.fingerprints ?? []
  const needle = search.value.trim().toLowerCase()
  if (needle === '') return list
  return list.filter(
    (r) => r.channel.toLowerCase().includes(needle) || r.model.toLowerCase().includes(needle),
  )
})

const summary = computed(() => {
  const list = scores.data.value?.scores ?? []
  if (list.length === 0) {
    return { capability: 0, reliability: 0, integrity: 0, fingerprint: 0, count: 0 }
  }
  let capability = 0
  let reliability = 0
  let integrity = 0
  let fingerprint = 0
  for (const r of list) {
    capability += r.scores.capability ?? 0
    reliability += r.scores.reliability ?? 0
    integrity += r.scores.usage_integrity ?? 0
    fingerprint += r.scores.fingerprint_match ?? 0
  }
  const n = list.length
  return {
    capability: capability / n,
    reliability: reliability / n,
    integrity: integrity / n,
    fingerprint: fingerprint / n,
    count: n,
  }
})

const mismatchTotal = computed(() =>
  (fingerprints.data.value?.fingerprints ?? []).reduce((sum, r) => sum + (r.mismatch ?? 0), 0),
)

const checkedTotal = computed(() =>
  (fingerprints.data.value?.fingerprints ?? []).reduce((sum, r) => sum + (r.checked ?? 0), 0),
)

function benchmarkEntries(row: ScoreRow): { name: string; value: number }[] {
  const map = row.scores.benchmarks ?? {}
  return Object.entries(map)
    .map(([name, value]) => ({ name, value }))
    .sort((a, b) => a.name.localeCompare(b.name))
}
</script>

<template>
  <StateBlock
    :loading="scores.loading.value && scores.data.value === null"
    :error="scores.error.value"
    :empty="scores.data.value === null"
    empty-text="管理接口没有返回跑分数据。"
    @retry="scores.load()"
  >
    <template v-if="scores.data.value">
      <div class="stats">
        <StatCard label="评分条目" :value="num(summary.count)" tone="accent" foot="渠道 × 模型" />
        <StatCard label="平均能力分" :value="pct(summary.capability)" />
        <StatCard label="平均可靠性" :value="pct(summary.reliability)" />
        <StatCard label="平均用量完整性" :value="pct(summary.integrity)" />
        <StatCard label="平均指纹匹配" :value="pct(summary.fingerprint)" />
        <StatCard
          label="指纹不符"
          :value="num(mismatchTotal)"
          :tone="mismatchTotal > 0 ? 'danger' : 'ok'"
          :foot="`共核验 ${num(checkedTotal)} 次`"
        />
      </div>

      <Panel title="渠道跑分" hint="来自路由评分簿：能力、指纹、可靠性、用量完整性">
        <template #actions>
          <input v-model="search" type="search" placeholder="筛选渠道或模型" style="width: 200px" />
          <button class="btn btn-sm" type="button" @click="scores.load()">刷新</button>
        </template>

        <StateBlock :empty="rows.length === 0" empty-text="还没有任何评分记录。">
          <div class="table-wrap">
            <table class="data">
              <thead>
                <tr>
                  <th style="width: 28px"></th>
                  <th>渠道</th>
                  <th>模型</th>
                  <th>能力</th>
                  <th>指纹匹配</th>
                  <th>可靠性</th>
                  <th>用量完整性</th>
                  <th>样本数</th>
                  <th>更新时间</th>
                </tr>
              </thead>
              <tbody>
                <template v-for="row in rows" :key="key(row)">
                  <tr
                    class="expandable"
                    :class="{ expanded: expanded[key(row)] }"
                    @click="toggle(row)"
                  >
                    <td class="muted">{{ expanded[key(row)] ? '−' : '+' }}</td>
                    <td class="mono">{{ row.channel }}</td>
                    <td class="mono">{{ row.model }}</td>
                    <td>
                      <BarMeter :value="row.scores.capability ?? 0" />
                    </td>
                    <td>
                      <BarMeter :value="row.scores.fingerprint_match ?? 0" />
                    </td>
                    <td>
                      <BarMeter :value="row.scores.reliability ?? 0" />
                    </td>
                    <td>
                      <BarMeter :value="row.scores.usage_integrity ?? 0" />
                    </td>
                    <td class="mono">{{ num(row.scores.samples) }}</td>
                    <td class="small nowrap">
                      {{ ts(row.scores.updated_at) }}
                      <span class="faint">（{{ relative(row.scores.updated_at) }}）</span>
                    </td>
                  </tr>
                  <tr v-if="expanded[key(row)]" class="detail-row">
                    <td colspan="9">
                      <div class="section-title">分项基准（benchmarks）</div>
                      <StateBlock
                        :empty="benchmarkEntries(row).length === 0"
                        empty-text="该条目没有分项基准数据。"
                      >
                        <div class="table-wrap">
                          <table class="data compact">
                            <thead>
                              <tr>
                                <th>基准</th>
                                <th>得分</th>
                              </tr>
                            </thead>
                            <tbody>
                              <tr v-for="b in benchmarkEntries(row)" :key="b.name">
                                <td class="mono">{{ b.name }}</td>
                                <td style="min-width: 220px">
                                  <BarMeter :value="b.value" />
                                </td>
                              </tr>
                            </tbody>
                          </table>
                        </div>
                      </StateBlock>
                    </td>
                  </tr>
                </template>
              </tbody>
            </table>
          </div>
        </StateBlock>
      </Panel>

      <Panel title="指纹证据" hint="GET /admin/fingerprints：模型身份探测的累计结果">
        <template #actions>
          <button class="btn btn-sm" type="button" @click="fingerprints.load()">刷新</button>
        </template>

        <StateBlock
          :loading="fingerprints.loading.value && fingerprints.data.value === null"
          :error="fingerprints.error.value"
          :empty="fingerprints.data.value !== null && evidence.length === 0"
          empty-text="还没有做过指纹核验（需要开启指纹探测）。"
          @retry="fingerprints.load()"
        >
          <div class="table-wrap">
            <table class="data compact">
              <thead>
                <tr>
                  <th>渠道</th>
                  <th>模型</th>
                  <th>核验次数</th>
                  <th>不符次数</th>
                  <th>不符率</th>
                  <th>平均得分</th>
                  <th>结论</th>
                </tr>
              </thead>
              <tbody>
                <tr v-for="row in evidence" :key="`${row.channel}|${row.model}`">
                  <td class="mono">{{ row.channel }}</td>
                  <td class="mono">{{ row.model }}</td>
                  <td class="mono">{{ num(row.checked) }}</td>
                  <td class="mono">{{ num(row.mismatch) }}</td>
                  <td>
                    <BarMeter
                      :value="ratio(row.mismatch, row.checked)"
                      :label="pct(ratio(row.mismatch, row.checked))"
                      :good="0.0001"
                      :bad="0.0001"
                      :tone="row.mismatch > 0 ? 'danger' : 'ok'"
                    />
                  </td>
                  <td class="mono">{{ row.mean_score.toFixed(4) }}</td>
                  <td>
                    <Badge
                      :text="row.mismatch > 0 ? '存在不符' : '全部匹配'"
                      :tone="row.mismatch > 0 ? 'danger' : 'ok'"
                    />
                  </td>
                </tr>
              </tbody>
            </table>
          </div>
          <div class="small muted" style="margin-top: 8px">
            平均得分越接近 0 表示与基线越一致；不符次数大于 0 说明该渠道可能并非声明的模型。
          </div>
        </StateBlock>
      </Panel>
    </template>
  </StateBlock>
</template>
