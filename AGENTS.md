# AGENTS.md

## Project

This repository is the experimental/early implementation of the **Caelorum Writing Core / Writing Monitor**, a local writing activity tracker for the Chinese novel project 《苍穹之诗》.

Current project path:

```text
/home/ingzo28/CaelOS/writing-monitor
```

The project is currently transitioning from technical spike to a usable local Writing Monitor.

Do not assume that every experimental feature should become part of the final product.

---

# Core Product Principle

The Writing Core is **not tied to one editor**.

The primary architecture is:

```text
Markdown file
    ↓
File Watch Adapter
    ↓
snapshot + hash + diff
    ↓
EditEvent
    ↓
Rust Writing Core
    ↓
SQLite
    ↓
Writing Monitor / future Tiru pet / future integrations
```

The user may write with:

* MarkText
* VS Code
* a future Caelorum Writer
* other Markdown editors

Writing Core must continue to work independently of the editor.

The existing CodeMirror editor is a debug / high-precision adapter, not a mandatory writing environment.

---

# Source of Truth

Novel正文 must remain normal files.

For Markdown:

```text
.md file = authoritative正文
```

Never make SQLite the authoritative正文 store.

SQLite may store:

* document metadata
* paths
* hashes
* timestamps
* sessions
* statistics
* edit-event metadata
* settings
* recovery metadata

Do not store complete正文 content in SQLite as the canonical document.

Temporary in-memory snapshots are allowed for diffing.

Recovery files may exist on disk when needed.

---

# Writing Core Is the Authority

Business logic belongs in Rust Writing Core.

React must not independently redefine:

* session start
* session end
* idle
* active writing time
* writing counts
* accumulated statistics

Frontend components should display Core state rather than calculate a competing version.

## Status API Contract

`GET http://127.0.0.1:43180/v1/status` is the stable read-only integration boundary for Writing Core.

It is localhost-only, reports Writing Core facts without正文 text, and must not be replaced by consumer access to the SQLite database. Consumers must not independently calculate active/idle/session statistics or watch Markdown files. Writing Core must not contain desktop-pet behavior, milestone, or animation policy.

---

# Session Semantics

Opening a document is **not** writing.

A session begins only after a real正文 change is observed.

Default production thresholds:

```text
idle_threshold = 300 seconds
session_end_threshold = 3600 seconds
```

Development/debug values may be temporarily overridden.

Definitions:

```text
session_span
= real elapsed time since session start

session_active
= active/thinking time excluding confirmed idle

session_idle
= accumulated confirmed idle time

since_last_edit
= time since the last observed real正文 change
```

Expected invariant:

```text
session_span ≈ session_active + session_idle
```

Small timer/refresh rounding differences are acceptable.

Idle history must not disappear when writing resumes.

The grace period before `idle_threshold` counts as active/thinking time.

---

# File Watch Rules

File Watch is the baseline tracking mechanism.

Use filesystem notifications rather than continuous polling.

Expected flow:

```text
notify
→ debounce
→ read file
→ hash
→ compare with last_good_snapshot
→ diff only when content changed
```

Important rules:

* `last_good_snapshot` must survive temporary target disappearance.
* Atomic replace must not reset the document/session.
* inode changes do not imply a new Writing Document.
* the monitored canonical path represents document identity for this stage.
* duplicate notify events must not create duplicate EditEvents.
* same hash means no正文 change.
* watcher events alone must never refresh `last_edit_at`.
* only an accepted正文 content change updates writing activity.

MarkText currently autosaves frequently enough that File Watch provides near-real-time tracking.

---

# Atomic Save Safety

Do not regress the existing atomic-save handling.

A temporary:

```text
Remove
Rename
Create
Replace
```

sequence from an editor must not cause:

```text
empty snapshot → entire document inserted
```

The previous accepted snapshot remains authoritative until a new valid file version is successfully read and diffed.

---

# Tracking Origins

Preserve the distinction between:

