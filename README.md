# lich13-switch

Codex、Claude Code、Grok Build、DSH 与 Antigravity CLI 独立供应商管理、本地 API 网关，兼具 Codex 账号切换、TOML 编辑及 Claude 可视化配置，支持 macOS 和 Windows。Tauri 2 + Rust + React。首次启动网关均默认关闭；之后启动时按上次启用意图恢复，可设置登录静默启动。

<img src="assets/icon.svg" alt="Prism Relay icon" width="100" />

- 菜单栏／托盘左键打开供应商／账号快捷面板，右键保留精简原生菜单。
- 添加 ChatGPT 登录或导入现有凭据；设备码登录支持复制链接和设备码，同一身份的 Token 刷新自动回存。
- 添加供应商时可填写显示名称，留空使用 endpoint 主机名；CC Switch 导入可选择平台及兼容协议，保留链接字段。
- Codex／Grok TOML 原文与 Claude 可视化／JSON 配置，共用草稿，支持校验、冲突保护和恢复上次配置。
- Claude 官方／API 全局配置独立保存，可从官方配置启动 CLI 登录；不替换官方 OAuth 凭据。
- 跟随系统深浅主题，关闭窗口后驻留；不会自动重启 Codex。
- 设置页和托盘菜单提供检查更新与 GitHub 入口。

**凭据保存在本机文件中，未加密。请像保护原客户端凭据一样保护这些文件。官方／API 配置分开保存不保证避免账号封禁。**

## 安装

