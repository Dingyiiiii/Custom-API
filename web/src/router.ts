import { createRouter, createWebHistory } from 'vue-router'
import type { RouteRecordRaw } from 'vue-router'

import OverviewView from './views/OverviewView.vue'
import ProvidersView from './views/ProvidersView.vue'
import ModelsView from './views/ModelsView.vue'
import RoutingView from './views/RoutingView.vue'
import UsageView from './views/UsageView.vue'
import AuditView from './views/AuditView.vue'
import UsersView from './views/UsersView.vue'
import ScoresView from './views/ScoresView.vue'
import SettingsView from './views/SettingsView.vue'

export interface NavEntry {
  path: string
  title: string
  icon: string
  /** One-line description shown under the page title. */
  hint: string
}

/** Sidebar order, also used to title the top bar. */
export const NAV: NavEntry[] = [
  { path: '/', title: '概览', icon: '◈', hint: '运行状态、计数与审计链完整性' },
  { path: '/providers', title: '提供商', icon: '◉', hint: '提供商、密钥、渠道健康度与模型发现' },
  { path: '/models', title: '模型', icon: '☰', hint: '合并后的模型目录与每渠道发现结果' },
  { path: '/routing', title: '路由', icon: '⇄', hint: '路由策略、协议与价格比较' },
  { path: '/usage', title: '用量', icon: '▤', hint: '按模型、渠道、用户聚合的用量与成本' },
  { path: '/audit', title: '审计', icon: '⛨', hint: '审计账本、哈希链校验与单条记录详情' },
  { path: '/users', title: '用户', icon: '☺', hint: '配额、消费与账号状态' },
  { path: '/scores', title: '跑分', icon: '★', hint: '能力、指纹匹配、可靠性与用量完整性' },
  { path: '/settings', title: '设置', icon: '⚙', hint: '只读配置摘要与版本信息（密钥已脱敏）' },
]

const routes: RouteRecordRaw[] = [
  { path: '/', name: 'overview', component: OverviewView },
  { path: '/providers', name: 'providers', component: ProvidersView },
  { path: '/models', name: 'models', component: ModelsView },
  { path: '/routing', name: 'routing', component: RoutingView },
  { path: '/usage', name: 'usage', component: UsageView },
  { path: '/audit', name: 'audit', component: AuditView },
  { path: '/users', name: 'users', component: UsersView },
  { path: '/scores', name: 'scores', component: ScoresView },
  { path: '/settings', name: 'settings', component: SettingsView },
  { path: '/:pathMatch(.*)*', redirect: '/' },
]

export const router = createRouter({
  // The gateway serves dist/ at the site root and falls back to index.html for
  // unknown paths, so history mode works on refresh and on deep links.
  history: createWebHistory(),
  routes,
})

// A trailing slash would make the browser resolve the relative asset URLs in
// index.html against a subdirectory, which turns a refresh into a blank page.
// Normalising it here keeps `base: './'` safe.
router.beforeEach((to) => {
  if (to.fullPath.length > 1 && to.path.endsWith('/')) {
    return { path: to.path.replace(/\/+$/, ''), query: to.query, hash: to.hash, replace: true }
  }
  return true
})
