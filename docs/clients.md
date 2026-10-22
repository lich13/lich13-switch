# 账号与客户端配置

## DeepSeek Harness

主窗口的 DeepSeek Harness 视图与 Codex、Claude Code、Grok Build 使用同一个本地监听，但拥有独立供应商、健康状态、调度、凭据和模型目录。接入入口只显示 DSH profile 的脱敏信息；旧版仅有 `config.yaml` 时提示升级，不自动迁移。

每个 profile 选择一种路由协议。`openai-completions` 和 `openai-responses` 使用 `/v1`，`anthropic-messages` 使用根基址。模型列表来自支持所选协议的在队供应商并集及手动项；列表为空时可手填明确模型 ID。勾选“设为默认”后才管理 `agent-default-model`，只影响新会话。

接入会保留 DSH YAML 注释、未知字段、其他 provider 和外部配置修改。供应商 Token 留在本应用 Rust 凭据存储；DSH 文件只引用本地网关 token。停止和恢复仅处理受管行，不整文件回滚；遇到外部修改会保留现场。

“同步到全部现有 profile”默认关闭。开启后按 profile 串行应用本次协议、模型和默认选择，不自动创建 profile。每项显示完成、失败或未执行；失败保留此前成功项，并回滚失败项，外部冲突保留恢复事务。所有受管 profile 共用首次接入前的凭据快照；全局停用逐项恢复配置，最后解除时恢复共享凭据。切换协议需先停用。

接入支持空数组及多行 patch，并保留注释和未知条目。非空的行内数组需先在配置编辑器展开为多行 YAML 数组；无法安全编辑时保留原文件并提示处理。

配置页选择 DSH 后，可编辑 profile patch、home patch 和已存在的旧版 `settings.yaml`，无需 bundle 解析成功。`.env` 和 `.credentials.yaml` 只显示名称与遮蔽值。保存和恢复都校验原文件版本与待写入内容，接入期间保护路由和受管默认模型，包括全局覆盖入口；存在恢复事务时拒绝普通编辑。每文件保留一份上一版私有原文备份；失败保留草稿，切换文件前确认未保存修改。

当前供应商依据可确定的 profile、bundle、profile patch、home patch 和旧版 settings 读取，不以“已接入”代替实际选择。home 覆盖单独提示；启动参数、其他进程环境和项目覆盖无法确认时降低判断置信度。凭据优先读取已存文件，再参考当前可见环境和 home `.env`，不执行 shell 或动态 YAML。bundle 查找支持 DSH home 下的 `runtime/node_modules`、`lib/node_modules` 及 PATH 中实际可执行文件的祖先目录；缺少依赖时先通过 DSH 启动对应 profile 安装。

