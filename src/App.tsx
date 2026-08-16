import { useEffect, useRef, useState } from "react";
import { defaultKeymap, history, historyKeymap, indentWithTab } from "@codemirror/commands";
import { markdown } from "@codemirror/lang-markdown";
import { EditorState, Transaction } from "@codemirror/state";
import {
  drawSelection,
  dropCursor,
  EditorView,
  highlightActiveLine,
  keymap,
  lineNumbers,
  type ViewUpdate,
} from "@codemirror/view";
import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { open } from "@tauri-apps/plugin-dialog";
import {
  filename as windowStateFilename,
  saveWindowState,
  StateFlags,
} from "@tauri-apps/plugin-window-state";
import "./App.css";

type ViewMode = "monitor" | "editor";
type MonitorViewMode = "compact" | "advanced";
type CoreState = "waiting" | "active" | "idle" | "ended";

type CoreStatus = {
  document_id?: string | null;
  session_id?: string | null;
  state: CoreState;
  since_last_edit_seconds: number;
  session_span_seconds: number;
  session_active_seconds: number;
  session_idle_seconds: number;
  session_inserted_chars: number;
  session_deleted_chars: number;
  session_net_chars: number;
  session_writing_inserted_units?: number | null;
  session_writing_deleted_units?: number | null;
  session_writing_net_units?: number | null;
  session_writing_count_version?: number | null;
  document_writing_count?: number | null;
  tracking_origin?: string | null;
};

type RuntimeTimingConfig = {
  idle_threshold_seconds: number;
  session_end_threshold_seconds: number;
  mode: "DEVELOPMENT" | "PRODUCTION" | string;
  source: string;
  adjustable: boolean;
};

type DocumentSnapshot = {
  document_id: string;
  path: string;
  content: string;
  hash: string;
  mtime_ms: number;
  size: number;
  recovery_path?: string | null;
};

type MonitorDocument = Omit<DocumentSnapshot, "content" | "recovery_path">;

type EditEvent = {
  document_id: string;
  timestamp_ms: number;
  source: string;
  tracking_origin: string;
  inserted_chars: number;
  deleted_chars: number;
  writing_inserted_units?: number | null;
  writing_deleted_units?: number | null;
  writing_count_version?: number | null;
  is_user_edit: boolean;
  ime_composition: boolean;
  event_id: string;
  change_count: number;
};

type DebugEvent = EditEvent & {
  preview: string;
};

type WritingDelta = {
  inserted_units: number;
  deleted_units: number;
  net_units: number;
  count_version: number;
};

type ExternalModification = {
  path: string;
};

type FileDiffPayload = {
  path: string;
  event: EditEvent;
  status: CoreStatus;
};

type WatchErrorPayload = {
  path: string;
  message: string;
};

type WatchDiagnostics = {
  path?: string | null;
  document_id?: string | null;
  baseline_chars?: number | null;
  current_file_chars?: number | null;
  last_good_hash_prefix?: string | null;
  watcher_state: string;
  pending_atomic_replace: boolean;
};

type WindowCapability = {
  backend: string;
  supported: boolean;
  reason: string;
};

type PinState = WindowCapability & {
  requested: boolean;
  effective: boolean;
  error?: string;
};

type StoredWindowState = {
  width: number;
  height: number;
  x: number;
  y: number;
  prev_x: number;
  prev_y: number;
  maximized: boolean;
  visible: boolean;
  decorated: boolean;
  fullscreen: boolean;
};

type WindowStateDiagnostics = {
  backend: string;
  label: string;
  current_width: number;
  current_height: number;
  current_x: number;
  current_y: number;
  state_filename: string;
  state_path: string;
  stored_state_exists: boolean;
  stored_state: StoredWindowState | null;
  stored_state_parse_error: string | null;
};

const emptyStatus: CoreStatus = {
  document_id: null,
  session_id: null,
  state: "waiting",
  since_last_edit_seconds: 0,
  session_span_seconds: 0,
  session_active_seconds: 0,
  session_idle_seconds: 0,
  session_inserted_chars: 0,
  session_deleted_chars: 0,
  session_net_chars: 0,
  session_writing_inserted_units: null,
  session_writing_deleted_units: null,
  session_writing_net_units: null,
  session_writing_count_version: null,
  document_writing_count: null,
  tracking_origin: null,
};

const emptyPin: PinState = {
  backend: "unknown",
  supported: false,
  reason: "正在检测窗口后端…",
  requested: false,
  effective: false,
};

const emptyWatchDiagnostics: WatchDiagnostics = {
  path: null,
  document_id: null,
  baseline_chars: null,
  current_file_chars: null,
  last_good_hash_prefix: null,
  watcher_state: "stopped",
  pending_atomic_replace: false,
};

function countUnicodeScalars(value: string): number {
  // JS Array.from(value).length == Rust str.chars().count().
  return Array.from(value).length;
}