从 [GitHub Releases](https://github.com/lich13/lich13-switch/releases) 下载 macOS Apple Silicon DMG 或 Windows x64 安装程序。macOS 包使用 ad-hoc 签名，Windows 包未配置商业代码签名；系统可能要求确认来自非商店开发者的应用。校验下载文件时使用 Release 中的 `SHA256SUMS`。

版本变化见 [发布说明](RELEASE.md)，数据范围和网络访问见 [隐私与安全](PRIVACY.md)。

## 基本使用

1. 在“网关”选择客户端，添加供应商，按需选择手动／自动模式和队列；首次启动网关默认关闭。
2. 启用网关后按提示重新打开对应客户端；Antigravity CLI 使用“启动 Antigravity”选择模型与工作目录。运行中切换只影响新请求，已有连接保持原供应商。
3. Codex 账号在“账号”页登录、导入、切换或导出；配置页保存前校验并保留未保存草稿。
4. 托盘左键打开快捷面板；“用量”查看采集记录与估算费用，“日志”查看异常与熔断原因。

详细操作：[面板、额度与系统控制](docs/controls.md)、[网关调度、白名单及并发](docs/gateway.md)、[用量与计价口径](docs/usage.md)。

## 各客户端说明

| 客户端 | 默认本地入口 | 配置与认证 |
| --- | --- | --- |
| Codex | `127.0.0.1:43721/v1` | Bearer 两字段或普通 API 文件事务；官方账号独立管理 |
| Claude Code | `127.0.0.1:43721` | `settings.json` 的连接字段；官方／API 配置档与可视化编辑 |
| Grok Build | `127.0.0.1:43721/v1` | 独立 API 模型与原生官方模型并存；OAuth 继续由 Grok 登录和刷新 |
| DSH | `127.0.0.1:43721/v1` | Chat Completions；profile 接入与模型映射 |
| Antigravity CLI | `127.0.0.1:43721` | Gemini Native；固定独立目录、模型选择与终端启动 |

五个客户端共享监听端口，通过各自的本地访问令牌分流，供应商和调度状态各自隔离。旧默认端口在成功绑定新端口后迁移，自定义端口保持不变；旧路径前缀在当前端口继续可用，缓存旧端口的客户端需重新打开。全部网关停用后可在高级设置修改共享端口。Grok 在“导入当前配置”中预览模型及凭据来源，确认后建立独立 API 模型；终端环境未被桌面应用继承时，可明确选择读取 `endpoint.env` 的简单赋值。

账号登录、导入导出及 Claude 官方／API 文件切换表见[账号与客户端配置](docs/clients.md)。协议、配置恢复和各客户端的边界见[客户端网关与调度](docs/gateway.md)。

## 隐私

应用不会把账号、配置或供应商凭据上传到本项目的服务，也不包含广告和遥测。启用网关后，业务请求只发送到用户配置的供应商；检查更新只访问官方 GitHub Release API。仓库和发布包使用脱敏夹具，不包含真实账号、令牌、回调地址或个人路径。完整说明见 [PRIVACY.md](PRIVACY.md)。

更新检查以外，公开价格更新、供应商额度／模型查询、端点测速及原生 CLI 登录按各自功能运行；完整目标、触发方式和源码检查边界见[网络访问清单](PRIVACY.md#网络访问)。

数据位置、旧安装兼容、恢复记录和清理范围见[本地数据与恢复](docs/data.md)。内部标识 `com.lich13.gpt-switch` 与旧私有目录保持兼容。

## 开发

Node.js 版本见 [.nvmrc](.nvmrc)，pnpm 版本见 [package.json](package.json) 的 `packageManager`，Rust 版本与组件见 [rust-toolchain.toml](rust-toolchain.toml)。CI 使用同一来源；另需 [Tauri 平台依赖](https://v2.tauri.app/start/prerequisites/)。

项目设置 `pmOnFail: ignore`，让前端锁文件保持 GitHub 依赖图可识别的单文档格式；pnpm 不再自行切换版本。本机通过 Corepack 使用 `packageManager` 指定的版本，或先安装并核对完全相同的 pnpm 版本。CI 的 `pnpm/action-setup` 继续从 `packageManager` 安装固定版本。

```sh
corepack pnpm --version
corepack pnpm install --frozen-lockfile
corepack pnpm icons
corepack pnpm test
corepack pnpm build
cargo test --locked --manifest-path src-tauri/Cargo.toml
cargo clippy --locked --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo fmt --manifest-path src-tauri/Cargo.toml -- --check
corepack pnpm tauri dev
```

`pnpm dev` 是标明“预览”的模拟界面，真实凭据只能在桌面应用中操作。`--smoke-test <绝对输出路径>` 使用独立临时账号和配置，验证 WebView、托盘、账号切换、网关监听与认证、两字段切换、退出恢复和 auth 不变后退出，不接触真实 Codex 目录。

`pnpm icons` 从原创 SVG 生成 9 档 PNG、黑白托盘图标、ICNS 和 ICO。发布通过 CI 构建 macOS Apple Silicon DMG 与 Windows x64 NSIS 安装包及校验和；真实 OAuth 和操作系统托盘交互验收与自动冒烟测试分别记录。

## 依赖维护

Dependabot 每周一 09:00（Asia/Taipei）检查 Cargo、前端和 GitHub Actions，普通更新冷却 7 天。Cargo 覆盖直接与间接依赖，以 `lockfile-only` 在现有约束内维护主锁文件，每次最多 1 个普通 PR；同时通过 `allow.update-types` 只允许兼容的 minor／patch，提前排除不兼容候选，避免解析器尝试 reqwest 0.13 等迁移。Cargo 的 `0.x` 次版本变化属于不兼容升级；本地 `lmfpd` 的依赖也通过主锁文件审计。前端普通更新允许 minor／patch，按 React 全家桶、其他运行依赖、开发工具分成 3 组，最多 3 个 PR；React 与 React DOM 的运行版本必须一致，对应类型包一起评估。Actions 合为一组，最多 1 个普通 PR，继续固定提交 SHA。

漏洞告警和自动安全修复在 GitHub 仓库设置中独立启用；安全更新使用独立组，不等待普通更新窗口或冷却期。普通更新的版本限制不屏蔽安全修复。保持人工合并，并启用合并后自动删除分支。

每月集中核对 Rust、Node、pnpm、审计工具、Tauri 关联组件及延期升级；安全修复优先处理。更新 Rust 锁文件使用 `cargo update --manifest-path src-tauri/Cargo.toml`，前端使用 `corepack pnpm update`，随后执行冻结安装、受影响测试、构建和安全检查。固定工具版本需要显式更新，并核对本机与 CI 一致；Tauri JS API、CLI 和 Rust 组件作为兼容组合评估，不能仅放宽单个包的约束。前端锁文件继续保持单文档，推送后确认 GitHub 依赖清单包含实际运行、开发和间接依赖。

Tauri 2.11 的运行时保持 2.11 系列，宏、构建和代码生成组件保持 2.6 系列，避免新组件的监视器返回类型、菜单宏及 Windows 图标接口与主包不匹配；升级 Tauri 主包时一起评估这些约束。`yaml-edit` 固定为 0.3.2，0.3.5 会使现有 DSH 空数组、注释和恢复往返测试失败；后续升级须先通过这些原有夹具，保留文件与外部修改保护。

没有安全或功能必要时，延期迁移 jsdom、图标库、测试断言库，以及 Rust 的 sha1／sha2、toml／toml_edit、reqwest、rusqlite、sysinfo、yawc 等兼容线。需要调整约束才能修复安全问题时先评估兼容性，不新增漏洞忽略。依赖 PR 失败时一次读取全部失败检查，先处理共同原因；只有修复或新证据出现后再重跑，必要时拆分失败组定位。已被吸收或明确延期的 PR 经核对后关闭，分支仅在确认没有额外提交时删除。

依赖审计使用 `cargo audit --file src-tauri/Cargo.lock` 和 `corepack pnpm audit`。v0.25.32 的 Rust 锁文件已移除 Tokio I/O 健全性告警、撤回的 `clipboard-win` 版本和 5 条 `unic-*` 维护告警；仍保留以下 5 条上游维护／健全性警告，不通过忽略列表隐藏。

| 间接依赖 | 告警与约束 |
| --- | --- |
| `proc-macro-error 1.0.4` | RUSTSEC-2024-0370，未维护；由 GTK 3 的宏依赖引入，不在 macOS／Windows 依赖树中。 |
| `glib 0.18.5` | RUSTSEC-2024-0429，迭代器健全性问题；由 Tauri 的 Linux GTK 3 链约束，不在 macOS／Windows 依赖树中。不能只替换为不兼容的 GLib 大版本。 |
| `zbus_names 4.3.5` | RUSTSEC-2026-0371，未维护；由 Linux 单实例插件的 D-Bus 链引入，不在 macOS／Windows 依赖树中，等待上游兼容迁移。 |
| `zvariant 5.15.1` | RUSTSEC-2026-0370，未维护；由同一 D-Bus 链引入，不在 macOS／Windows 依赖树中，等待上游兼容迁移。 |
| `zvariant_derive 5.15.1` | RUSTSEC-2026-0372，未维护；由同一 D-Bus 链引入，不在 macOS／Windows 依赖树中，等待上游兼容迁移。 |

MIT License。独立项目，与 OpenAI 无隶属关系。
