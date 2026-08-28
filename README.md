# Caelorum Writing Core

Writing Core 现在是独立常驻的 headless runtime；Tauri Writing Monitor 是可选 GUI client，Caelorum「创作」通过 Browser 后端代理消费同一个 Core。正文统计、SQLite、file watcher、session/idle policy 与 live snapshot/dedupe 都只存在于这一套 Rust runtime 中。

```text
caelorum-writing-core (systemd user service, 127.0.0.1:43180)
├── Caelorum Creation
├── Standalone Writing Monitor client
└── future Tiru consumer
```

核心数据流仍是：

```text
普通 Markdown 文件
        ↓
notify 父目录监听 + debounce
        ↓
内存 snapshot + SHA-256 hash
        ↓
similar Unicode scalar diff
        ↓
RAW Unicode scalar EditEvent
        +
Markdown → prose → WRITING token diff
        ↓
Rust Writing Core / SQLite
```

Standalone 中的 CodeMirror 仍是可选 client；它把完整 snapshot 发送给 headless Core，不拥有第二个 Core、watcher 或 SQLite：

```text
CodeMirror transaction → POST /v1/snapshot → Headless Writing Core
```

正文唯一真源仍然是 `.md` 文件。SQLite 只保存文档元数据、session、统计和事件 metadata，不保存正文全文；File Watch 的 previous snapshot 只存在进程内存。

## Daily Use

先构建两个正式 executable，再安装：

```bash
npm run tauri build
cargo build --release --manifest-path src-tauri/Cargo.toml --bin caelorum-writing-core
./scripts/install-local.sh
```

安装脚本会安装并启用 `caelorum-writing-core.service`。登录后 Core 自动启动；不需要先打开 GUI：

```bash
systemctl --user status caelorum-writing-core.service
curl -sS http://127.0.0.1:43180/v1/status
```

日常启动命令固定为：

```bash
writing-monitor
```

它可以从任意当前目录执行，启动已安装的可选 Tauri client，并连接现有 Headless Core。关闭 GUI 不会停止 Core。默认命令启动后立即返回终端；排错时可使用：

```bash
writing-monitor --foreground
```

源码修改并重新构建后，需要再次执行 `./scripts/install-local.sh` 更新安装副本。

## 当前能力

- Headless Core 由 systemd user service 独立持有生命周期并只监听 `127.0.0.1:43180`
- tracked document 路径保存在 Core data dir 的 `tracked-document.json`；Core 重启时尽量恢复，文件失效时安全跳过
- Writing Monitor 默认入口：作为 client 选择一个 `.md` / `.markdown` 文件并调用 `/v1/track`
- 监听目标文件所在父目录，支持普通保存和 atomic replace 后的目标路径恢复
- 300ms debounce；只有文件系统事件触发后才重新读取、hash 和 diff
- `similar 3.1.2` character diff，统计单位是 Rust `char` / Unicode scalar
- RAW scalar 统计保持原口径；WRITING 使用 `pulldown-cmark 0.13.4` prose extraction、`unicode-script 0.5.8` 的 Han 判断和 token-level secondary diff
- WRITING token：Han 单字、连续 Latin 英文词、连续数字；标点、空白、Markdown syntax、URL destination、inline/fenced code 不计
- 标题和链接/image alt 的语义文字计入；链接 URL 与图片路径不计
- 当前文档 Writing Count 在打开、真实保存和 File Diff accepted snapshot 后从当前 Markdown 重算，SQLite 不保存正文
- File Watch 事件使用 `source = file.diff`、`tracking_origin = file_diff`
- Creation / CodeMirror live snapshot 使用共享 snapshot/dedupe 路径；保存相同内容不重复计数
- baseline 不产生写作事件；same hash 不产生事件
- session 分开统计：`session_span_seconds`、`session_active_seconds`、`session_idle_seconds`、`since_last_edit_seconds`
- 默认 idle threshold 为 300 秒；session end threshold 为 3600 秒
- 可用环境变量缩短人工测试阈值：`WRITING_SPIKE_IDLE_SECONDS=10`、`WRITING_SPIKE_SESSION_END_SECONDS=60`
- Monitor Debug 区显示 Rust 当前实际生效的 runtime timing；Debug build 可用 Apply 临时改为 10/60，release 不提供覆盖命令
- File Diff Debug Log 显示 event_id；同一 event_id 在 Core/SQLite/React 只保留一条
- 数据库迁移到 schema version 3，新增 `writing_*_units`、`current_writing_units` 和 `writing_count_version`；旧历史 Writing 字段保持 `NULL`