```text
editor_transaction
file_diff
```

`editor_transaction` may know:

* typing
* paste
* delete
* replace
* undo
* redo

`file_diff` only knows changes observed between saved versions.

Never claim that File Watch sees unsaved keystroke-level activity.

---

# RAW vs WRITING Statistics

There are two intentionally different accounting layers.

## RAW

Technical file-change accounting.

Uses Unicode scalar units.

Examples include:

* punctuation
* whitespace
* Markdown syntax
* newlines

RAW exists for debugging and data integrity.

Do not silently change its meaning.

## WRITING

Author-facing writing count.

Current Writing Count version:

```text
writing_count_version = 1
```

Rules:

* each Han character = 1 writing unit
* each continuous English word = 1
* each continuous number = 1
* punctuation = 0
* whitespace = 0
* newlines = 0
* Markdown syntax = 0
* heading text counts
* link label text counts
* URL target does not count
* code blocks and inline code do not count
* Markdown formatting-only changes should not change Writing Count

Examples:

```text
欧林索斯
= 4
```

```text
“啊？”
RAW > WRITING
WRITING = 1
```

```text
Tiru飞到了第59章。
WRITING = 7
```

Do not replace RAW statistics with Writing statistics.

Maintain both.

---

# Writing Delta

Writing inserted/deleted values must not be derived only from:

```text
new_document_count - old_document_count
```

because that loses rewrite effort.

Token-boundary cases must remain correct:

```text
Tir → Tiru
writing net = 0
```

```text
5 → 59
writing net = 0
```

```text
Writing → Writing Core
writing net = +1
```

Current document total Writing Count should be recalculated from the actual current Markdown content, not reconstructed indefinitely from historical session deltas.

---

# Monitor UI

Default UI is `compact`.

Compact mode is intended to be a small, long-running desktop monitor.

Primary values:

```text
总时间
= session span

休息时间
= session idle

实际码字时间
= session active

码字字数
= session writing net
```

The Writing Count card may also show the current document total.

Do not add technical/debug information back into the default compact view without explicit user request.

Advanced mode may contain:

* File Watch diagnostics
* RAW statistics
* Writing statistics details
* runtime timing
* hashes
* event IDs
* debug logs
* CodeMirror debug tools

---

# Window Behavior

The user wants the Monitor to behave as a small desktop instrument.

Window size and position should be remembered between launches.

The configured default size is only the fallback when no saved user window state exists.

Do not add startup logic that overwrites restored user size/position.

Do not automatically resize the native window when switching compact/advanced views unless explicitly requested.

---

# Wayland / XWayland

Primary desktop environment is Ubuntu 24.04 with Wayland.

Native Wayland:

```text
application-controlled always-on-top is not relied upon
```

XWayland/X11 compatibility mode may be used for Pin / always-on-top.

Do not introduce:

* focus stealing loops
* repeated raise calls
* `wmctrl` hacks
* `xdotool` hacks
* global keyboard hooks
* compositor-specific hacks

Always-on-top must never mean repeatedly stealing keyboard focus.

MarkText must remain able to receive typing while the Monitor stays visible above it.

---

# DOCX

DOCX support is planned but is **not part of the current core milestone**.

Current direction:

```text
Markdown = native first-class format
DOCX = later import/export support
```

Do not implement native DOCX editing unless explicitly requested.

Do not introduce Pandoc, Mammoth, docx.js, or other DOCX dependencies preemptively.

---

# Tiru Desktop Pet

The future Tiru desktop pet is a consumer of Writing Core state.

It must not become the source of writing statistics.

Expected future state interface may expose values such as:

```text
state
since_last_edit
session_span
session_active
session_idle
session_writing_net
document
```

The pet does not need正文 content.

Do not expose正文 text to the pet layer unless explicitly requested.

---

# Scope Discipline

Do not proactively add:

