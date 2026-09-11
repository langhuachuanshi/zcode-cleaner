# ZCode 清理器

一个轻量的桌面小工具（Tauri），用来清理 `~/.zcode` 里的**会话数据**和**缓存**。

## 功能

- **总览**：`.zcode` 总占用 / 会话数 / 数据库体积 / 可清理缓存
- **不卡界面**：所有耗时操作（扫描、删除、清理、VACUUM）在后台线程执行，主线程只负责渲染；执行期间显示全局 loading 遮罩，并实时推送进度（如“正在删除会话 12/627：sess_xxx”）
- **会话清理**：以数据库 732 条会话记录为主线，聚合磁盘 5 处数据（转录 / 产物 / Shell 快照 / 模型 IO / 图片缓存）
  - 显示会话标题、项目目录、最后活动时间、磁盘占用
  - 删除会话 = 磁盘文件 + 数据库索引/消息历史（含 part 正文、用量统计等 13 张关联表）**一并永久删除**
  - 子会话（subagent）自动递归一并删除
- **保留策略**（持久化保存）：会话保留最近 N 天、日志保留最近 N 天、活跃保护开关；扫描后自动预选超出保留期的项，「按策略清理」一键执行
- **数据库瘦身**：删除会话后 SQLite 文件不会自动变小，点 VACUUM 把空间真正归还磁盘（实测 `part` 表占约 600M，历史会话清掉后缩幅明显）
- **安全边界**：
  - 近 24 小时活跃的会话默认锁定不可删（防止误删正在用的会话），可关闭保护
  - 永久删除前有强确认弹窗，列明将删内容
  - `db.sqlite` 本体不能整库删除，只支持删单会话记录 + VACUUM

## 开发

```bash
npm install
npx tauri dev     # 开发调试
npx tauri build   # 产物在 src-tauri/target/release/
```

环境要求：Node 18+、Rust (MSVC)、WebView2（Win11 自带）。

## 数据位置说明

| 位置 | 内容 |
|---|---|
| `~/.zcode/cli/agents/sess_*` | 会话转录（最大头） |
| `~/.zcode/cli/artifacts/sess_*` | 会话产物 |
| `~/.zcode/cli/exec/sess_*` | Shell 快照 |
| `~/.zcode/cli/rollout/model-io-sess_*.jsonl` | 模型 IO 日志 |
| `~/.zcode/cli/image-cache/sess_*` | 图片缓存 |
| `~/.zcode/cli/db/db.sqlite` | 会话索引 + 消息历史数据库 |
| `~/.zcode/cli/log`、`~/.zcode/v2/logs` | 按日期日志 |
| `~/.zcode/cli/plugins/cache` | 插件下载缓存 |
| `~/.zcode/v2/crash` | 崩溃转储 |

设置保存在 `~/.zcode-cleaner.json`。
