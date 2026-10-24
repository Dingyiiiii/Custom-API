# 管理控制台前端（web/）

Custom-API 海关网关的运营控制台：Vue 3 + Vite + TypeScript 单页应用，深色主题，
全中文界面。**没有使用任何 UI 组件库或图表库**，表格、弹窗、标签页、进度条全部由
CSS / SVG 实现。

本目录位于 Rust 版仓库根目录下的 `web/`（Go 版对应 `src/customapi/web/`）。网关在
运行时从 `web/dist/` 读取构建产物，下面所有命令都在本目录执行。

## 构建与开发

```sh
cd web            # 本目录（仓库根下的 web/）
npm ci            # 或 npm install（使用 https://registry.npmmirror.com 镜像）
npm run dev       # 开发服务器 http://localhost:5173，/admin 反向代理到 127.0.0.1:8081
npm run typecheck # vue-tsc --noEmit
npm run build     # vue-tsc --noEmit && vite build -> dist/
npm run preview   # 预览已构建产物 http://localhost:5199
```

`npm run build` 会清空 `dist/`（`emptyOutDir: true`），随后由 `postbuild`
（`scripts/keep-gitkeep.mjs`）把 `dist/.gitkeep` 写回去，保证干净检出时目录依然存在。
`dist/.gitkeep` 与 `web/placeholder/` 都是网关在“尚未构建前端”时的兜底，勿删。

## 与后端的约定

- 控制台由**管理监听端口**（默认 `127.0.0.1:8081`）提供，因此所有接口都是同源相对
  路径 `/admin/...`，不需要鉴权头，也不依赖任何 CDN。
- `vite.config.ts` 使用 `base: '/'`，构建产物中的资源引用是绝对路径（`/assets/...`），
  对应网关把 `web/dist/` 挂在站点根目录；运行时不加载任何 CDN 资源。
- 路由使用 history 模式；网关对未知路径回落到 `index.html`，因此刷新深链接可用。
  路由守卫会把结尾的 `/` 去掉，避免同一页面出现 `/providers/` 这样的重复 URL。

## 无后端时的本地联调

`scripts/mock-admin.mjs` 是仅用于开发的假后端（不会被构建打包），按 Go 处理器真实
输出的 JSON 形状提供全部 `/admin/*` 接口：

```sh
node scripts/mock-admin.mjs        # 监听 127.0.0.1:8081
npm run dev                        # 或 npm run preview
```

## 目录结构

```
web/
├── index.html            入口 HTML（挂载 #app）
├── vite.config.ts        base '/'、outDir dist、/admin 代理
├── tsconfig.json         strict + noUnusedLocals，vue-tsc 校验
├── scripts/
│   ├── keep-gitkeep.mjs  构建后恢复 dist/.gitkeep
│   └── mock-admin.mjs    开发用假后端
└── src/
    ├── main.ts           应用入口（Pinia + Router）
    ├── App.vue           侧边栏 + 顶栏 + 路由出口
    ├── router.ts         路由表与侧边栏顺序（NAV）
    ├── api.ts            带类型的 fetch 客户端与 ApiError
    ├── types.ts          /admin/* 响应的接口定义
    ├── async.ts          loading / error / data 三态封装
    ├── format.ts         数字、金额、时间、时长格式化
    ├── labels.ts         枚举到中文标签与徽章配色
    ├── store.ts          全局状态：版本、健康度、轻提示
    ├── styles.css        深色主题与全部组件样式
    ├── components/       Badge、BarMeter、Modal、Panel、StateBlock、Tabs…
    └── views/            概览、提供商、模型、路由、用量、审计、用户、跑分、设置
```
