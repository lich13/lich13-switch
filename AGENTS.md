# lich13-switch

- 简体中文协作。先读 DESIGN.md；保持产品名 lich13-switch、包名 lich13-switch。
- Codex／Claude Code／Grok Build 共用回环监听，默认端口 43721；无前缀请求按独立本地令牌分流，旧客户端前缀作为当前端口上的兼容别名，供应商与调度状态隔离，全部直连；Codex Bearer 模式仅写 custom 两字段，普通 API 模式仅写 auth.json 和当前选中 provider 的 base_url；首次识别不改用户文件，升级保留已有连接方式。Claude 网关仅写 settings.json 的 env 两字段；Grok 明确导入后按事务管理独立 API 模型连接，原生模型选择和官方 OAuth 仍由 Grok 管理，旧快照保留兼容恢复；Claude 配置编辑器只改用户选择的字段。内部身份 com.lich13.gpt-switch 和旧私有路径保持兼容。
- Claude 官方／API 配置切换是独立的全局多文件事务；仅按文件角色处理 settings.json、旧 claude.json、config.json 的 primaryApiKey 和全局 .claude.json 的 API 批准记录。不得替换或回滚 OAuth 凭据、机器身份、MCP、项目信任和 shell；普通网关操作继续保持两字段契约。
- 账号切换仅写 auth.json；config.toml 独立原文编辑，Claude settings.json 支持可视化与 JSON 共用内存草稿，保留注释和未知字段。禁止自动重启 Codex。
- Rust 独占文件及凭据操作；列表和事件只能返回脱敏元数据。真实凭据、用户配置和登录输出不得提交、截图或写入日志。
- 前端命令／事件契约集中于 src/bridge-contracts.ts，生命周期由 src/subscriptions.ts 管理；Rust 持久化在 src-tauri/src/storage/，应用级迁移入口在 migrations.rs，共享协议结算在 gateway/health_settlement.rs。用户操作细节位于 docs/，版本历史保留于 RELEASE.md。
- 按本次改动选择 pnpm test／pnpm build、cargo test／cargo clippy 的最小必要检查范围，验证编译、类型和明确代码故障；完整回归及 Windows 构建沿用现有 CI。Browser、原生程序、官方 OAuth 和业务流程验收默认交用户，提供入口、步骤、预期结果和未覆盖项；仅在用户明确要求或具体代码硬性故障需要时执行必要验收。
- 本项目后续开发交付计划默认包含提交、推送、标签 CI 和 GitHub Release 发布，除非用户明确缩小范围；部署和本地安装仍需单独授权。安装只做必要的成功状态或启动检查；功能验收标记待用户验收，不阻塞助手交付。普通规则维护不触发发布；只清理任务创建的临时和可再生产物。