存在多个 profile 时，仅在其静态供应商与模型选择一致时展示共同结果；选择不同时显示无法判断，可在接入面板查看指定 profile。最后同步的 profile 不代表原生启动器的实际选择，参见 [DSH 启动参数](https://github.com/deepseek-ai/deepseek-harness/blob/master/apps/cli/src/args.ts)。

## Claude 模型发现

Claude 网关只自动管理地址和令牌两个字段。模型发现由用户手动开启 `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`；客户端会过滤、排序和合并网关返回的模型选项，因此界面不保证逐一展示网关返回的全部 ID。

## Grok Build 配置来源

Grok 导入预览按系统 managed、用户 managed、用户配置、用户 requirements、系统 requirements 的顺序读取。预览会标出模型、地址、协议、凭据可用性、缺失环境变量和无法由 GUI 确认的 `GROK_HOME`、CLI 临时模型、`/model` 选择及远端 campaign 状态。写入只修改用户文件中已确认的受管字段；requirements 或明确环境覆盖阻止接管时显示具体原因。状态读取与接管限制分开：默认模型按 requirements 固定约束、环境变量、用户配置、managed 默认值确定，残留受管模型表不会改变有效选择；模型自己的凭据优先于全局官方登录偏好。地址唯一且凭据不可得时显示“按地址匹配”，已知凭据不同不降级匹配。

## Codex 账号

启动及每 2 秒检查当前 `auth.json`，文件身份和时间等元数据未变时复用内存结果，变化或元数据不可靠时稳定读取：已有身份的更新版本同步入库，新身份自动加入列表。ChatGPT 优先按一致的 `chatgpt_user_id`／`user_id` 与工作区区分，缺失稳定用户 ID 时回退到 `sub` 与工作区；API Key 按指纹区分。邮箱与套餐不参与唯一身份判断，矛盾的身份字段拒绝覆盖。优先比较有效 Token 签发时间，没有可比较时间时再使用有效 `last_refresh`（兼容秒、毫秒和 RFC3339），保留已保存的较新凭据并提供“使用已保存版本”；单个账号的日常同步在无法可靠排序时采用最近检测到的稳定内容。监控不回写 auth 或配置，无效／缺失文件不会删除账号。删除账号后，同一文件版本在重启后也不会重新入库，出现新版本才重新收录。

读取旧账号库时，确认属于同一用户及工作区的重复项会合并，保留最早条目的 ID、名称和位置，采用可确认较新的凭据，不按套餐高低选择。兼容的未知字段和账号交换元数据保留；身份或扩展字段冲突、两份不同凭据无法判断新旧时保留原记录并报错。规范化提交检查文件版本，不覆盖外部修改，也不改写当前 `auth.json`；失败可在问题排除后重试。套餐取自实际凭据，刷新不会额外联网推断订阅。

点击“添加账号”通过官方 Codex CLI 登录；账号页“导入”支持完整 Codex `auth.json`、Sub2API `openai/oauth` 和 CLIProxyAPI `type=codex` JSON。预览后可部分选择，重复账号默认跳过，明确勾选后更新；导入仅保存账号，不自动激活。

账号页“导出”提供 Sub2API 多账号单文件和 CLIProxyAPI 每账号一个 JSON。OAuth 使用独立模板，保留识别出的行为配置；真实 Token、身份和有效期取自所选账号，不生成替代 `id_token`。同一账号原格式往返保留专用元数据，外部格式的附加信息不写入客户端认证文件。其他平台、不兼容认证模式及缺失凭据会明确提示。

选择账号后仅替换认证文件，配置保持不变。现有 Codex 进程可能缓存凭据，需要自行重新打开。

ChatGPT 登录需要本机已安装 [官方 Codex CLI](https://developers.openai.com/codex/cli)。应用自动查找 PATH、常见安装目录和 macOS nvm；旧安装中的 CLI 路径偏好继续兼容。Windows npm 安装使用同目录 Codex JavaScript 入口和 Node.js。登录使用独立临时目录；成功后若与当前账号相同，更新账号库和当前 `auth.json`，其他账号仅加入列表；10 分钟未完成会取消。浏览器授权需本人完成，也支持设备码登录。浏览器登录完成后，可把当前 CLI 提供的 `http://127.0.0.1:<port>/success?...` 地址粘贴回应用；应用只向当前本机 CLI 回调服务提交一次，并要求该临时 CLI 已生成与回调匹配的完整 `auth.json`，不从裸 `id_token` 拼接凭据。回调地址只在当前登录会话内有效。

账号页的 ChatGPT 账号可启用“官方账号”。启用会先正常停用 Codex 网关，再注释现有 custom 的 `base_url`、`experimental_bearer_token` 和 `supports_websockets` 三个独立字段，并切换该账号；关闭时按事务记录精确恢复原行文本。官方连接期间不能启动 Codex 网关或编辑这三个字段，其他配置仍可编辑；外部修改受管行会显示冲突。关闭后不会自动启动网关。

默认目录为 `CODEX_HOME` 或 `~/.codex`（Windows 为用户主目录中的 `.codex`），旧路径偏好继续兼容。账号切换仅影响文件；`keyring` / `auto` / `ephemeral`、provider bearer token、环境变量或命令认证可能使用其他来源。

配置页按原文保存，保留注释、未知字段和换行。外部修改发生时保留未保存草稿并阻止覆盖；复制草稿、重新读取并合并后保存。删除列表中的账号不会注销当前 Codex。

每个 ChatGPT 账号行提供“设备码刷新”，用于更新指定账号的凭据。完成同一用户的官方 CLI 隔离登录后，整组替换 Token 及配套账号字段，工作区变化不再拦截，也无需额外选择工作区。保留账号名称、顺序和扩展字段；登录到其他用户时拒绝覆盖。当前磁盘账号同步更新 `auth.json`，其他账号仅更新账号库。登录期间账号或磁盘凭据变化时停止覆盖；刚完成的有效官方登录不会因旧刷新时间被拒绝。

## Claude 可视化配置

配置页选择 Claude Code，仅编辑当前 `CLAUDE_CONFIG_DIR` 或 `~/.claude` 中的全局 `settings.json`。默认以模型、权限、偏好、环境变量和高级分组编辑；JSON 模式提供原文、高亮、查找、折叠及显式格式化。两种方式共用内存草稿，切换不会自动保存。项目、组织配置和 `~/.claude.json` 不在编辑范围。

默认模型支持供应商列表与完整 ID 手填；已有 `ANTHROPIC_MODEL` 优先于根节点模型，推理强度写入支持 `max` 的 `CLAUDE_CODE_EFFORT_LEVEL`。权限规则可通过工具、匹配内容、处理方式添加或原文修改，不自动放宽权限。未知字段通过高级树增删，已有插件、Hook 和状态栏配置完整保留；编辑不会安装插件或执行 Hook。

“默认”表示未设置；零、关闭和空字符串分别保留。恢复默认只删除选定字段。查看改动会遮蔽敏感值，“恢复上次配置”先载入草稿，再由用户保存。`Cmd/Ctrl+S` 保存前校验 JSON、重复键和已知类型；其他字段、格式及 CRLF 保留。外部修改造成版本冲突时保留草稿。

网关运行或有待恢复事务时，编辑器不能改变其两项连接字段，其他设置可以保存；网关关闭时可显式修改，但不会自动新增或切换供应商。模型、环境等设置保存后提示重新打开 Claude Code；普通偏好显示保存结果。其他终端、项目及组织配置仍可能覆盖当前文件。参见 [Claude 配置](https://code.claude.com/docs/en/settings) 与 [模型优先级](https://code.claude.com/docs/en/model-config)。

## Claude 官方／API 配置

Claude 网关页和配置页提供“官方／API”切换。首次启用官方配置前，先退出正在运行的 Claude Code 会话；应用正常停止 Claude 网关，保存当前 API 档，再写入最小官方配置。后续可在配置页保存自己的官方偏好。返回 API 恢复原连接配置，但不会自动开启网关。普通供应商切换仍只管理地址与认证两个字段。

全局文件按角色处理：

| 文件 | 切换行为 |
| --- | --- |
| 当前配置目录中的 `settings.json` | API 档保存原文；官方档首次不继承 API 连接、模型映射及 Hook／插件设置，之后保留官方偏好 |
| 当前配置目录中的旧 `claude.json` | 记录存在状态并暂存，防止成为旧 API 回退入口；返回 API 时恢复 |
| 当前配置目录中的 `config.json` | 只移除并恢复原有 `primaryApiKey`，不改变其他字段，不凭空创建文件或 Key |
| 对应的全局 `.claude.json` | 只处理 API Key 批准记录及首次未登录初始化；保留有效官方账户、机器身份、MCP 与项目信任 |
| `.credentials.json`、macOS Keychain | 不替换、不复制、不删除、不回滚，由官方 CLI 管理 |

默认目录使用用户主目录的 `.claude.json`；指定配置目录时使用该目录中对应的 `.claude.json`。切换校验文件版本、严格 JSON 和重复键；遇到外部受管字段修改会停止并保留现场。辅助文件缺失不会为清理而创建。部分写入失败由恢复事务处理，只恢复仍能确认属于本次操作的内容；CLI 新写入的账户和初始化状态不会随返回 API 而整份回滚。

“官方登录”调用本机 `claude auth login --claudeai`，授权完成后读取 CLI 实际登录状态。应用不修改 `claude` 命令、PATH、shell、项目设置、Skills 或 MCP，不强制终止会话。项目、IDE、环境变量或组织策略仍可能覆盖全局配置；可识别的覆盖会显示提示。官方配置用于避免连接混用，不保证所有启动入口完全隔离，也不承诺避免账号封禁。

## CC Switch 链接

设置中的“CC Switch 链接”读取系统默认接收应用，可选择已安装且支持协议的 lich13-switch、CC Switch 或 lich13studio。macOS 直接设置并回读；Windows 打开系统默认应用界面完成选择。安装、启动和刷新均不抢占默认关联。

支持 `ccswitch://v1/import?resource=provider&app=codex` 和 `app=claude`。导入预览显示目标客户端，重复检测按客户端隔离。接收后确认名称和地址，再新增供应商；不切换当前供应商、不启停网关、不改 Codex 配置。部署子路径原样保留，凭据只在 Rust 内存中等待确认。已有表单草稿会保留，导入排队处理。其他资源、客户端、脚本和远程配置不导入。
