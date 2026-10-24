# 测试虚拟机（libvirt 托管 · 无头 · 串口控制台）

Custom-API 的测试环境是一台 **FreeBSD 15.1-RELEASE** 虚拟机，由 libvirt 严格托管。
它刻意做成**非特权**的：无宿主设备直通、用户态网络、专用网桥，并且可以直接从串口
控制台进入。

Rust 版的测试机多一项用途：`vm/create-vm.sh baseline` 会在 guest 内装入 `rust`
工具链，这样既可以在 FreeBSD 上原生 `cargo build` 验证，也让 `deploy.sh` 在宿主没有
FreeBSD 交叉工具链时有一条退路（见 `docs/DEPLOYMENT-zh.md` 第 2 节）。

---

## 1. 一键创建

```sh
./vm/create-vm.sh              # 定义网络与域、下载镜像、启动、等待 SSH
./vm/create-vm.sh baseline     # 在 guest 内安装 curl/jq/ca_root_nss/sudo/python3/rust/git
./vm/create-vm.sh status       # 查看域/网络/地址/串口设备
```

等价于 `make vm`、`make vm-baseline`、`make vm-status`。

脚本做的事：

| 步骤 | 说明 |
|---|---|
| 网络 | 定义 NAT 网络 `customapi-net`（`10.77.30.0/24`，网桥 `virbr-capi`），为虚拟机固定 DHCP 地址 `10.77.30.31` |
| 镜像 | 从 **USTC 镜像**下载 `FreeBSD-15.1-RELEASE-amd64-BASIC-CLOUDINIT-ufs.qcow2.xz`，`xz -t` 校验完整性后解压 |
| 磁盘 | 基于基础镜像创建 60 GiB 覆盖层；`growfs_enable=YES` 在首次启动时把 UFS 根分区扩到整盘 |
| cloud-init（`nuageinit`） | 生成 `CIDATA` 标签的 seed ISO，创建 `agent` 用户（`wheel` + `operator`，`sudo -n` 免密）、注入 SSH 公钥、启用 sshd、配置 USTC pkg 镜像与国内 DNS |
| 域 | 定义 4 vCPU / 4096 MiB / virtio-blk / virtio-net / 串口控制台的无头域 |
| 访问 | 写入 `~/.ssh/config` 别名 `customapi-bsd` |

镜像与 seed 落在 **`/var/lib/libvirt/images/customapi/`**，而不是工作区内。
这不是随意选择：QEMU 以非特权用户 `qemu` 运行，无法穿越 `0700` 的家目录，
放在工作区里会导致 `无法访问存储文件...权限不够`。

---

## 2. 进入虚拟机

### 串口控制台（推荐）

```sh
virsh -c qemu:///system console customapi-freebsd
# 退出：Ctrl+]
```

控制台在 `ttyu0`。镜像默认已在 `/boot/loader.conf` 里设置
`console="comconsole,vidconsole"`，因此内核启动信息与登录提示都会出现在串口上。
**如果控制台一片空白，先按一次回车**——`getty` 只在有输入时才输出提示符。

> 两个容易误判为故障的行为：**空闲的控制台不会有任何输出**，所以对已启动且空闲的
> guest 执行 `read` 返回空是正常的——先发一个换行；另外**单次读写可能错过回显**，
> 重复发送换行并等待数秒最为可靠。

辅助脚本（无需 TTY，适合自动化）：

```sh
vm/bin/vmctl.sh attach          # 把串口 pty 桥接成 unix socket（socat）
vm/bin/vmctl.sh read 5          # 读 5 秒
vm/bin/vmctl.sh send 'ls\n'     # 发送
vm/bin/vmctl.sh ip              # 取 IP
```

`vm/bin/vmconsole.py` 是等价的 Python 实现（直接操作 pty，带 termios 原始模式），
用法见 `vm/bin/README.md`。

### SSH

```sh
ssh customapi-bsd               # 别名指向 agent@10.77.30.31
```

`agent` 在 `wheel` 组内，`sudo -n` 免密。

> 首次创建后如果 SSH 报 `Permission denied (publickey)`，说明 cloud-init 还没跑完；
> 串口控制台可以立刻用，等 1–2 分钟再试 SSH。

---

## 3. 日常操作

```sh
./vm/create-vm.sh start | stop | status | console | ssh

virsh -c qemu:///system list --all
virsh -c qemu:///system domifaddr customapi-freebsd --source lease
virsh -c qemu:///system net-list --all
```

### 重建 / 清理

```sh
./vm/create-vm.sh destroy   # 解除域定义，保留镜像
./vm/create-vm.sh purge     # 同时删除工作磁盘与 seed（保留基础镜像与 .xz）
```

`create-vm.sh` 是幂等的：重复执行会解除旧域定义、重建磁盘覆盖层并重启，
因此可以随时把虚拟机恢复到干净状态。

---

## 4. 虚拟机规格

