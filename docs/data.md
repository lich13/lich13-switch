# 本地数据与恢复

v0.25.14 新增模型目录缓存：各客户端数据目录的 `provider-models.json` 保存供应商 ID、配置版本、模型 ID、查询时间、缓存状态和脱敏错误；每个供应商最多 4,096 个模型，模型 ID 最长 256 字节，文件最多 2 MiB。删除供应商会删除对应缓存项，损坏文件不会被失败刷新覆盖。定价目录旁的 `models-dev-index.json` 只保存裁剪后的 models.dev 公共元数据，下载上限 16 MiB，缓存上限 4 MiB。

DSH 目录使用 profile 原有的 `cordis.patch.yml`、`.env` 和 `.credentials.yaml`；恢复事务使用私有 `dsh-recovery.json`，提交成功后删除。凭据文件沿用现有私有权限保护，恢复只修改受管行并保留未知字段。用量数据库仍为 v13，不新增 SQL 计量列、固定字典或历史费用重算；推理、速度和解析依据保存在现有诊断体中，冷详情和重启读取继续保留。

`gateway-listener.json` 保存三个客户端共用的端口及格式版本；旧客户端 `gateway.json` 中的端口仅用于兼容读取，运行入口以共享文件为准。Grok 明确导入期间的 `grok/grok-import.json` 保存 TOML、可选 `endpoint.env` 与供应商库的前后状态，包含凭据，采用私有权限；完成后删除，崩溃后继续恢复，遇外部修改保留现场，不能作为普通过期文件清理或分享。

数据目录为 Tauri 的 `com.lich13.gpt-switch` 应用数据目录（macOS：`~/Library/Application Support/com.lich13.gpt-switch`；Windows：`%APPDATA%/com.lich13.gpt-switch`）。`accounts.json` 保存完整账号，`previous-auth.json`、`previous-config.toml`、`claude/previous-settings.json` 各保留一份最近写入前的文件。`startup.json` 保存启动偏好，`quick.json` 保存快捷面板标签与固定状态。`gateway.json` 保存 Codex 状态，`claude/gateway.json` 保存 Claude 状态，`grok/gateway.json` 保存 Grok 状态（供应商、并发、RPM、模型白名单、访问令牌和恢复意图）；各客户端目录内的 `gateway-recovery.json` 保存版本化的连接事务与停止目标，普通 API 模式写入两文件期间的 `gateway-api-write.json` 保存短期恢复现场（完成后移除），`rpm-window.json` 只保存最近 60 秒的供应商／客户端限速时间戳，写入完成后清理过期记录。供应商 Token 仅用于其上游认证；凭据不写日志、不返回到列表或事件。文件采用当前用户权限，**未加密**，应像原 `auth.json` 一样保护。

已移除的旧统计数据不会恢复。升级仍精确删除应用私有目录内的 `usage.sqlite` 及 WAL／SHM／journal、`usage-settings.json`、`model-pricing.json`；失败可在设置中重试。账号、供应商、Key 额度、电源助手和 Codex 文件保留。现行用量数据位于独立 `usage/` 子目录，不受根目录旧数据清理影响。

应用内供应商代理已移除。升级先完成恢复事务，再清除两客户端旧代理对象、绑定和 `proxy-profiles.json`，不保留代理密码副本；失败时停止自动恢复，并在网关恢复入口重试。账号、供应商身份、Key、顺序、并发、白名单及启动意图保留。不会修改系统网络代理。

损坏的用量设置保留原文件，并在同一私有目录保存按内容摘要去重的备份，通过已有错误通道反馈；正常设置和数据格式保持兼容。启动清理只处理三个客户端私有目录内可确认归属的遗留请求暂存文件，恢复事务不按年龄删除。

`thread-reservations.json` 仅保存线程摘要、供应商内部标识和活动时间。运行中写入失败时保留内存名额，相关新请求暂时拒绝；后续调度活动重新保存成功后恢复准入，活动请求及默认 300 秒预留不被清空。启动时无法可靠读取的名额文件不会被新快照覆盖。
