<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { useRoute } from 'vue-router'

import { api, errorText } from '../api'
import { useAsync } from '../async'
import Badge from '../components/Badge.vue'
import Panel from '../components/Panel.vue'
import StateBlock from '../components/StateBlock.vue'
import { dash, money, num, pct, ts, unitPrice } from '../format'
import { promotionKind, protocolTone, strategy, trust } from '../labels'
import type { PriceCompareResponse } from '../types'

const providers = useAsync(api.providers)
const catalogue = useAsync(api.models)

const routing = computed(() => providers.data.value?.routing ?? null)
const protocols = computed(() => providers.data.value?.protocols ?? [])

// --- price comparison -------------------------------------------------------

const model = ref('')
const inputTokens = ref(1000)
const outputTokens = ref(500)
const at = ref('')
const comparing = ref(false)
const compareError = ref('')
const result = ref<PriceCompareResponse | null>(null)

const route = useRoute()

// `/routing?model=gpt-4o` runs the comparison immediately, so a comparison is
// a shareable link; plain visits stay idle until the operator submits.
onMounted(() => {
  const raw = route.query.model
  const wanted = Array.isArray(raw) ? raw[0] : raw
  if (typeof wanted === 'string' && wanted.trim() !== '') {
    model.value = wanted.trim()
    void compare()
  }
})

const modelOptions = computed(() => {
  const names = (catalogue.data.value?.models ?? []).map((m) => m.model)
  return names.slice(0, 500)
})

const sortedOffers = computed(() => {
  const offers = result.value?.offers ?? []
  return [...offers].sort((a, b) => a.cost - b.cost)
})

const currency = computed(() => sortedOffers.value[0]?.currency ?? 'CNY')

const cheapestCost = computed(() => sortedOffers.value[0]?.cost ?? 0)

/** Extra over the cheapest offer, as a percentage. */
function premium(cost: number): number {
  if (cheapestCost.value <= 0) return 0
  return (cost - cheapestCost.value) / cheapestCost.value
}

async function compare(): Promise<void> {
  if (model.value.trim() === '') {
    compareError.value = '请先填写模型名称。'
    result.value = null
    return
  }
  comparing.value = true
  compareError.value = ''
  try {
    const atValue = at.value === '' ? undefined : new Date(at.value).toISOString()
    result.value = await api.comparePrices(
      model.value.trim(),
      Math.max(0, Math.trunc(inputTokens.value) || 0),
      Math.max(0, Math.trunc(outputTokens.value) || 0),
      atValue,
    )
  } catch (err) {
    compareError.value = errorText(err)
    result.value = null
  } finally {
    comparing.value = false
  }
}

function fillModelFromRow(name: string): void {
  model.value = name
}

// --- price catalogue --------------------------------------------------------

const priceFilter = ref('')
const prices = useAsync(api.prices)

const filteredPrices = computed(() => {
  const rows = prices.data.value?.prices ?? []
  const needle = priceFilter.value.trim().toLowerCase()
  if (needle === '') return rows.slice(0, 200)
  return rows
    .filter((r) => r.model.toLowerCase().includes(needle) || r.channel.toLowerCase().includes(needle))
    .slice(0, 200)
})
</script>