function classifySource(rawSource: string, inserted: number, deleted: number): string {
  if (rawSource === "undo" || rawSource.startsWith("undo.")) return "undo";
  if (rawSource === "redo" || rawSource.startsWith("redo.")) return "redo";
  if (rawSource.startsWith("input.paste")) return "input.paste";
  if (rawSource.startsWith("delete.cut")) return "delete.cut";
  if (rawSource.startsWith("delete")) return rawSource;
  if (rawSource.startsWith("input") && inserted > 0 && deleted > 0) return "input.replace";
  if (rawSource.startsWith("input")) return rawSource;
  if (inserted > 0 || deleted > 0) return rawSource || "programmatic";
  return rawSource || "selection";
}

function formatDuration(seconds: number): string {
  const safeSeconds = Math.max(0, Math.floor(seconds));
  const hours = Math.floor(safeSeconds / 3600);
  const minutes = Math.floor((safeSeconds % 3600) / 60);
  const remainder = safeSeconds % 60;
  return [hours, minutes, remainder].map((part) => String(part).padStart(2, "0")).join(":");
}

function formatSigned(value: number | null | undefined): string {
  if (value === null || value === undefined) return "—";
  return value > 0 ? `+${value}` : String(value);
}

function shortEventId(eventId: string): string {
  if (eventId.length <= 20) return eventId;
  return `${eventId.slice(0, 8)}…${eventId.slice(-8)}`;
}

function originLabel(origin: string | null | undefined): string {
  if (origin === "file_diff") return "FILE DIFF";
  if (origin === "editor_transaction") return "CODEMIRROR";
  if (origin === "mixed") return "MIXED";
  return "—";
}

function shortFileName(path: string): string {
  return path.split(/[\\/]/).pop() || path;
}

function compactStateLabel(state: CoreState): string {
  if (state === "active") return "ACTIVE";
  if (state === "idle") return "IDLE";
  if (state === "ended") return "ENDED";
  return "READY";
}

function CompactMetric({
  label,
  value,
  note,
  tone,
}: {
  label: string;
  value: string;
  note?: string;
  tone?: string;
}) {
  return (
    <div className={`compact-metric ${tone ? `metric-${tone}` : ""}`}>
      <span>{label}</span>
      <strong>{value}</strong>
      {note && <small>{note}</small>}
    </div>
  );
}

function createEditorState(
  document: string,
  compositionActive: { current: boolean },
  onEdit: (update: ViewUpdate) => void,
): EditorState {
  return EditorState.create({
    doc: document,
    extensions: [
      markdown(),
      lineNumbers(),
      highlightActiveLine(),
      drawSelection(),
      dropCursor(),
      EditorView.lineWrapping,
      history(),
      keymap.of([...defaultKeymap, ...historyKeymap, indentWithTab]),
      EditorView.domEventHandlers({
        compositionstart: () => {
          compositionActive.current = true;
        },
        compositionend: () => {
          compositionActive.current = false;
        },
      }),
      EditorView.updateListener.of(onEdit),
    ],
  });
}