* AI writing
* cloud sync
* accounts
* collaboration
* Scrivener-like project management
* chapter trees
* dashboards
* streak systems
* writing goals
* tray integration
* autostart
* DOCX editing
* Tiru integration

unless the current task explicitly asks for them.

Prefer the smallest change that solves the current task.

Do not refactor stable Writing Core logic while performing unrelated UI work.

---

# Testing

Before declaring a core change complete, run the applicable checks.

Preferred commands:

```bash
npm run build
cargo fmt --check
cargo test
cargo check --release
```

The Ubuntu development environment may require WebKitGTK/libsoup/javascriptcoregtk development libraries or the existing development sysroot arrangement.

If tests are blocked by missing system dependencies:

* report the dependency failure accurately;
* do not claim PASS;
* do not rewrite working code merely to avoid the environment issue.

For GUI behavior such as:

* MarkText autosave behavior
* actual always-on-top
* focus behavior
* window-state restoration
* IME behavior

do not fabricate success from unit tests.

Prepare the runtime and request human verification where visual/manual confirmation is required.

---

# Regression Rules

Do not regress previously verified behavior:

* Markdown remains the正文 source of truth.
* atomic replace must not cause full-document insertion spikes.
* duplicate notify events must not double-count.
* `active` must not blindly equal `span`.
* accumulated idle must survive resume.
* RAW and Writing counts remain separate.
* initial baseline must not count the whole document as newly written.
* opening the app must not create a fake writing session.
* window restore must not be overwritten by startup defaults.

---

# Working Style

Before changing code:

1. inspect the existing implementation;
2. identify the actual data/control path;
3. preserve already-verified behavior;
4. make the smallest necessary change;
5. run relevant checks;
6. report what was actually verified.

When debugging runtime behavior, trace:

```text
Core
→ Tauri
→ React
→ visible UI
```

instead of assuming that passing unit tests proves the full GUI path.

Do not silently expand task scope.

---

# Current Project Status

At the time this file was created:

* File Watch tracking works with MarkText.
* MarkText autosave is observed at roughly several-second intervals and is sufficient for normal tracking.
* atomic replace handling has been corrected.
* active/idle/session calculations have passed manual testing.
* RAW Unicode scalar statistics are retained.
* Writing Count v1 has passed manual testing.
* compact Writing Monitor is implemented.
* XWayland always-on-top has passed manual testing.
* window size/position persistence has been repaired and is ready/accepted for normal use.
* the project is ready to leave the initial spike/debug-heavy phase.

Treat these as established behavior unless a later task explicitly changes them.

## Execution Economy

Minimize unnecessary context, tool calls, and verification.

1. Scope first.
   Use `rg`, `rg --files`, filenames, symbols, and targeted searches before reading files.
   Do not scan the whole repository unless the task requires it.

2. Read narrowly.
   For large files, read only relevant ranges.
   Do not `cat` large files merely for orientation.

3. Keep command output small.
   For large logs/test output, prefer targeted error matching or bounded tails.
   Do not load thousands of irrelevant output lines into context.

4. Inspect diffs progressively.
   Prefer `git status`, `git diff --stat`, and `git diff --name-only` first.
   Read full diffs only for relevant files.

5. Use the smallest sufficient validation.
   For a localized fix, run the directly relevant test/check once.
   If it passes, stop.
   Run the full test suite only when the change is cross-cutting, high-risk,
   or explicitly requested.

6. Do not perform repeated "safety" reviews after successful validation.
   Do not re-read unchanged files or repeat successful commands without evidence.

7. If a fix fails, re-diagnose from the new evidence.
   Do not repeat the same attempted fix or verification loop.

8. Stay in scope.
   Do not fix unrelated issues, refactor adjacent code, audit the repository,
   or proactively expand the task unless necessary.

9. Use extra tools selectively.
   Do not invoke subagents, expert agents, MCP servers, web search, or skills
   unless they materially help the current task.

10. Stop when done.
    Once the requested behavior is implemented and the necessary validation passes,
    report the result and end the task.