## Development

GUI 开发启动仍使用：

```bash
npm run tauri dev
```

Headless 开发运行可使用 `cargo run --manifest-path src-tauri/Cargo.toml --bin caelorum-writing-core`。日常使用不需要启动 Vite、Node、Cargo 或 Tauri dev server。

当前验证机没有系统级 WebKitGTK 4.1 / rsvg2 开发包，之前验证使用了 `/tmp/caelorum-writing-sysroot` 临时用户态 sysroot。先在终端加载 Rust：

```bash
cd /home/ingzo28/CaelOS/writing-monitor
. /home/ingzo28/.cargo/env
```

本机仍缺少系统级 WebKitGTK 4.1 / libsoup-3.0 开发包，但之前准备的用户态 sysroot 仍存在于 `/tmp/caelorum-writing-sysroot`，可以复用，不需要重新下载。设置临时 sysroot 并启动 Native Wayland：

```bash
export PKG_CONFIG_SYSROOT_DIR=/tmp/caelorum-writing-sysroot/root
export PKG_CONFIG_PATH=/tmp/caelorum-writing-sysroot/root/usr/lib/x86_64-linux-gnu/pkgconfig:/usr/lib/x86_64-linux-gnu/pkgconfig:/usr/share/pkgconfig
export LIBRARY_PATH=/tmp/caelorum-writing-sysroot/root/usr/lib/x86_64-linux-gnu:/usr/lib/x86_64-linux-gnu:/lib/x86_64-linux-gnu
export LD_LIBRARY_PATH=/tmp/caelorum-writing-sysroot/root/usr/lib/x86_64-linux-gnu:/usr/lib/x86_64-linux-gnu:/lib/x86_64-linux-gnu
export C_INCLUDE_PATH=/usr/include:/usr/include/x86_64-linux-gnu
npm run tauri dev
```

启动后如需检查 runtime timing，进入 Monitor 的高级模式；默认 compact 主界面不展开 Debug 区。Debug build 可在高级模式输入 `10 / 60` 并点击 Apply；Rust 会立即使用新值。环境变量仍可在启动前覆盖阈值，但无需依赖它来确认当前 runtime 配置。

XWayland / Pin 技术验证不要永久修改默认方式，在同一终端运行：

```bash
npm run tauri:dev:x11
```

停止开发窗口：终端按 `Ctrl+C`。

## Window State

窗口默认尺寸为 `430 × 360`，最小尺寸为 `340 × 280`。项目使用官方 `tauri-plugin-window-state`，保存单一窗口的 width、height、x、y 以及其他窗口状态；关闭时保存，下次启动时恢复。用户在 advanced 模式调整过的尺寸也会沿用，回到 compact 不会强制缩回。

插件恢复坐标时会检查保存位置是否与当前可用 monitor 相交。如果之前使用的显示器已经不存在，旧坐标不会被应用，由系统重新决定安全位置；本项目不额外实现多屏坐标管理。

## Window Pin 结论

窗口默认配置为 `alwaysOnTop: false`、`focus: false`。置顶是 Monitor 的可选开关，偏好保存在 WebView 的 localStorage 中，并区分 requested / supported / effective；窗口尺寸和坐标不使用 localStorage，而由 Window State 插件管理。

Native Wayland 会显示：

```text
置顶：当前不可用
```