function App() {
  const editorHostRef = useRef<HTMLDivElement>(null);
  const editorViewRef = useRef<EditorView | null>(null);
  const pendingEditorPathRef = useRef<string | null>(null);
  const documentIdRef = useRef<string | null>(null);
  const documentPathRef = useRef<string | null>(null);
  const monitorPathRef = useRef<string | null>(null);
  const compositionActiveRef = useRef(false);
  const loadingRef = useRef(false);
  const saveTimerRef = useRef<number | undefined>(undefined);
  const windowStateSaveTimerRef = useRef<number | undefined>(undefined);
  const eventSequenceRef = useRef(0);
  const recordQueueRef = useRef<Promise<void>>(Promise.resolve());
  const seenEventIdsRef = useRef(new Set<string>());

  const [mode, setMode] = useState<ViewMode>("monitor");
  const [monitorViewMode, setMonitorViewMode] = useState<MonitorViewMode>("compact");
  const [monitorPath, setMonitorPath] = useState("");
  const [status, setStatus] = useState<CoreStatus>(emptyStatus);
  const [events, setEvents] = useState<DebugEvent[]>([]);
  const [saveState, setSaveState] = useState("尚未选择监控文件");
  const [externalPath, setExternalPath] = useState<string | null>(null);
  const [watchError, setWatchError] = useState<string | null>(null);
  const [watchDiagnostics, setWatchDiagnostics] =
    useState<WatchDiagnostics>(emptyWatchDiagnostics);
  const [pin, setPin] = useState<PinState>(emptyPin);
  const [runtimeTiming, setRuntimeTiming] = useState<RuntimeTimingConfig | null>(null);
  const [idleTimingInput, setIdleTimingInput] = useState("10");
  const [sessionEndTimingInput, setSessionEndTimingInput] = useState("60");
  const [timingError, setTimingError] = useState<string | null>(null);
  const [windowStateDiagnostics, setWindowStateDiagnostics] =
    useState<WindowStateDiagnostics | null>(null);
  const [windowStateSaveStatus, setWindowStateSaveStatus] = useState("尚未保存");
  const [windowStateRestoreStatus, setWindowStateRestoreStatus] = useState("尚未读取");
  const [windowStateError, setWindowStateError] = useState<string | null>(null);

  function appendDebugEvent(event: DebugEvent, received = false) {
    if (received && import.meta.env.DEV) {
      console.debug("[REACT RECEIVED]", {
        event_id: event.event_id,
        inserted: event.inserted_chars,
        deleted: event.deleted_chars,
        source: event.source,
      });
    }
    if (seenEventIdsRef.current.has(event.event_id)) {
      if (received && import.meta.env.DEV) {
        console.debug("[REACT DUPLICATE IGNORED]", { event_id: event.event_id });
      }
      return;
    }
    seenEventIdsRef.current.add(event.event_id);
    setEvents((current) => [event, ...current].slice(0, 100));
  }

  function receiveStatus(source: string, nextStatus: CoreStatus) {
    if (import.meta.env.DEV) {
      console.debug("[REACT RECEIVED]", {
        source,
        span: nextStatus.session_span_seconds,
        active: nextStatus.session_active_seconds,
        idle: nextStatus.session_idle_seconds,
        since_last_edit: nextStatus.since_last_edit_seconds,
        state: nextStatus.state,
        document_id: nextStatus.document_id,
        session_id: nextStatus.session_id,
      });
    }
    setStatus(nextStatus);
  }

  function queueEditorEvent(
    event: EditEvent,
    oldMarkdown: string,
    newMarkdown: string,
    preview: string,
  ) {
    recordQueueRef.current = recordQueueRef.current
      .catch(() => undefined)
      .then(async () => {
        let nextEvent = event;
        try {
          const delta = await invoke<WritingDelta>("get_writing_delta", {
            oldMarkdown,
            newMarkdown,
          });
          nextEvent = {
            ...event,
            writing_inserted_units: delta.inserted_units,
            writing_deleted_units: delta.deleted_units,
            writing_count_version: delta.count_version,
          };
        } catch (error) {
          // RAW remains recordable, but Writing must stay unavailable rather
          // than silently inheriting the scalar count.
          setSaveState(`写作统计不可用（RAW 仍记录）: ${String(error)}`);
        }
        appendDebugEvent({ ...nextEvent, preview });
        const nextStatus = await invoke<CoreStatus>("record_edit", { event: nextEvent });
        receiveStatus("record_edit", nextStatus);
      })
      .catch((error: unknown) => {
        setSaveState(`统计错误: ${String(error)}`);
      });
  }

  async function applyDebugTiming() {
    const idle = Number(idleTimingInput);
    const sessionEnd = Number(sessionEndTimingInput);
    if (!Number.isInteger(idle) || !Number.isInteger(sessionEnd) || idle <= 0) {
      setTimingError("Idle threshold 必须是大于 0 的整数。");
      return;
    }
    if (sessionEnd <= idle) {
      setTimingError("Session end 必须大于 idle threshold。");
      return;
    }

    try {
      const nextTiming = await invoke<RuntimeTimingConfig>("apply_debug_timing", {
        idleThresholdSeconds: idle,
        sessionEndThresholdSeconds: sessionEnd,
      });
      setRuntimeTiming(nextTiming);
      setTimingError(null);
      if (documentIdRef.current) {
        receiveStatus("apply_debug_timing", await invoke<CoreStatus>("get_core_status"));
      }
    } catch (error) {
      setTimingError(String(error));
    }
  }

  async function saveDocument() {
    const view = editorViewRef.current;
    const documentId = documentIdRef.current;
    const path = documentPathRef.current;
    if (!view || !documentId || !path) return;

    setSaveState("保存中…");
    try {
      await invoke("save_document", {
        path,
        documentId,
        content: view.state.doc.toString(),
      });
      setSaveState(`已安全保存 ${new Date().toLocaleTimeString("zh-CN", { hour12: false })}`);
    } catch (error) {
      setSaveState(`保存失败: ${String(error)}`);
    }
  }

  function scheduleSave() {
    if (saveTimerRef.current !== undefined) window.clearTimeout(saveTimerRef.current);
    saveTimerRef.current = window.setTimeout(() => {
      void saveDocument();
    }, 1200);
    setSaveState("待保存");
  }

  function handleEditorUpdate(update: ViewUpdate) {
    if (loadingRef.current || !update.docChanged || !documentIdRef.current) return;

    for (const transaction of update.transactions) {
      if (!transaction.docChanged) continue;

      let insertedChars = 0;
      let deletedChars = 0;
      const insertedPreview: string[] = [];
      let changeCount = 0;
      transaction.changes.iterChanges((fromA, toA, _fromB, _toB, inserted) => {
        changeCount += 1;
        deletedChars += countUnicodeScalars(transaction.startState.doc.sliceString(fromA, toA));
        const insertedText = inserted.toString();
        insertedChars += countUnicodeScalars(insertedText);
        if (insertedPreview.join("").length < 100) insertedPreview.push(insertedText);
      });

      const rawSource = transaction.annotation(Transaction.userEvent) ?? "programmatic";
      const source = classifySource(rawSource, insertedChars, deletedChars);
      const isUserEdit =
        transaction.isUserEvent("input") ||
        transaction.isUserEvent("delete") ||
        transaction.isUserEvent("undo") ||
        transaction.isUserEvent("redo") ||
        transaction.isUserEvent("move.drop");
      if (!isUserEdit) continue;

      const timestampMs = Date.now();
      const event: EditEvent = {
        document_id: documentIdRef.current,
        timestamp_ms: timestampMs,
        source,
        tracking_origin: "editor_transaction",
        inserted_chars: insertedChars,
        deleted_chars: deletedChars,
        is_user_edit: true,
        ime_composition:
          compositionActiveRef.current ||
          rawSource.includes("compose") ||
          editorViewRef.current?.composing === true,
        event_id: `tx-${timestampMs}-${eventSequenceRef.current++}`,
        change_count: changeCount,
      };

      queueEditorEvent(
        event,
        transaction.startState.doc.toString(),
        transaction.newDoc.toString(),
        insertedPreview.join("").slice(0, 100),
      );
    }

    scheduleSave();
  }

  async function stopTracking() {
    try {
      await invoke("stop_tracking");
      seenEventIdsRef.current.clear();
      documentIdRef.current = null;
      documentPathRef.current = null;
      monitorPathRef.current = null;
      setMonitorPath("");
      setStatus(emptyStatus);
      setWatchDiagnostics(emptyWatchDiagnostics);
      setExternalPath(null);
      setWatchError(null);
      setSaveState("尚未选择监控文件");
      setEvents([]);
      setMonitorViewMode("compact");
    } catch (error) {
      setWatchError(`停止监控失败: ${String(error)}`);
    }
  }

  async function startMonitor() {
    const selected = await open({
      multiple: false,
      directory: false,
      filters: [{ name: "Markdown", extensions: ["md", "markdown"] }],
    });
    if (typeof selected !== "string") return;

    try {
      if (mode === "editor" || documentIdRef.current) await invoke("stop_tracking");
      const document = await invoke<MonitorDocument>("start_monitor", { path: selected });
      documentIdRef.current = document.document_id;
      documentPathRef.current = document.path;
      monitorPathRef.current = document.path;
      setMode("monitor");
      setMonitorViewMode("compact");
      setMonitorPath(document.path);
      setEvents([]);
      setExternalPath(null);
      setWatchError(null);
      seenEventIdsRef.current.clear();
      receiveStatus("start_monitor", await invoke<CoreStatus>("get_core_status"));
      setWatchDiagnostics(await invoke<WatchDiagnostics>("get_watch_diagnostics"));
      setRuntimeTiming(await invoke<RuntimeTimingConfig>("get_runtime_config"));
      setSaveState("已建立 baseline；等待 Markdown 保存变化");
    } catch (error) {
      setWatchError(`启动监控失败: ${String(error)}`);
    }
  }

  async function enterEditorMode() {
    if (mode === "monitor" && documentIdRef.current) await stopTracking();
    setMode("editor");
  }

  async function openEditorDocument() {
    const selected = await open({
      multiple: false,
      directory: false,
      filters: [{ name: "Markdown", extensions: ["md", "markdown"] }],
    });
    if (typeof selected !== "string") return;
    try {
      await invoke("stop_tracking");
      seenEventIdsRef.current.clear();
      documentIdRef.current = null;
      pendingEditorPathRef.current = selected;
      setEvents([]);
      setExternalPath(null);
      setWatchError(null);
      setMode("editor");
      setSaveState("正在打开 CodeMirror Debug 文件…");
    } catch (error) {
      setWatchError(`切换编辑器失败: ${String(error)}`);
    }
  }

  async function loadEditorDocument(path: string, view: EditorView) {
    try {
      const snapshot = await invoke<DocumentSnapshot>("open_document", { path });
      loadingRef.current = true;
      view.setState(createEditorState(snapshot.content, compositionActiveRef, handleEditorUpdate));
      loadingRef.current = false;
      documentIdRef.current = snapshot.document_id;
      documentPathRef.current = snapshot.path;
      setMonitorPath("");
      setExternalPath(null);
      setEvents([]);
      seenEventIdsRef.current.clear();
      receiveStatus("open_document", await invoke<CoreStatus>("get_core_status"));
      setWatchDiagnostics(await invoke<WatchDiagnostics>("get_watch_diagnostics"));
      setRuntimeTiming(await invoke<RuntimeTimingConfig>("get_runtime_config"));
      setSaveState(
        snapshot.recovery_path
          ? `发现恢复文件：${snapshot.recovery_path}`
          : "已打开，正文未修改",
      );
      view.focus();
    } catch (error) {
      setSaveState(`打开失败: ${String(error)}`);
    }
  }

  useEffect(() => {
    void invoke<RuntimeTimingConfig>("get_runtime_config")
      .then(setRuntimeTiming)
      .catch((error) => setTimingError(`读取 runtime timing 失败: ${String(error)}`));
  }, []);

  useEffect(() => {
    if (mode !== "editor" || !editorHostRef.current) return;
    const view = new EditorView({
      state: createEditorState("", compositionActiveRef, handleEditorUpdate),
      parent: editorHostRef.current,
    });
    editorViewRef.current = view;
    const pendingPath = pendingEditorPathRef.current;
    pendingEditorPathRef.current = null;
    if (pendingPath) void loadEditorDocument(pendingPath, view);
    return () => {
      if (saveTimerRef.current !== undefined) window.clearTimeout(saveTimerRef.current);
      view.destroy();
      editorViewRef.current = null;
    };
  }, [mode]);

  useEffect(() => {
    let stopFileDiff: (() => void) | undefined;
    let stopExternal: (() => void) | undefined;
    let stopWatchError: (() => void) | undefined;
    let active = true;
    void listen<FileDiffPayload>("file-diff-event", ({ payload }) => {
      if (payload.path !== monitorPathRef.current) return;
      appendDebugEvent({ ...payload.event, preview: "已保存正文变化" }, true);
      receiveStatus("file-diff-event", payload.status);
      setSaveState("检测到保存后的正文变化");
    }).then((unlisten) => {
      if (active) stopFileDiff = unlisten;
      else unlisten();
    });
    void listen<ExternalModification>("external-modification", ({ payload }) => {
      if (payload.path === documentPathRef.current) setExternalPath(payload.path);
    }).then((unlisten) => {
      if (active) stopExternal = unlisten;
      else unlisten();
    });
    void listen<WatchErrorPayload>("file-watch-error", ({ payload }) => {
      if (payload.path === monitorPathRef.current || payload.path === documentPathRef.current) {
        setWatchError(`${payload.path}: ${payload.message}`);
      }
    }).then((unlisten) => {
      if (active) stopWatchError = unlisten;
      else unlisten();
    });
    return () => {
      active = false;
      stopFileDiff?.();
      stopExternal?.();
      stopWatchError?.();
    };
  }, []);

  useEffect(() => {
    const timer = window.setInterval(() => {
      if (!documentIdRef.current) return;
      void Promise.all([
        invoke<CoreStatus>("get_core_status"),
        invoke<WatchDiagnostics>("get_watch_diagnostics"),
      ])
        .then(([nextStatus, nextDiagnostics]) => {
          receiveStatus("poll", nextStatus);
          setWatchDiagnostics(nextDiagnostics);
        })
        .catch(() => undefined);
    }, 1000);
    return () => window.clearInterval(timer);
  }, []);

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const capability = await invoke<WindowCapability>("get_window_capability");
        const stored = localStorage.getItem("monitor_always_on_top_preference");
        const requested = stored ? Boolean(JSON.parse(stored).requested) : false;
        let effective = false;
        let error: string | undefined;
        if (capability.supported && requested) {
          try {
            const window = getCurrentWindow();
            await window.setAlwaysOnTop(true);
            effective = await window.isAlwaysOnTop();
          } catch (reason) {
            error = String(reason);
          }
        }
        if (!cancelled) setPin({ ...capability, requested, effective, error });
      } catch (error) {
        if (!cancelled) setPin({ ...emptyPin, reason: `置顶能力检测失败: ${String(error)}` });
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    const mainWindow = getCurrentWindow();
    let active = true;
    let stopResize: (() => void) | undefined;
    let stopMove: (() => void) | undefined;

    void Promise.all([
      mainWindow.onResized(() => scheduleWindowStateSave("resize")),
      mainWindow.onMoved(() => scheduleWindowStateSave("move")),
    ]).then(([unlistenResize, unlistenMove]) => {
      if (active) {
        stopResize = unlistenResize;
        stopMove = unlistenMove;
      } else {
        unlistenResize();
        unlistenMove();
      }
    });
    void refreshWindowStateDiagnostics(true);

    return () => {
      active = false;
      stopResize?.();
      stopMove?.();
      if (windowStateSaveTimerRef.current !== undefined) {
        window.clearTimeout(windowStateSaveTimerRef.current);
      }
    };
  }, []);

  async function togglePin(requested: boolean) {
    if (!pin.supported) return;
    try {
      const window = getCurrentWindow();
      await window.setAlwaysOnTop(requested);
      const effective = await window.isAlwaysOnTop();
      localStorage.setItem(
        "monitor_always_on_top_preference",
        JSON.stringify({ requested }),
      );
      setPin((current) => ({ ...current, requested, effective, error: undefined }));
    } catch (error) {
      setPin((current) => ({ ...current, requested, effective: false, error: String(error) }));
    }
  }

  async function refreshWindowStateDiagnostics(startup = false): Promise<void> {
    try {
      const stateFilename = await windowStateFilename();
      const diagnostics = await invoke<WindowStateDiagnostics>("get_window_state_diagnostics", {
        stateFilename,
      });
      setWindowStateDiagnostics(diagnostics);
      setWindowStateError(null);
      if (startup) {
        setWindowStateRestoreStatus(
          `自动 restore 后读取完成 ${new Date().toLocaleTimeString("zh-CN", { hour12: false })}`,
        );
        console.debug("[WINDOW STATE] frontend mounted", diagnostics);
      }
    } catch (error) {
      setWindowStateError(String(error));
      if (startup) setWindowStateRestoreStatus(`读取失败: ${String(error)}`);
    }
  }

  function scheduleWindowStateSave(reason: "resize" | "move") {
    if (windowStateSaveTimerRef.current !== undefined) {
      window.clearTimeout(windowStateSaveTimerRef.current);
    }
    setWindowStateSaveStatus(`待保存（${reason} 停止 750ms）`);
    windowStateSaveTimerRef.current = window.setTimeout(() => {
      void (async () => {
        try {
          await saveWindowState(StateFlags.SIZE | StateFlags.POSITION);
          await refreshWindowStateDiagnostics();
          setWindowStateSaveStatus(
            `已保存 ${new Date().toLocaleTimeString("zh-CN", { hour12: false })}`,
          );
          console.debug("[WINDOW STATE] debounced save completed");
        } catch (error) {
          setWindowStateSaveStatus(`保存失败: ${String(error)}`);
          setWindowStateError(String(error));
        }
      })();
    }, 750);
  }

  const isEditor = mode === "editor";
  const isAdvanced = !isEditor && monitorViewMode === "advanced";

  return (
    <main
      className={`spike-shell ${
        isEditor ? "editor-shell" : isAdvanced ? "advanced-shell" : "compact-shell"
      }`}
    >
      {isEditor ? (
        <>
          <header className="toolbar">
            <div>
              <span className="eyebrow">CodeMirror Debug</span>
              <h1>CodeMirror Debug</h1>
              <p className="path" title={documentPathRef.current ?? ""}>
                {documentPathRef.current || "尚未打开 Debug Markdown 文件"}
              </p>
            </div>
            <div className="toolbar-actions">
              <button type="button" onClick={() => void startMonitor()}>
                选择并监控 Markdown
              </button>
              <button type="button" className="selected" onClick={() => void openEditorDocument()}>
                打开 Markdown
              </button>
            </div>
          </header>

          <div className="toolbar editor-toolbar">
            <div>
              <span className="eyebrow">Optional precision adapter</span>
              <strong>CodeMirror transaction → editor_transaction</strong>
            </div>
            <div className="toolbar-actions">
              <button type="button" onClick={() => void saveDocument()} disabled={!documentPathRef.current}>
                立即安全保存
              </button>
            </div>
          </div>
        </>
      ) : isAdvanced ? (
        <>
          <header className="toolbar advanced-toolbar">
            <div>
              <span className="eyebrow">Writing Monitor / Advanced</span>
              <h1>Writing Monitor / 写作监视器</h1>
              <p className="path" title={monitorPath}>{monitorPath || "尚未选择要监控的 Markdown 文件"}</p>
            </div>
            <div className="toolbar-actions">
              <button type="button" onClick={() => setMonitorViewMode("compact")}>
                简洁模式
              </button>
              <button type="button" onClick={() => void startMonitor()}>
                更换文件
              </button>
              <button type="button" onClick={() => void enterEditorMode()}>
                CodeMirror Debug
              </button>
            </div>
          </header>

          <section className="monitor-toolbar">
            <div>
              <span className="eyebrow">File Watch Adapter</span>
              <strong>{monitorPath ? "正在监听父目录中的目标文件" : "等待选择文件"}</strong>
              <p>首次读取只建立 baseline；只有保存后的真实正文变化才会生成 file.diff。</p>
            </div>
            <div className="monitor-actions">
              <button type="button" onClick={() => void startMonitor()}>
                选择文件并开始
              </button>
              <button type="button" onClick={() => void stopTracking()} disabled={!monitorPath}>
                停止监控
              </button>
            </div>
          </section>

          <section className="pin-panel">
            <div>
              <span className="eyebrow">Window capability</span>
              <strong>
                {pin.backend === "native_wayland"
                  ? "置顶：当前 Wayland 不支持"
                  : `置顶：${pin.supported ? (pin.effective ? "已生效" : "未开启") : "当前不可用"}`}
              </strong>
              <p>{pin.supported ? `${pin.backend}：${pin.reason}` : `Native Wayland：${pin.reason}`}</p>
            </div>
            <label className="pin-toggle">
              <input
                type="checkbox"
                checked={pin.requested}
                disabled={!pin.supported}
                onChange={(event) => void togglePin(event.currentTarget.checked)}
            />
            请求窗口置顶
          </label>
          {pin.error && <small className="pin-error">API 错误：{pin.error}</small>}
          </section>

          <section className="window-state-panel">
            <div className="panel-heading">
              <div>
                <span className="eyebrow">WINDOW STATE DEBUG</span>
                <h2>窗口状态持久化</h2>
              </div>
              <div className="window-state-status">
                <span>Last save: {windowStateSaveStatus}</span>
                <span>Last restore: {windowStateRestoreStatus}</span>
              </div>
            </div>
            {windowStateDiagnostics ? (
              <div className="window-state-grid">
                <div><span>Window backend</span><strong>{windowStateDiagnostics.backend}</strong></div>
                <div><span>Window label</span><strong>{windowStateDiagnostics.label}</strong></div>
                <div>
                  <span>Current size</span>
                  <strong>{windowStateDiagnostics.current_width} × {windowStateDiagnostics.current_height}</strong>
                </div>
                <div>
                  <span>Current position</span>
                  <strong>({windowStateDiagnostics.current_x}, {windowStateDiagnostics.current_y})</strong>
                </div>
                <div><span>State filename</span><strong>{windowStateDiagnostics.state_filename}</strong></div>
                <div><span>Stored state exists</span><strong>{windowStateDiagnostics.stored_state_exists ? "yes" : "no"}</strong></div>
                <div className="window-state-wide">
                  <span>State path</span>
                  <strong>{windowStateDiagnostics.state_path}</strong>
                </div>
                <div className="window-state-wide">
                  <span>Stored size / position</span>
                  <strong>
                    {windowStateDiagnostics.stored_state
                      ? `${windowStateDiagnostics.stored_state.width} × ${windowStateDiagnostics.stored_state.height} @ (${windowStateDiagnostics.stored_state.x}, ${windowStateDiagnostics.stored_state.y})`
                      : windowStateDiagnostics.stored_state_parse_error || "无 main 状态"}
                  </strong>
                </div>
              </div>
            ) : (
              <p className="window-state-empty">正在读取窗口状态…</p>
            )}
            {windowStateError && <small className="pin-error">Window State API 错误：{windowStateError}</small>}
          </section>
        </>
      ) : (
        <>
          <header className="compact-header">
            <div className="compact-brand">
              <span className="eyebrow">Writing Monitor</span>
              <h1>Writing Monitor</h1>
              {monitorPath && (
                <p className="compact-file" title={monitorPath}>{shortFileName(monitorPath)}</p>
              )}
            </div>
            {monitorPath && (
              <span className={`compact-state state-${status.state}`}>
                <i aria-hidden="true" />{compactStateLabel(status.state)}
              </span>
            )}
          </header>

          {!monitorPath ? (
            <section className="compact-empty">
              <p>选择一个正文 Markdown 文件开始监控</p>
              <button type="button" className="primary-button" onClick={() => void startMonitor()}>
                选择并监控 Markdown
              </button>
              <button type="button" className="quiet-button" onClick={() => setMonitorViewMode("advanced")}>
                高级模式
              </button>
            </section>
          ) : (
            <>
              <section className="compact-metrics" aria-label="Writing Monitor 核心数据">
                <CompactMetric
                  label="总时间"
                  value={formatDuration(status.session_span_seconds)}
                  tone="span"
                />
                <CompactMetric
                  label="休息时间"
                  value={formatDuration(status.session_idle_seconds)}
                  tone="idle"
                />
                <CompactMetric
                  label="实际码字时间"
                  value={formatDuration(status.session_active_seconds)}
                  tone="active"
                />
                <CompactMetric
                  label="码字字数"
                  value={formatSigned(status.session_writing_net_units)}
                  note={
                    status.document_writing_count === null || status.document_writing_count === undefined
                      ? undefined
                      : `当前正文 ${status.document_writing_count} 字`
                  }
                  tone="count"
                />
              </section>

              <div className="compact-actions">
                <button type="button" onClick={() => void startMonitor()}>
                  更换文件
                </button>
                <button type="button" onClick={() => void stopTracking()}>
                  停止监控
                </button>
                <button type="button" className="primary-button" onClick={() => setMonitorViewMode("advanced")}>
                  高级模式
                </button>
              </div>
            </>
          )}
        </>
      )}

      {isEditor && externalPath && (
        <div className="warning" role="alert">
          检测到外部修改：{externalPath}。CodeMirror Debug 不会静默覆盖它，请先重新打开文件。
        </div>
      )}
      {watchError && (
        <div className="warning" role="alert">
          文件监控错误：{watchError}
        </div>
      )}

      {isEditor && <section className="editor-panel"><div ref={editorHostRef} className="editor-host" /></section>}

      {(isEditor || isAdvanced) && (
        <>
          <section className="status-grid" aria-label="Writing Core status">
            <div>
              <span>state</span>
              <strong className={`state-${status.state}`}>{status.state}</strong>
            </div>
            <div>
              <span>since last edit</span>
              <strong>{formatDuration(status.since_last_edit_seconds)}</strong>
            </div>
            <div>
              <span>session span</span>
              <strong>{formatDuration(status.session_span_seconds)}</strong>
            </div>
            <div>
              <span>active writing</span>
              <strong>{formatDuration(status.session_active_seconds)}</strong>
            </div>
            <div>
              <span>session idle</span>
              <strong>{formatDuration(status.session_idle_seconds)}</strong>
            </div>
            <div>
              <span>writing + / -</span>
              <strong>
                {status.session_writing_inserted_units ?? "—"} / {status.session_writing_deleted_units ?? "—"}
              </strong>
            </div>
            <div>
              <span>writing net</span>
              <strong>{formatSigned(status.session_writing_net_units)}</strong>
            </div>
            <div>
              <span>document writing</span>
              <strong>{status.document_writing_count ?? "—"}</strong>
            </div>
            <div>
              <span>raw scalar + / -</span>
              <strong>{status.session_inserted_chars} / {status.session_deleted_chars}</strong>
            </div>
            <div>
              <span>raw scalar net</span>
              <strong>{formatSigned(status.session_net_chars)}</strong>
            </div>
            <div>
              <span>tracking</span>
              <strong>{originLabel(status.tracking_origin)}</strong>
            </div>
          </section>

          <section className="debug-panel">
            <div className="panel-heading">
              <div>
                <span className="eyebrow">{isEditor ? "Editor Transaction Debug Log" : "File Diff Debug Log"}</span>
                <h2>{isEditor ? "CodeMirror 精确编辑事务" : "已保存正文变化"}</h2>
              </div>
              <span className="save-state">{saveState}</span>
            </div>
            <div className="runtime-timing">
              <span className="eyebrow">RUNTIME TIMING</span>
              <div className="runtime-timing-values">
                <span>
                  Idle threshold: <strong>{runtimeTiming?.idle_threshold_seconds ?? "—"} s</strong>
                </span>
                <span>
                  Session end: <strong>{runtimeTiming?.session_end_threshold_seconds ?? "—"} s</strong>
                </span>
                <span>Mode: <strong>{runtimeTiming?.mode ?? "LOADING"}</strong></span>
                <span>Source: <strong>{runtimeTiming?.source ?? "—"}</strong></span>
              </div>
              {import.meta.env.DEV && (
                <form
                  className="dev-timing-controls"
                  onSubmit={(event) => {
                    event.preventDefault();
                    void applyDebugTiming();
                  }}
                >
                  <label>
                    Idle threshold
                    <input
                      inputMode="numeric"
                      value={idleTimingInput}
                      onChange={(event) => setIdleTimingInput(event.currentTarget.value)}
                    />
                  </label>
                  <label>
                    Session end
                    <input
                      inputMode="numeric"
                      value={sessionEndTimingInput}
                      onChange={(event) => setSessionEndTimingInput(event.currentTarget.value)}
                    />
                  </label>
                  <button type="submit">Apply</button>
                </form>
              )}
              {timingError && <small className="pin-error">Timing error: {timingError}</small>}
            </div>
            <p className="metric-note">
              WRITING 是面向小说正文的写作单位；RAW 是 Unicode scalar 技术账本。File Diff 统计保存前后观察到的变化，不推断两次保存之间的输入、撤销或粘贴。
            </p>
            <p className="metric-note">
              document_id={status.document_id || "—"} · session_id={status.session_id || "—"}
            </p>
            <p className="metric-note">
              watcher={watchDiagnostics.watcher_state} · pending_replace={
                watchDiagnostics.pending_atomic_replace ? "true" : "false"
              } · baseline_chars={watchDiagnostics.baseline_chars ?? "—"} · current_file_chars={
                watchDiagnostics.current_file_chars ?? "—"
              } · last_good_hash={watchDiagnostics.last_good_hash_prefix || "—"}
            </p>
            <div className="event-log">
              {events.length === 0 && (
                <div className="empty-log">
                  {isEditor ? "在 CodeMirror 中进行真实编辑后，事件会显示在这里。" : "用 MarkText 或 VS Code 保存监控文件后，file.diff 会显示在这里。"}
                </div>
              )}
              {events.map((event) => (
                <div className="event-row" key={event.event_id}>
                  <time>{new Date(event.timestamp_ms).toLocaleTimeString("zh-CN", { hour12: false })}</time>
                  <code title={event.event_id}>id={shortEventId(event.event_id)}</code>
                  <code>{event.source}</code>
                  <span>raw +{event.inserted_chars}</span>
                  <span>raw -{event.deleted_chars}</span>
                  <span>
                    writing +{event.writing_inserted_units ?? "—"} / -{event.writing_deleted_units ?? "—"}
                  </span>
                  {event.preview && <em>{event.preview}</em>}
                  <small>{originLabel(event.tracking_origin)}</small>
                  {event.ime_composition && <small>IME composition</small>}
                </div>
              ))}
            </div>
          </section>
        </>
      )}
    </main>
  );
}

export default App;
