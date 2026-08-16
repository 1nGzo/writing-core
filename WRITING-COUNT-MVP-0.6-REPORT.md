# Writing Core MVP-0.6 写作字数统计验证报告

## 1. 最终 Writing Count 规则

| 内容 | 规则 |
| --- | --- |
| Han | 每个 `Script::Han` 字符计 1；覆盖 Unified Ideographs、扩展区和兼容汉字 |
| English | 连续 Latin 英文词计 1；常见 apostrophe 形式如 `don't`、`Tiru's` 保持一个词 |
| Number | 连续数字计 1，例如 `59`、`2026` |
| 标点 | 0 |
| 空格/Tab/换行 | 0 |
| Markdown | parser syntax 0；heading 内自然语言计入 |
| Link | label 计入，URL destination 0 |
| Image | alt text 计入，图片路径 0 |
| Code | inline code 和 fenced/indented code block 0 |
| Metadata/HTML | Markdown metadata、HTML tag/block 作为技术结构排除 |

统一输出 `total / han / english_words / numbers`，当前 UI 主要使用 `total`。

## 2. 技术实现

```text
Markdown
  → pulldown-cmark 0.13.4 parser events
  → prose text
  → Han / English / Number tokenization
  → WritingCount
```

Rust 核心使用 `pulldown-cmark` 和 `unicode-script`。没有新增正文存储；Markdown 文件仍是正文唯一真源。

## 3. RAW 与 Writing 双层统计

例如 `Tiru飞到了第59章。`：

```text
RAW scalar total: 12
WRITING total:     7
```

`欧林索斯。 → 欧林索斯！` 的变化为：

```text
RAW:     inserted 1 / deleted 1 / net 0
WRITING: inserted 0 / deleted 0 / net 0
```

## 4. Delta 算法

File Watch 和 CodeMirror 都对 old/new Markdown 的 prose token stream 做 secondary diff。Han token 按具体字符对齐；英文和数字按单位类型对齐，因此：

```text
Tir → Tiru          WRITING net 0
5 → 59              WRITING net 0
Writing → Writing Core   WRITING net +1
```

不会把孤立 diff chunk `u` 或 `9` 错算成新写作单位。

## 5. Document Count

`document_writing_count` 在打开文档、真实保存和 File Diff accepted snapshot 后直接从当前 Markdown 重算，不通过 `initial_count + session_net` 长期推导。

## 6. SQLite

schema version 从 2 迁移到 3，新增：

```text
documents.current_writing_units
documents.writing_count_version
sessions.writing_inserted_units
sessions.writing_deleted_units
sessions.writing_net_units
sessions.writing_count_version
edit_events.writing_inserted_units
edit_events.writing_deleted_units
edit_events.writing_net_units
edit_events.writing_count_version
```

旧事件和旧 session 的 Writing 字段保持 `NULL`，没有用 RAW 数字伪造历史 Writing 数据。SQLite 仍不保存正文全文。

## 7. 自动测试

```text
cargo fmt --manifest-path src-tauri/Cargo.toml -- --check   PASS
cargo test --manifest-path src-tauri/Cargo.toml             23 passed
npm run build                                                PASS
```

测试覆盖 MVP 示例、Markdown semantic extraction、Unicode Han script、token boundary、RAW/WRITING 分离、document count、session 累计和既有 File Watch/session 回归。

## 8. Performance

测试 profile 下，100k Han scalar Markdown smoke input：

```text
writing_count_100k_scalars_ms=45
```

该数值是当前验证机和测试 profile 的参考值，不是正式 benchmark 契约。

## 9. 已知限制

- File Diff 只统计已保存的 old/new snapshot，不可见两次保存之间的输入后删除、撤销等劳动。
- 当前 UI 仍是 Debug/Monitor UI，没有开始正式用户 UI、历史统计页或 DOCX。
- CodeMirror 通过 Rust transient command 计算 Writing delta；若 IPC/统计调用失败，事件仍可保留 RAW，但 Writing 字段为 unavailable，不 fallback 到 RAW。
- 尚未进行本轮 MarkText 人工验收，因此不能把自动测试当成真实人工运行 PASS。

## 10. 结论

```text
READY FOR MANUAL TEST
```

## 11. 最短人工验收

1. 在 Monitor + MarkText 中保存 `“欧林索斯……真的？”`，确认 RAW 增量大于 Writing 增量。
2. 保存 `Tiru飞到了第59章。`，确认 Writing 增量为 `+7`。
3. 将 `林祺` 改为 `**林祺**`，再将 `Tir` 扩为 `Tiru`，确认 Writing net 都为 `0`，RAW 仍能记录 Markdown/字符变化。
