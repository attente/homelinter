import { useCallback, useEffect, useRef, useState } from 'react';
import { api, resolveApiError } from './api';
import { Browse } from './components/Browse';
import { ConfigView } from './components/ConfigView';
import { Header } from './components/Header';
import { RunView } from './components/RunView';
import { ScanForm, ScanFormValues } from './components/ScanForm';
import { Toasts } from './components/Toasts';
import { EventSourceClient } from './events';
import { downloadText } from './lib/download';
import { elapsedLabel, opLine, stateLabel } from './lib/format';
import type { ExecutionReport, ScanProgress, Session, SseEvent, Suggestion } from './types';

type Phase = 'idle' | 'scanning' | 'browse' | 'running' | 'report';

interface AppState {
  phase: Phase;
  session: Session | null;
  suggestions: Suggestion[];
  progress: ScanProgress | null;
  runLog: string[];
  report: ExecutionReport | null;
  runError: string | null;
  busy: boolean;
  toasts: string[];
  configOpen: boolean;
  showScanForm: boolean;
}

const POLL_MS = 2000;

export default function App() {
  const [st, setSt] = useState<AppState>({
    phase: 'idle',
    session: null,
    suggestions: [],
    progress: null,
    runLog: [],
    report: null,
    runError: null,
    busy: false,
    toasts: [],
    configOpen: false,
    showScanForm: true,
  });
  const sseRef = useRef<EventSourceClient | null>(null);
  const stRef = useRef(st);
  stRef.current = st;

  const patch = useCallback((p: Partial<AppState>) => {
    setSt((prev) => ({ ...prev, ...p }));
  }, []);

  const pushToast = useCallback((msg: string) => {
    setSt((prev) => ({ ...prev, toasts: [...prev.toasts, `${msg}`] }));
    setTimeout(() => {
      setSt((prev) => ({ ...prev, toasts: prev.toasts.filter((t) => t !== msg) }));
    }, 4500);
  }, []);

  const dismissToast = useCallback((i: number) => {
    setSt((prev) => ({ ...prev, toasts: prev.toasts.filter((_, j) => j !== i) }));
  }, []);

  const refreshSuggestions = useCallback(async (): Promise<Suggestion[]> => {
    const list = await api.suggestions();
    setSt((prev) => ({ ...prev, suggestions: list.items }));
    return list.items;
  }, []);

  const enterBrowse = useCallback(async () => {
    try {
      await refreshSuggestions();
      setSt((prev) => ({ ...prev, phase: 'browse', showScanForm: false }));
    } catch (e: unknown) {
      pushToast(`cannot load suggestions: ${resolveApiError(e)}`);
      setSt((prev) => ({ ...prev, phase: 'idle' }));
    }
  }, [pushToast, refreshSuggestions]);

  const startScan = useCallback(
    async (values: ScanFormValues) => {
      setSt((prev) => ({
        ...prev,
        busy: true,
        phase: 'scanning',
        showScanForm: false,
        suggestions: [],
        progress: null,
        report: null,
        runLog: [],
        runError: null,
      }));
      try {
        const targetPaths: Record<string, string> = {};
        for (const line of values.targetPaths.split('\n')) {
          const trimmed = line.trim();
          if (trimmed.length === 0) continue;
          const eq = trimmed.indexOf('=');
          if (eq <= 0) {
            throw new Error(`invalid target path line (expected kind=template): ${trimmed}`);
          }
          targetPaths[trimmed.slice(0, eq).trim()] = trimmed.slice(eq + 1).trim();
        }
        await api.startScan({
          root: values.root.length > 0 ? values.root : undefined,
          hidden: values.hidden,
          target_paths: Object.keys(targetPaths).length > 0 ? targetPaths : undefined,
        });
      } catch (e: unknown) {
        pushToast(`scan failed to start: ${resolveApiError(e)}`);
        setSt((prev) => ({ ...prev, busy: false, phase: 'idle', showScanForm: true }));
      }
    },
    [pushToast],
  );

  const fetchExecution = useCallback(async () => {
    try {
      const report = await api.execution();
      setSt((prev) => ({ ...prev, report, phase: 'report' }));
    } catch (e: unknown) {
      pushToast(`cannot load execution report: ${resolveApiError(e)}`);
    }
  }, [pushToast]);
  const handleSse = useCallback(
    async (ev: SseEvent) => {
      const d = ev.data as Record<string, unknown>;
      switch (ev.name) {
        case 'session.state': {
          const state = String(d.state ?? '');
          const cur = stRef.current;
          if (state === 'ready' || state === 'done') {
            if (cur.phase === 'scanning' || cur.phase === 'idle') {
              await enterBrowse();
            }
            if (state === 'done' && cur.phase === 'running' && !cur.report) {
              await fetchExecution();
            }
          }
          break;
        }
        case 'scan.progress':
          setSt((prev) => ({
            ...prev,
            progress: d as unknown as ScanProgress,
            phase: prev.phase === 'idle' ? 'scanning' : prev.phase,
          }));
          break;
        case 'scan.complete':
          setSt((prev) => ({ ...prev, progress: null }));
          await enterBrowse();
          break;
        case 'suggestion.batch':
          setSt((prev) => ({ ...prev, suggestions: (d.items as Suggestion[]) ?? [] }));
          break;
        case 'batch.validated': {
          const v = d as { valid: boolean; rejected: { id: string; reason: string }[] };
          if (!v.valid) {
            pushToast(`re-validation rejected ${v.rejected.length} suggestion(s)`);
          }
          break;
        }
        case 'execution.op':
          setSt((prev) => ({
            ...prev,
            runLog: [
              ...prev.runLog,
              opLine(d as { suggestion_id: string; operation: string; status: string; detail?: unknown }),
            ],
          }));
          break;
        case 'execution.complete':
          await fetchExecution();
          break;
        case 'config.updated':
          pushToast('config updated by another client');
          break;
        case 'cache.cleared':
          pushToast('cache cleared by another client');
          break;
        default:
          break;
      }
    },
    [enterBrowse, fetchExecution, pushToast],
  );


  const poll = useCallback(async () => {
    let session: Session;
    try {
      session = await api.session();
    } catch {
      return;
    }
    const cur = stRef.current;
    setSt((prev) => ({ ...prev, session }));
    const state = session.state;
    if (state === 'scanning' && cur.phase !== 'scanning') {
      setSt((prev) => ({ ...prev, phase: 'scanning', showScanForm: false }));
    } else if ((state === 'ready' || state === 'done') && (cur.phase === 'scanning' || cur.phase === 'idle')) {
      await enterBrowse();
    } else if (state === 'done' && cur.phase === 'running' && !cur.report) {
      await fetchExecution();
    } else if (state === 'failed' && cur.phase !== 'idle') {
      const msg = (session as { failure?: { message?: string } }).failure?.message;
      pushToast(`scan failed: ${msg ?? 'unknown reason'}`);
      setSt((prev) => ({ ...prev, phase: 'idle', showScanForm: true, progress: null }));
    }
  }, [enterBrowse, fetchExecution, pushToast]);

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const session = await api.session();
        if (cancelled) return;
        setSt((prev) => ({ ...prev, session }));
        if (session.state === 'ready' || session.state === 'done') {
          const list = await api.suggestions();
          if (cancelled) return;
          setSt((prev) => ({
            ...prev,
            suggestions: list.items,
            phase: 'browse',
            showScanForm: false,
          }));
        }
      } catch {
        // Service may be mid-restart; the poll loop will recover.
      }
    })();
    const sse = new EventSourceClient();
    sse.onEvent((ev) => {
      void handleSse(ev);
    });
    sse.start();
    sseRef.current = sse;
    const timer = setInterval(() => {
      void poll();
    }, POLL_MS);
    return () => {
      cancelled = true;
      clearInterval(timer);
      sse.stop();
    };
  }, [handleSse, poll]);

  const toggle = useCallback(
    async (id: string, select: boolean) => {
      setSt((prev) => ({
        ...prev,
        suggestions: prev.suggestions.map((s) => (s.id === id ? { ...s, selected: select } : s)),
      }));
      try {
        if (select) {
          await api.selectionAdd([id]);
        } else {
          await api.selectionRemove([id]);
        }
      } catch (e: unknown) {
        pushToast(`selection update failed: ${resolveApiError(e)}`);
        await refreshSuggestions();
      }
    },
    [pushToast, refreshSuggestions],
  );

  const selectMany = useCallback(
    async (ids: string[]) => {
      setSt((prev) => ({
        ...prev,
        suggestions: prev.suggestions.map((s) => ({ ...s, selected: ids.includes(s.id) })),
      }));
      try {
        await api.selectionPut(ids);
      } catch (e: unknown) {
        pushToast(`selection update failed: ${resolveApiError(e)}`);
        await refreshSuggestions();
      }
    },
    [pushToast, refreshSuggestions],
  );

  const clearSelection = useCallback(() => {
    void selectMany([]);
  }, [selectMany]);

  const run = useCallback(
    async () => {
      const ids = stRef.current.suggestions.filter((s) => s.selected).map((s) => s.id);
      if (ids.length === 0) {
        pushToast('no suggestions selected');
        return;
      }
      setSt((prev) => ({
        ...prev,
        phase: 'running',
        report: null,
        runLog: [],
        runError: null,
        busy: true,
      }));
      try {
        await api.run(ids);
        setSt((prev) => ({ ...prev, busy: false }));
        pushToast(`running ${ids.length} suggestion(s)`);
      } catch (e: unknown) {
        pushToast(`cannot start run: ${resolveApiError(e)}`);
        setSt((prev) => ({ ...prev, busy: false, phase: 'browse' }));
      }
    },
    [pushToast],
  );

  const cancel = useCallback(async () => {
    try {
      await api.cancel();
      pushToast('cancelled in-flight work');
    } catch (e: unknown) {
      pushToast(`cancel failed: ${resolveApiError(e)}`);
    }
  }, [pushToast]);

  const exportPlan = useCallback(async () => {
    const ids = stRef.current.suggestions.filter((s) => s.selected).map((s) => s.id);
    if (ids.length === 0) {
      pushToast('no suggestions selected');
      return;
    }
    try {
      const res = await api.exportScript(ids);
      downloadText('homelinter-plan.sh', res.text);
      pushToast(`exported ${res.count} operation(s) as a shell plan`);
    } catch (e: unknown) {
      pushToast(`export failed: ${resolveApiError(e)}`);
    }
  }, [pushToast]);

  const doneFromReport = useCallback(async () => {
    try {
      await refreshSuggestions();
      setSt((prev) => ({ ...prev, phase: 'browse', report: null, runLog: [] }));
    } catch (e: unknown) {
      pushToast(`cannot refresh: ${resolveApiError(e)}`);
    }
  }, [pushToast, refreshSuggestions]);

  const selectedCount = st.suggestions.filter((s) => s.selected).length;
  const state = st.session?.state ?? 'unknown';
  const counts = {
    suggestions: st.suggestions.length,
    shown: st.suggestions.length,
    selected: selectedCount,
  };

  let body;
  if (st.configOpen) {
    body = <ConfigView onClose={() => patch({ configOpen: false })} pushToast={pushToast} />;
  } else if (st.showScanForm) {
    body = (
      <ScanForm
        defaultRoot={st.session?.scan_root ?? ''}
        hasSuggestions={st.suggestions.length > 0}
        onStart={startScan}
        onResume={() => patch({ showScanForm: false, phase: 'browse' })}
      />
    );
  } else if (st.phase === 'scanning') {
    body = (
      <main className="run-view">
        <div className="run-status">
          <span className="state-busy">{stateLabel(state)} ...</span>
          {st.progress && (
            <span className="progress-line">
              {st.progress.phase}, scanned={st.progress.scanned}, elapsed={elapsedLabel(st.progress.elapsed_ms)}
            </span>
          )}
          <div className="toolbar">
            <button type="button" onClick={() => void cancel()}>cancel</button>
          </div>
        </div>
      </main>
    );
  } else if (st.phase === 'running' || st.phase === 'report') {
    body = (
      <RunView
        state={state}
        progress={null}
        log={st.runLog}
        report={st.report}
        error={st.runError}
        onCancel={() => void cancel()}
        onDone={() => void doneFromReport()}
      />
    );
  } else {
    body = (
      <Browse
        suggestions={st.suggestions}
        onToggle={(id, sel) => void toggle(id, sel)}
        onSelectMany={(ids) => void selectMany(ids)}
        onClearSelection={clearSelection}
        onRun={() => void run()}
        onExport={() => void exportPlan()}
      />
    );
  }

  return (
    <div className="app">
      <Header
        state={state}
        root={st.session?.scan_root ?? null}
        counts={counts}
        busy={st.busy}
        onNewScan={() => patch({ showScanForm: true, phase: 'idle', configOpen: false })}
        onConfig={() => patch({ configOpen: true })}
      />
      {body}
      <Toasts toasts={st.toasts} onDismiss={dismissToast} />
    </div>
  );
}