| 项目 | 值 | 理由 |
|---|---|---|
| 域类型 | `kvm` | 宿主提供 `/dev/kvm` 时使用硬件加速 |
| vCPU / 内存 | 4 / 4096 MiB | 与目标机（4C/8G）同量级；留出余量给宿主 |
| 磁盘 | 60 GiB qcow2 覆盖层 | Rust 工具链来自 pkg；真正占空间的是 `target/`，本仓库调试构建实测 3.4 GiB，release 构建再加数 GiB |
| 网络 | `customapi-net` NAT，固定 `10.77.30.31` | 与宿主上已有的 `openwrt-lan`（`192.168.122.0/24`）隔离，避免冲突 |
| 显示 | 无（`--console` 串口） | 无头运行 |
| 特权 | 无设备直通、无 `hostdev`、用户态网络 | 非特权测试环境 |
| 随机源 | virtio-rng ← `/dev/urandom` | 避免虚拟化下熵不足拖慢 TLS 与密钥生成 |
| CPU | `host-passthrough` | guest 直接看到宿主 CPU 特性，避免虚拟 CPU 模型缺指令导致构建/运行差异 |

### 宿主机防火墙

guest 需要访问宿主上的临时 HTTP 服务来做首次引导，因此创建脚本会确保
`libvirt` 防火墙区域放通 `tcp/8899`。缺少这一步时的症状很典型：
**能 ping 通网桥，但 HTTP 连接被拒**（因为 firewalld 的 zone 规则拦在 INPUT 上）。

---

## 5. 在虚拟机里构建与验证

**发布二进制由宿主交叉编译**（更快且可复现），虚拟机内的工具链用于验证
「在真实 FreeBSD 上能编译、能跑」这件事：

```sh
# guest 内原生构建（baseline 已装 rust/cargo）
ssh customapi-bsd 'pkg info -e rust && rustc --version && cargo --version'
ssh customapi-bsd 'cd /path/to/Custom-API && cargo build --release -p customsd -p benchd -p customsctl'

# 宿主侧：交叉编译 + 交付 + 验收（常规路径）
./deploy/scripts/deploy.sh build
./deploy/scripts/deploy.sh all
```

两点需要知道：

- guest 内没有任何 crates 镜像配置，`cargo` 会直接访问 crates.io。国内网络下如需加速，
  在 guest 的 `/root/.cargo/config.toml` 里配置源替换（这是 cargo 配置项，不是环境变量），
  或者干脆把源码拷进去之前先在宿主跑一次 `cargo fetch`。
- 交叉构建失败时，`deploy.sh build` 会退回到宿主原生构建并**明确警告**
  "the binaries will NOT run on FreeBSD"；此时真正可行的做法是在 guest 内原生构建
  （见 `docs/DEPLOYMENT-zh.md` 2.0）。

**所有下载都走国内镜像**：pkg 走 USTC（`/usr/local/etc/pkg/repos/ustc.conf`），
FreeBSD 镜像走 USTC，DNS 为 `223.5.5.5` / `119.29.29.29`（由 seed 写入）。

---

## 6. 故障排查

| 症状 | 原因 | 处理 |
|---|---|---|
| `virsh start` 报 `无法访问存储文件...权限不够` | 镜像放在 `0700` 的家目录下 | 把镜像放到 `/var/lib/libvirt/images/`，属主 `qemu:qemu` |
| 串口控制台无输出 | `getty` 只在有输入时输出 | 按一次回车 |
| `virsh console` 报 `没有控制的 TTY 则不能运行互动控制台` | 需要 TTY | 用 `vm/bin/vmctl.sh` / `vm/bin/vmconsole.py`，或真正的终端 |
| SSH 被拒 | cloud-init 未完成，或公钥未注入 | 用串口控制台确认；检查 `~/.ssh/id_ed25519.pub` 是否为创建时使用的密钥 |
| guest 能 ping 但不能 HTTP | 宿主 firewalld 拦 INPUT | `firewall-cmd --zone=libvirt --add-port=8899/tcp`（脚本会自动做） |
| `pkg install` 极慢或失败 | 未走国内镜像 | 检查 `/usr/local/etc/pkg/repos/ustc.conf` 是否存在且 `FreeBSD.conf` 为 `enabled: no` |
| `cargo build` 卡在下载依赖 | guest 直连 crates.io | 配置 `/root/.cargo/config.toml` 源替换，或在宿主 `cargo fetch` 后拷贝 `~/.cargo/registry` |
| 磁盘空间不足 | 覆盖层未扩容 | `virsh blockresize` 或直接重建（`create-vm.sh` 会创建 60 GiB 磁盘） |

---

## 7. 关于硬件加速

域定义为 `type='kvm'` 并使用 `host-passthrough` CPU。可以这样确认它确实在用
硬件加速而不是纯软件模拟（TCG）：

```sh
virsh -c qemu:///system dumpxml customapi-freebsd | grep "domain type"
tr '\0' '\n' < /proc/$(pgrep -f 'qemu-system.*customapi' | head -1)/cmdline | grep -A1 accel
# 期望看到：-accel / kvm
```

**Go 版实测：本项目开发环境以 KVM 运行**（4 vCPU），一次完整 FreeBSD 验收测试
（89 项，含多次进程启停）耗时约 20 秒，Go 原生构建也在秒级完成。
（Go 版记录，Rust 版待复核：Rust release 构建含 thin LTO，冷构建时间会长一些。）

宿主未暴露 `/dev/kvm` 时（例如在受限容器中）虚拟机会以 TCG 纯软件模拟启动，
症状是 guest 明显变慢、首次启动要数分钟。此时可改写域定义：

```sh
sed -i "s|<domain type='kvm'>|<domain type='qemu'>|; s|host-passthrough|qemu64|" \
  vm/customapi-freebsd.xml
virsh -c qemu:///system define vm/customapi-freebsd.xml
```

功能验证在 TCG 下同样可用，只是每一步都要耐心一些。