Native Wayland 不会调用 `setAlwaysOnTop(true)`，也不会用循环 raise、focus、wmctrl、xdotool 或全局窗口 hack。Tauri API 的 `setAlwaysOnTop` / `isAlwaysOnTop` 和 `core:window:default` 权限已接入；但 API 返回成功不等于 compositor 实际改变层级。当前 Native Wayland 启动已验证，真实层级仍需肉眼确认。

XWayland 运行时会识别为 `backend = xwayland`，并允许调用 `setAlwaysOnTop(true)`；启动路径已提供，实际 Pin 层级仍需肉眼确认。GTK 的 keep-above 行为依赖窗口管理器；不能把 API 状态当成真实 topmost 保证。

Pin 逻辑没有周期性 `focus()`、`raise()` 或自动抢焦点循环。用户点击 MarkText 后，MarkText 应继续获得键盘焦点；Monitor 是否保持在上层必须用肉眼确认。

## 自动验证

```bash
npm run build
cargo fmt --manifest-path src-tauri/Cargo.toml -- --check
cargo test --manifest-path src-tauri/Cargo.toml
```

当前 Rust 自动测试覆盖：

- Unicode scalar 口径
- File Diff same-hash、插入、删除、replace 统计
- active grace 与 idle 区间分离
- `active → idle → active` 后历史 idle 不清零
- 60 秒 session end 后重新创建 session
- editor/file diff tracking origin 混合
- runtime timing 默认值、Debug override、非法阈值拒绝与即时重算
- 相同 event_id 的 Core 幂等持久化
- 中文、标点、英文、数字、中英混排、Markdown heading/bold/link/image/code/frontmatter
- Han extensions/compatibility ideographs、英文 apostrophe、`Tir → Tiru`、`5 → 59`、标点/格式变更
- RAW 与 WRITING 分离、当前文档重算和 100k scalar Markdown smoke benchmark

## 人工测试：MarkText + Monitor

1. 启动 Monitor，确认 Debug 区显示 Rust runtime timing `300 / 3600`，再 Apply `10 / 60`，确认 Rust 日志和界面一致。
2. 选择 `fixtures/mvp-smoke.md`，用 MarkText 打开同一个文件，输入中文并保存；确认只出现一个带 event_id 的 `file.diff`。
3. 等待 30 秒再保存一次，预期 `session_span` 约 30 秒、`session_active` 约 10 秒、`session_idle` 约 20 秒；恢复写作后 idle 不清零、session_id 不变。
4. 检查 inserted / deleted / net、SQLite 新增事件数与 Debug Log 唯一行一致。
5. 在 Native Wayland 下点击其他窗口：Monitor 的 Pin 应明确显示不可用；键盘输入必须继续进入其他窗口。
6. 使用 `npm run tauri:dev:x11` 重启，再打开 Pin。点击 MarkText 后肉眼确认 Monitor 是否仍在上方，同时确认 MarkText 仍拥有键盘焦点。
7. 在 XWayland 模式检查 CodeMirror Debug 是否正常显示；中文 IME、字体和 DPI 需要人工记录，不以 API 成功代替结果。

## 已知限制

- File Diff 只能观察已保存正文变化；两次保存之间“输入后删除”“粘贴后撤销”等中间劳动不可见。
- MarkText 的 autosave / Ctrl+S 节奏决定 File Watch 的观察精度，本轮需要真实记录，不能宣传为逐按键统计。
- Native Wayland 没有本项目可以安全依赖的标准应用级 always-on-top 保证。
- XWayland 置顶可能可行，但中文 IME、字体/DPI 和实际层级仍需人工验证。
- 本轮不实现 DOCX、章节树、桌宠、AI、全局键盘 hook 或正式产品 UI。

## MVP-0.6 状态

```text
READY FOR MANUAL PIN TEST
```

代码和自动验证已准备好；窗口状态恢复、MarkText 保存节奏、Wayland/XWayland 实际层级、焦点和 IME 结果必须由人肉眼确认后再决定下一步。