<template>
  <StateBlock
    :loading="providers.loading.value && providers.data.value === null"
    :error="providers.error.value"
    :empty="providers.data.value === null"
    empty-text="管理接口没有返回路由配置。"
    @retry="providers.load()"
  >
    <template v-if="providers.data.value && routing">
      <div class="grid grid-2">
        <Panel title="路由策略" hint="只读：由服务端配置决定">
          <dl class="kv">
            <dt>选择策略</dt>
            <dd>
              <Badge
                :text="strategy(routing.strategy).text"
                :tone="strategy(routing.strategy).tone"
                :title="routing.strategy"
              />
            </dd>
            <dt>同协议优先</dt>
            <dd>
              <Badge
                :text="routing.prefer_same_protocol ? '已开启' : '已关闭'"
                :tone="routing.prefer_same_protocol ? 'ok' : 'muted'"
              />
              <span class="small muted" style="margin-left: 8px">
                优先选择与客户端请求协议相同的渠道，避免转换带来的语义损失。
              </span>
            </dd>
            <dt>允许协议转换</dt>
            <dd>
              <Badge
                :text="routing.allow_translation ? '允许' : '禁止'"
                :tone="routing.allow_translation ? 'ok' : 'warn'"
              />
              <span class="small muted" style="margin-left: 8px">
                关闭后，只有原生支持该协议的渠道会被选中。
              </span>
            </dd>
            <dt>最大尝试次数</dt>
            <dd class="mono">{{ num(routing.max_attempts) }}</dd>
            <dt>重试条件</dt>
            <dd>
              <span class="tag-list">
                <Badge
                  v-for="r in routing.retry_on ?? []"
                  :key="r"
                  :text="r"
                  tone="muted"
                />
                <span v-if="(routing.retry_on ?? []).length === 0" class="muted">不重试</span>
              </span>
            </dd>
          </dl>
        </Panel>

        <Panel title="协议与转换" hint="网关支持的客户端协议">
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
                <tr v-for="p in protocols" :key="p.id">
                  <td>
                    <Badge :text="p.id" :tone="protocolTone(p.id)" />
                  </td>
                  <td>{{ p.name }}</td>
                  <td class="mono">{{ p.endpoint }}</td>
                </tr>
              </tbody>
            </table>
          </div>
          <div class="divider" />
          <div class="small muted">
            当客户端协议与渠道协议不一致时，网关会在请求与响应两个方向上做转换（审计记录里会标记
            <Badge text="已转换" tone="info" />）。若某渠道声明
            <Badge text="可翻译" tone="info" />，说明它可以接收其他协议的请求。
          </div>
        </Panel>
      </div>

      <Panel title="价格比较" hint="GET /admin/prices/compare：给定 token 组合，比较各渠道的实际成本">
        <div class="filters">
          <div class="field">
            <label for="cmp-model">模型</label>
            <input
              id="cmp-model"
              v-model="model"
              list="cmp-model-options"
              placeholder="例如 gpt-4o"
              style="width: 220px"
            />
            <datalist id="cmp-model-options">
              <option v-for="m in modelOptions" :key="m" :value="m" />
            </datalist>
          </div>
          <div class="field">
            <label for="cmp-in">输入 tokens</label>
            <input id="cmp-in" v-model.number="inputTokens" type="number" min="0" style="width: 120px" />
          </div>
          <div class="field">
            <label for="cmp-out">输出 tokens</label>
            <input id="cmp-out" v-model.number="outputTokens" type="number" min="0" style="width: 120px" />
          </div>
          <div class="field">
            <label for="cmp-at">时间（可选，用于低谷价）</label>
            <input id="cmp-at" v-model="at" type="datetime-local" />
          </div>
          <button class="btn btn-primary" type="button" :disabled="comparing" @click="compare">
            <span v-if="comparing" class="spinner" />{{ comparing ? '比价中…' : '开始比价' }}
          </button>
        </div>

        <div v-if="compareError" class="state error">{{ compareError }}</div>

        <div v-else-if="result" style="margin-top: 12px">
          <div class="row between" style="margin-bottom: 8px">
            <div class="small muted">
              模型 <span class="mono">{{ result.model || model }}</span> ·
              输入 {{ num(result.input_tokens) }} tokens · 输出 {{ num(result.output_tokens) }} tokens ·
              计价时点 {{ ts(result.at) }}
            </div>
            <div v-if="result.cheapest" class="small">
              最便宜渠道：
              <Badge :text="result.cheapest" tone="ok" />
            </div>
          </div>

          <StateBlock :empty="sortedOffers.length === 0" empty-text="没有渠道能为该模型报价。">
            <div class="table-wrap">
              <table class="data">
                <thead>
                  <tr>
                    <th>#</th>
                    <th>渠道</th>
                    <th>可信度</th>
                    <th>实际成本</th>
                    <th>标价成本</th>
                    <th>折扣</th>
                    <th>较最低价</th>
                    <th>折合单价 / 百万</th>
                    <th>生效促销</th>
                  </tr>
                </thead>
                <tbody>
                  <tr v-for="(offer, i) in sortedOffers" :key="offer.channel">
                    <td class="mono">{{ i + 1 }}</td>
                    <td class="mono">
                      {{ offer.channel }}
                      <Badge v-if="i === 0" text="最低价" tone="ok" />
                    </td>
                    <td>
                      <Badge :text="trust(offer.trust).text" :tone="trust(offer.trust).tone" />
                    </td>
                    <td class="mono">{{ money(offer.cost, offer.currency) }}</td>
                    <td class="mono muted">{{ money(offer.list_cost, offer.currency) }}</td>
                    <td class="mono">{{ pct(offer.discount_pct, 1, true) }}</td>
                    <td class="mono">{{ i === 0 ? '—' : `+${pct(premium(offer.cost))}` }}</td>
                    <td class="mono">{{ unitPrice(offer.unit_price, offer.currency) }}</td>
                    <td>
                      <span class="tag-list">
                        <Badge
                          v-for="(promo, pi) in offer.promotions ?? []"
                          :key="pi"
                          :text="`${promotionKind(promo.kind)} ×${promo.multiplier}`"
                          tone="info"
                          :title="promo.note ?? ''"
                        />
                        <span v-if="(offer.promotions ?? []).length === 0" class="muted">无</span>
                      </span>
                    </td>
                  </tr>
                </tbody>
              </table>
            </div>
            <div class="small muted" style="margin-top: 8px">
              合计参考：最低价 {{ money(cheapestCost, currency) }}；共 {{ num(sortedOffers.length) }} 个渠道报价。
            </div>
          </StateBlock>
        </div>
      </Panel>

      <Panel title="价格目录" hint="GET /admin/prices：每个渠道 + 模型的目录价">
        <template #actions>
          <input
            v-model="priceFilter"
            type="search"
            placeholder="筛选模型或渠道"
            style="width: 200px"
          />
          <button class="btn btn-sm" type="button" @click="prices.load()">重新加载</button>
        </template>
        <StateBlock
          :loading="prices.loading.value && prices.data.value === null"
          :error="prices.error.value"
          :empty="prices.data.value !== null && prices.data.value.prices.length === 0"
          empty-text="价格目录为空：没有为任何渠道配置价格。"
          @retry="prices.load()"
        >
          <div class="table-wrap">
            <table class="data compact">
              <thead>
                <tr>
                  <th>模型</th>
                  <th>渠道</th>
                  <th>可信度</th>
                  <th>输入 / 百万</th>
                  <th>输出 / 百万</th>
                  <th>有效单价</th>
                  <th>计价方式</th>
                  <th>来源</th>
                  <th></th>
                </tr>
              </thead>
              <tbody>
                <tr v-for="(row, i) in filteredPrices" :key="`${row.channel}|${row.model}|${i}`">
                  <td class="mono">{{ row.model }}</td>
                  <td class="mono">{{ row.channel }}</td>
                  <td>
                    <Badge :text="trust(row.trust).text" :tone="trust(row.trust).tone" />
                  </td>
                  <td class="mono">{{ unitPrice(row.input_per_m, row.currency) }}</td>
                  <td class="mono">{{ unitPrice(row.output_per_m, row.currency) }}</td>
                  <td class="mono">{{ unitPrice(row.effective_unit_price, row.currency) }}</td>
                  <td>{{ dash(row.scheme) }}</td>
                  <td class="small muted">{{ dash(row.source) }}</td>
                  <td>
                    <button
                      class="btn btn-sm"
                      type="button"
                      @click="
                        () => {
                          fillModelFromRow(row.model)
                          void compare()
                        }
                      "
                    >
                      比价
                    </button>
                  </td>
                </tr>
                <tr v-if="filteredPrices.length === 0">
                  <td colspan="9" class="muted">没有符合筛选条件的价格条目。</td>
                </tr>
              </tbody>
            </table>
          </div>
          <div class="small muted" style="margin-top: 8px">最多显示 200 行，可用筛选缩小范围。</div>
        </StateBlock>
      </Panel>
    </template>
  </StateBlock>
</template>
