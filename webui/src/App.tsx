import { useCallback, useEffect, useRef, useState } from "react";
import { api, resolveApiError, SelectionFilter } from "./api";
import { Browse } from "./components/Browse";
import { ConfigView } from "./components/ConfigView";
import { Header } from "./components/Header";
import { RunView } from "./components/RunView";
import { ScanForm, ScanFormValues } from "./components/ScanForm";
import { ScanStages } from "./components/ScanStages";
import { Toasts } from "./components/Toasts";
import { EventSourceClient } from "./events";
import { downloadText } from "./lib/download";
import {
  Filter,
  filterFromSearchParams,
  filterToSearchParams,
  matchesFilter,
  mergeSuggestionEvents,
} from "./lib/filter";
import {
  actionLabelParts,
  appendScanStage,
  countsOf,
  countsSummary,
  metadataProgressLabel,
  stateLabel,
} from "./lib/format";
import {
  applyExecutionOp,
  applyReport,
  buildRunState,
  type RunState,
} from "./lib/run";
import type {
  ExecutionOp,
  ExecutionReport,
  MetadataProgress,
  ScanProgress,
  ScanStage,
  Session,
  SseEvent,
  Suggestion,
  SuggestionStatus,
} from "./types";

type Phase = "idle" | "scanning" | "browse" | "running" | "report";

interface AppState {
  phase: Phase;
  session: Session | null;
  suggestions: Suggestion[];
  /** Server total for the active filter; the loaded window may be smaller. */
  suggestionsTotal: number;
  /** Complete set of asset kinds for the current batch; the loaded page need
   *  not contain every kind, so this comes from the report summary. */
  kinds: string[];
  filter: Filter;
  stages: ScanStage[];
  metadataProgress: MetadataProgress | null;
  /** Live state of the current run: every requested suggestion with its
   *  action sentence and status, folded from `execution.op` events and the
   *  final report. Null outside a run. */
  run: RunState | null;
  report: ExecutionReport | null;
  runError: string | null;
  busy: boolean;
  toasts: string[];
  configOpen: boolean;
  showScanForm: boolean;
  pendingAction: "run" | "export" | null;
}

const POLL_MS = 2000;
/** Suggestions fetched per `GET /suggestions` request while browsing. */
const PAGE_SIZE = 500;

/** Initial filter, seeded from the page URL so a reload or a shared link
 *  restores the filtered view; every filter change is written back. */
function initialFilter(): Filter {
  return filterFromSearchParams(new URLSearchParams(window.location.search));
}

/** Map the SPA's filter view onto the service's selection-filter body;
 *  `all` becomes an omitted (unconstrained) field. */
function toApiFilter(f: Filter): SelectionFilter {
  return {
    type: f.type === "all" ? undefined : f.type,
    asset_kind: f.kind === "all" ? undefined : f.kind,
    status: f.status === "all" ? undefined : f.status,
    manual: f.manual === "manual" ? true : undefined,
    path: f.path || undefined,
  };
}

export default function App() {
  const [st, setSt] = useState<AppState>({
    phase: "idle",
    session: null,
    suggestions: [],
    suggestionsTotal: 0,
    kinds: [],
    filter: initialFilter(),
    stages: [],
    metadataProgress: null,
    run: null,
    report: null,
    runError: null,
    busy: false,
    toasts: [],
    configOpen: false,
    showScanForm: true,
    pendingAction: null,
  });
  const sseRef = useRef<EventSourceClient | null>(null);
  const stRef = useRef(st);
  stRef.current = st;

  // Provider metadata resolves one suggestion at a time over SSE. Buffer
  // those deltas and apply them once per animation frame so a large batch
  // does not trigger one state update (and one full-list reconcile) per row.
  const pendingUpdatesRef = useRef<Map<string, Suggestion>>(new Map());
  const pendingRemovalsRef = useRef<Set<string>>(new Set());
  const flushScheduledRef = useRef(false);

  const flushSuggestionEvents = useCallback(() => {
    flushScheduledRef.current = false;
    const updates = pendingUpdatesRef.current;
    const removals = pendingRemovalsRef.current;
    if (updates.size === 0 && removals.size === 0) return;
    pendingUpdatesRef.current = new Map();
    pendingRemovalsRef.current = new Set();
    setSt((prev) => {
      const { suggestions, changed } = mergeSuggestionEvents(
        prev.suggestions,
        updates,
        removals,
        prev.filter,
      );
      return changed ? { ...prev, suggestions } : prev;
    });
  }, []);

  const scheduleSuggestionFlush = useCallback(() => {
    if (flushScheduledRef.current) return;
    flushScheduledRef.current = true;
    requestAnimationFrame(flushSuggestionEvents);
  }, [flushSuggestionEvents]);

  const dropPendingSuggestionEvents = useCallback(() => {
    if (pendingUpdatesRef.current.size > 0) pendingUpdatesRef.current = new Map();
    if (pendingRemovalsRef.current.size > 0) pendingRemovalsRef.current = new Set();
  }, []);

  const patch = useCallback((p: Partial<AppState>) => {
    setSt((prev) => ({ ...prev, ...p }));
  }, []);

  const pushToast = useCallback((msg: string) => {
    setSt((prev) => ({ ...prev, toasts: [...prev.toasts, `${msg}`] }));
    setTimeout(() => {
      setSt((prev) => ({
        ...prev,
        toasts: prev.toasts.filter((t) => t !== msg),
      }));
    }, 4500);
  }, []);

  const dismissToast = useCallback((i: number) => {
    setSt((prev) => ({
      ...prev,
      toasts: prev.toasts.filter((_, j) => j !== i),
    }));
  }, []);

  const loadTokenRef = useRef(0);
  const loadingMoreRef = useRef(false);

  const pageParams = useCallback(
    (filter: Filter, offset: number) => ({
      type: filter.type === "all" ? undefined : filter.type,
      kind: filter.kind === "all" ? undefined : filter.kind,
      status: filter.status === "all" ? undefined : filter.status,
      manual: filter.manual === "manual" ? true : undefined,
      path: filter.path || undefined,
      offset,
      limit: PAGE_SIZE,
    }),
    [],
  );

  /** Replace the loaded window with the first page for `filter`, and pick up
   *  the complete set of asset kinds from the report summary (a single page
   *  need not contain every kind). */
  const resetSuggestions = useCallback(
    async (filter: Filter) => {
      const token = ++loadTokenRef.current;
      loadingMoreRef.current = false;
      dropPendingSuggestionEvents();
      const [list, summary] = await Promise.all([
        api.suggestions(pageParams(filter, 0)),
        api.summary().catch(() => null),
      ]);
      if (loadTokenRef.current !== token) return;
      setSt((prev) => ({
        ...prev,
        suggestions: list.items,
        suggestionsTotal: list.total,
        kinds: summary
          ? Object.keys(summary.suggestions_by_kind).sort()
          : prev.kinds,
      }));
    },
    [dropPendingSuggestionEvents, pageParams],
  );

  /** Append the next page. Bounded by the server total and skipped when a
   *  filter change has already advanced the load generation. */
  const loadMore = useCallback(async () => {
    const cur = stRef.current;
    if (loadingMoreRef.current) return;
    if (cur.suggestions.length >= cur.suggestionsTotal) return;
    const token = loadTokenRef.current;
    loadingMoreRef.current = true;
    try {
      const list = await api.suggestions(
        pageParams(cur.filter, cur.suggestions.length),
      );
      if (loadTokenRef.current !== token) return;
      setSt((prev) => {
        const seen = new Set(prev.suggestions.map((s) => s.id));
        const added = list.items.filter((s) => !seen.has(s.id));
        return {
          ...prev,
          suggestions: [...prev.suggestions, ...added],
          suggestionsTotal: list.total,
        };
      });
    } catch (e: unknown) {
      pushToast(`cannot load more suggestions: ${resolveApiError(e)}`);
    } finally {
      loadingMoreRef.current = false;
    }
  }, [pageParams, pushToast]);

  // Keep the URL in sync with the filter so a reload or a shared link
  // restores the filtered view. There is no router, so the write-through
  // uses history.replaceState and never pushes history entries.
  useEffect(() => {
    const q = filterToSearchParams(st.filter);
    const qs = q.toString();
    const url = qs
      ? `${window.location.pathname}?${qs}`
      : window.location.pathname;
    window.history.replaceState(null, "", url);
  }, [st.filter]);

  const applyFilter = useCallback(
    (filter: Filter) => {
      setSt((prev) => ({
        ...prev,
        filter,
        suggestions: [],
        suggestionsTotal: 0,
      }));
      void resetSuggestions(filter).catch((e: unknown) => {
        pushToast(`cannot filter suggestions: ${resolveApiError(e)}`);
      });
    },
    [pushToast, resetSuggestions],
  );

  /** Refresh the session-count fields after a selection mutation; the 2s
   *  poll is the backstop. */
  const refreshSession = useCallback(async () => {
    try {
      const session = await api.session();
      setSt((prev) => ({ ...prev, session }));
    } catch {
      // The poll will catch up.
    }
  }, []);

  const enterBrowse = useCallback(async () => {
    try {
      await resetSuggestions(stRef.current.filter);
      setSt((prev) => ({ ...prev, phase: "browse", showScanForm: false }));
    } catch (e: unknown) {
      pushToast(`cannot load suggestions: ${resolveApiError(e)}`);
      setSt((prev) => ({ ...prev, phase: "idle" }));
    }
  }, [pushToast, resetSuggestions]);

  const startScan = useCallback(
    async (_values: ScanFormValues) => {
      const curState = stRef.current.session?.state;
      if (
        curState === "scanning" ||
        curState === "analyzing" ||
        curState === "executing"
      ) {
        pushToast("a scan or execution is already in progress");
        return;
      }
      setSt((prev) => ({
        ...prev,
        busy: true,
        phase: "scanning",
        showScanForm: false,
        suggestions: [],
        stages: [],
        metadataProgress: null,
        report: null,
        run: null,
        runError: null,
      }));
      try {
        // Root, hidden, and destination paths are persisted by the scan
        // form (PUT /config) before it calls onStart; the scan request
        // itself carries no parameters.
        await api.startScan();
      } catch (e: unknown) {
        pushToast(`scan failed to start: ${resolveApiError(e)}`);
        setSt((prev) => ({
          ...prev,
          busy: false,
          phase: "idle",
          showScanForm: true,
        }));
      }
    },
    [pushToast],
  );

  const fetchExecution = useCallback(async () => {
    try {
      const report = await api.execution();
      setSt((prev) => ({
        ...prev,
        report,
        phase: "report",
        run: prev.run ? applyReport(prev.run, report.entries) : prev.run,
      }));
    } catch (e: unknown) {
      pushToast(`cannot load execution report: ${resolveApiError(e)}`);
    }
  }, [pushToast]);

  /** A run finished while this client was not watching it (the user navigated
   *  back mid-run, or another client started the run): fetch the report for
   *  the record and refresh the suggestion rows, but do not yank the view to
   *  the run screen. */
  const runFinishedElsewhere = useCallback(async () => {
    let summary = "";
    try {
      const report = await api.execution();
      summary = countsSummary(countsOf(report.entries));
      setSt((prev) => ({ ...prev, report }));
    } catch {
      // The refreshed rows still show the outcome without the report.
    }
    try {
      await resetSuggestions(stRef.current.filter);
    } catch (e: unknown) {
      pushToast(`cannot refresh: ${resolveApiError(e)}`);
    }
    pushToast(summary ? `execution finished: ${summary}` : "execution finished");
  }, [pushToast, resetSuggestions]);

  /** Label run rows whose suggestions were outside the loaded browse window,
   *  fetched by id in modest chunks so the labels land while the run is still
   *  executing its first suggestions. */
  const fillRunLabels = useCallback(
    async (
      ids: string[],
      known: Map<string, Suggestion>,
      root: string | null,
    ) => {
      const missing = ids.filter((id) => !known.has(id));
      const CHUNK = 32;
      for (let i = 0; i < missing.length; i += CHUNK) {
        const settled = await Promise.all(
          missing.slice(i, i + CHUNK).map(async (id) => {
            try {
              return [id, await api.suggestion(id)] as const;
            } catch {
              return null;
            }
          }),
        );
        const fetched = new Map<string, Suggestion>();
        for (const r of settled) {
          if (r) fetched.set(r[0], r[1]);
        }
        if (fetched.size === 0) continue;
        setSt((prev) => ({
          ...prev,
          run: prev.run
            ? {
                ...prev.run,
                items: prev.run.items.map((it) => {
                  const s = fetched.get(it.id);
                  return s ? { ...it, parts: actionLabelParts(s, root) } : it;
                }),
              }
            : prev.run,
        }));
      }
    },
    [],
  );
  const handleSse = useCallback(
    async (ev: SseEvent) => {
      const d = ev.data as Record<string, unknown>;
      switch (ev.name) {
        case "session.state": {
          const state = String(d.state ?? "");
          const cur = stRef.current;
          if (state === "ready" || state === "done") {
            // A deliberately open scan setup page (showScanForm) is not
            // "idle" in the recovery sense; never yank the user off it.
            if (
              !cur.showScanForm &&
              (cur.phase === "scanning" ||
                cur.phase === "idle" ||
                cur.phase === "browse")
            ) {
              await enterBrowse();
            }
            if (state === "done" && cur.phase === "running" && !cur.report) {
              await fetchExecution();
            }
          }
          break;
        }
        case "scan.progress":
          // A scan in flight is never hidden behind the scan setup form: a
          // window opened or reloaded mid-scan (or a scan started by another
          // client) moves to the progress view, which carries the live stage
          // list and the cancel button.
          setSt((prev) => ({
            ...prev,
            stages: appendScanStage(prev.stages, d as unknown as ScanProgress),
            phase: prev.phase === "idle" ? "scanning" : prev.phase,
            showScanForm: false,
          }));
          break;
        case "scan.complete":
          setSt((prev) => ({ ...prev, stages: [] }));
          // If the user is on the scan setup page, the completed scan was
          // started elsewhere; leave them where they are. The new batch is
          // already applied via suggestion.batch and waiting on resume.
          if (!stRef.current.showScanForm) {
            await enterBrowse();
          }
          break;
        case "suggestion.batch": {
          // The event carries only the batch total; rows are fetched a page
          // at a time via `GET /suggestions` (see DESIGN.md "Events (SSE)").
          dropPendingSuggestionEvents();
          setSt((prev) => ({
            ...prev,
            suggestionsTotal: Number(d.total ?? 0),
          }));
          break;
        }
        case "suggestion.updated": {
          const item = d.item as Suggestion | undefined;
          if (item) {
            pendingUpdatesRef.current.set(item.id, item);
            scheduleSuggestionFlush();
          }
          break;
        }
        case "suggestion.removed": {
          const id = String(d.id ?? "");
          if (id) {
            pendingRemovalsRef.current.add(id);
            pendingUpdatesRef.current.delete(id);
            scheduleSuggestionFlush();
          }
          break;
        }
        case "metadata.progress":
          setSt((prev) => ({
            ...prev,
            metadataProgress: {
              completed: Number(d.completed ?? 0),
              total: Number(d.total ?? 0),
            },
          }));
          break;
        case "batch.validated": {
          const v = d as {
            valid: boolean;
            rejected: { id: string; reason: string }[];
          };
          if (!v.valid) {
            pushToast(
              `re-validation rejected ${v.rejected.length} suggestion(s)`,
            );
          }
          if (v.rejected.length > 0) {
            const reasons = new Map(v.rejected.map((r) => [r.id, r.reason]));
            setSt((prev) => {
              const next = prev.suggestions
                .map((s) =>
                  reasons.has(s.id)
                    ? {
                        ...s,
                        status: "rejected" as SuggestionStatus,
                        rejection_reason: reasons.get(s.id) ?? null,
                      }
                    : s,
                )
                .filter((s) => matchesFilter(s, prev.filter));
              return { ...prev, suggestions: next };
            });
          }
          break;
        }
        case "execution.op":
          setSt((prev) => ({
            ...prev,
            run: prev.run
              ? applyExecutionOp(prev.run, d as unknown as ExecutionOp)
              : prev.run,
          }));
          break;
        case "execution.complete": {
          const curPhase = stRef.current.phase;
          if (curPhase === "running" || curPhase === "report") {
            await fetchExecution();
          } else {
            await runFinishedElsewhere();
          }
          break;
        }
        case "config.updated": {
          // No toast: the scan setup page persists settings as they change,
          // so this event fires on every saved edit. The configuration view
          // mirrors via the config hash; batch staleness shows the banner.
          try {
            const session = await api.session();
            setSt((prev) => ({ ...prev, session }));
          } catch {
            // The 2s poll will catch up.
          }
          break;
        }
        case "cache.cleared":
          pushToast("cache cleared by another client");
          break;
        default:
          break;
      }
    },
    [dropPendingSuggestionEvents, enterBrowse, fetchExecution, pushToast, runFinishedElsewhere, scheduleSuggestionFlush],
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
    if (session.metadata_progress) {
      setSt((prev) => ({ ...prev, metadataProgress: session.metadata_progress ?? null }));
    }
    const state = session.state;
    if (state === "scanning" && (cur.phase !== "scanning" || cur.showScanForm)) {
      setSt((prev) => ({ ...prev, phase: "scanning", showScanForm: false }));
    } else if (
      (state === "ready" || state === "done") &&
      !cur.showScanForm &&
      (cur.phase === "scanning" ||
        cur.phase === "idle" ||
        (cur.phase === "browse" && cur.suggestions.length === 0))
    ) {
      await enterBrowse();
    } else if (state === "done" && cur.phase === "running" && !cur.report) {
      await fetchExecution();
    } else if (state === "failed" && cur.phase !== "idle") {
      const msg = (session as { failure?: { message?: string } }).failure
        ?.message;
      pushToast(`scan failed: ${msg ?? "unknown reason"}`);
      setSt((prev) => ({
        ...prev,
        phase: "idle",
        showScanForm: true,
        stages: [],
        metadataProgress: null,
      }));
    }
  }, [enterBrowse, fetchExecution, pushToast]);

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const session = await api.session();
        if (cancelled) return;
        setSt((prev) => ({ ...prev, session }));
        if (session.state === "ready" || session.state === "done") {
          // Enter browse with the URL-restored filter so a reload or shared
          // link lands on the same filtered view.
          await resetSuggestions(stRef.current.filter);
          if (cancelled) return;
          setSt((prev) => ({
            ...prev,
            phase: "browse",
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
  }, [handleSse, poll, resetSuggestions]);

  const toggle = useCallback(
    async (id: string, select: boolean) => {
      setSt((prev) => ({
        ...prev,
        suggestions: prev.suggestions.map((s) =>
          s.id === id ? { ...s, selected: select } : s,
        ),
      }));
      try {
        if (select) {
          await api.selectionAdd([id]);
        } else {
          await api.selectionRemove([id]);
        }
      } catch (e: unknown) {
        pushToast(`selection update failed: ${resolveApiError(e)}`);
        await resetSuggestions(stRef.current.filter);
      }
      void refreshSession();
    },
    [pushToast, refreshSession, resetSuggestions],
  );

  /** Select every suggestion matching the active filter (the whole filtered
   *  set, not just the loaded window), via the service's filter-based add;
   *  the optimistic update mirrors it across the loaded rows. */
  const selectFiltered = useCallback(
    (filter: Filter) => {
      setSt((prev) => ({
        ...prev,
        suggestions: prev.suggestions.map((s) =>
          !s.selected && matchesFilter(s, filter)
            ? { ...s, selected: true }
            : s,
        ),
      }));
      void (async () => {
        try {
          await api.selectionAddFilter(toApiFilter(filter));
        } catch (e: unknown) {
          pushToast(`selection update failed: ${resolveApiError(e)}`);
          await resetSuggestions(stRef.current.filter);
        }
        void refreshSession();
      })();
    },
    [pushToast, refreshSession, resetSuggestions],
  );

  /** Deselect every suggestion matching the active filter (again the whole
   *  filtered set), leaving selections outside the filtered set untouched;
   *  with no dimension constrained this clears the whole selection. */
  const clearFiltered = useCallback(
    (filter: Filter) => {
      setSt((prev) => ({
        ...prev,
        suggestions: prev.suggestions.map((s) =>
          s.selected && matchesFilter(s, filter)
            ? { ...s, selected: false }
            : s,
        ),
      }));
      void (async () => {
        try {
          await api.selectionRemoveFilter(toApiFilter(filter));
        } catch (e: unknown) {
          pushToast(`selection update failed: ${resolveApiError(e)}`);
          await resetSuggestions(stRef.current.filter);
        }
        void refreshSession();
      })();
    },
    [pushToast, refreshSession, resetSuggestions],
  );

  const startRun = useCallback(
    async (ids: string[]) => {
      if (ids.length === 0) {
        pushToast("no suggestions selected");
        return;
      }
      const root = stRef.current.session?.scan_root ?? null;
      const known = new Map(
        stRef.current.suggestions.map((s) => [s.id, s] as const),
      );
      const run = buildRunState(ids, known, root);
      setSt((prev) => ({
        ...prev,
        phase: "running",
        run,
        report: null,
        runError: null,
        busy: true,
      }));
      void fillRunLabels(ids, known, root);
      try {
        await api.run(ids);
        setSt((prev) => ({ ...prev, busy: false }));
        pushToast(`running ${ids.length} suggestion(s)`);
      } catch (e: unknown) {
        pushToast(`cannot start run: ${resolveApiError(e)}`);
        setSt((prev) => ({ ...prev, busy: false, phase: "browse", run: null }));
      }
    },
    [fillRunLabels, pushToast],
  );

  // The batch carries the hash of the config it was scanned under; when the
  // live config hash moved past it, the destinations are stale. Ask before
  // executing or exporting, and surface a banner while browsing.
  const batchIsStale = useCallback((sess: Session | null, count: number) => {
    return (
      !!sess &&
      !!sess.batch_config_hash &&
      !!sess.config_hash &&
      sess.batch_config_hash !== sess.config_hash &&
      count > 0
    );
  }, []);
  const configDirty = batchIsStale(st.session, st.suggestionsTotal);

  const readSelection = useCallback(async (): Promise<string[] | null> => {
    try {
      return (await api.selection()).ids;
    } catch (e: unknown) {
      pushToast(`cannot read selection: ${resolveApiError(e)}`);
      return null;
    }
  }, [pushToast]);

  const batchSize = useCallback(
    () =>
      stRef.current.session?.suggestions_total ??
      stRef.current.suggestionsTotal,
    [],
  );

  const run = useCallback(async () => {
    const ids = await readSelection();
    if (ids === null) return;
    if (ids.length === 0) {
      pushToast("no suggestions selected");
      return;
    }
    if (batchIsStale(stRef.current.session, batchSize())) {
      setSt((prev) => ({ ...prev, pendingAction: "run" }));
      return;
    }
    await startRun(ids);
  }, [batchIsStale, batchSize, pushToast, readSelection, startRun]);

  const cancel = useCallback(async () => {
    try {
      await api.cancel();
      pushToast("cancelled in-flight work");
    } catch (e: unknown) {
      pushToast(`cancel failed: ${resolveApiError(e)}`);
    }
  }, [pushToast]);

  const doExport = useCallback(
    async (ids: string[]) => {
      if (ids.length === 0) {
        pushToast("no suggestions selected");
        return;
      }
      try {
        const res = await api.exportScript(ids);
        downloadText("homelinter-plan.sh", res.text);
        pushToast(`exported ${res.count} operation(s) as a shell plan`);
      } catch (e: unknown) {
        pushToast(`export failed: ${resolveApiError(e)}`);
      }
    },
    [pushToast],
  );

  const exportPlan = useCallback(async () => {
    const ids = await readSelection();
    if (ids === null) return;
    if (ids.length === 0) {
      pushToast("no suggestions selected");
      return;
    }
    if (batchIsStale(stRef.current.session, batchSize())) {
      setSt((prev) => ({ ...prev, pendingAction: "export" }));
      return;
    }
    await doExport(ids);
  }, [batchIsStale, batchSize, doExport, pushToast, readSelection]);

  // Per-suggestion metadata fetch: invalidate this suggestion's cached
  // provider results and ask again, without re-running the scan. The
  // service returns it marked pending and streams the resolved update.
  const refreshMetadata = useCallback(
    async (id: string, clearOverrides = false) => {
      try {
        const updated = await api.refreshMetadata(id, clearOverrides);
        setSt((prev) => ({
          ...prev,
          suggestions: prev.suggestions.map((s) =>
            s.id === id ? updated : s,
          ),
        }));
        void refreshSession();
      } catch (e: unknown) {
        pushToast(`cannot fetch metadata: ${resolveApiError(e)}`);
      }
    },
    [pushToast, refreshSession],
  );

  // Save a manual metadata edit. The service persists it keyed by content
  // identity, so identical content picks it up on later scans; the returned
  // suggestion replaces the row, or is dropped when the edit makes it stay
  // put.
  const setMetadata = useCallback(
    async (
      id: string,
      edit: { location: string; subject: string; author: string },
    ) => {
      const value = (v: string) => (v.trim() === '' ? null : v);
      try {
        const updated = await api.setSuggestionMetadata(id, {
          location: value(edit.location),
          subject: value(edit.subject),
          author: value(edit.author),
        });
        setSt((prev) => ({
          ...prev,
          suggestions: updated
            ? prev.suggestions.map((s) => (s.id === id ? updated : s))
            : prev.suggestions.filter((s) => s.id !== id),
        }));
        pushToast(updated ? 'metadata saved' : 'edit removed the suggestion');
        void refreshSession();
      } catch (e: unknown) {
        pushToast(`cannot save metadata: ${resolveApiError(e)}`);
      }
    },
    [pushToast, refreshSession],
  );

  // Re-apply the effective metadata for this suggestion and rebuild its
  // destination, without a scan or a provider call.
  const rebuildSuggestion = useCallback(
    async (id: string) => {
      try {
        const updated = await api.rebuildSuggestion(id);
        setSt((prev) => ({
          ...prev,
          suggestions: updated
            ? prev.suggestions.map((s) => (s.id === id ? updated : s))
            : prev.suggestions.filter((s) => s.id !== id),
        }));
        pushToast(updated ? 'suggestion rebuilt' : 'rebuild removed the suggestion');
        void refreshSession();
      } catch (e: unknown) {
        pushToast(`cannot rebuild: ${resolveApiError(e)}`);
      }
    },
    [pushToast, refreshSession],
  );

  const dismissPending = useCallback(() => {
    setSt((prev) => ({ ...prev, pendingAction: null }));
  }, []);

  const reScanFromModal = useCallback(() => {
    patch({
      pendingAction: null,
      showScanForm: true,
      phase: "idle",
      configOpen: false,
    });
  }, [patch]);

  // Execute/export anyway, accepting the stale destinations.
  const proceedPending = useCallback(async () => {
    const action = stRef.current.pendingAction;
    if (!action) return;
    setSt((prev) => ({ ...prev, pendingAction: null }));
    const ids = await readSelection();
    if (ids === null) return;
    if (action === "run") {
      await startRun(ids);
    } else {
      await doExport(ids);
    }
  }, [doExport, readSelection, startRun]);

  /** Leave the run screen for the suggestion browser: the normal exit once
   *  the report is in, and also the way back while a run is still executing
   *  (the run continues in the background; its completion then refreshes the
   *  rows instead of yanking the view back). */
  const leaveRun = useCallback(async () => {
    const executing = stRef.current.session?.state === "executing";
    try {
      await resetSuggestions(stRef.current.filter);
    } catch (e: unknown) {
      pushToast(`cannot refresh: ${resolveApiError(e)}`);
    }
    setSt((prev) => ({ ...prev, phase: "browse", report: null, run: null }));
    if (executing) {
      pushToast("execution continues in the background");
    }
  }, [pushToast, resetSuggestions]);

  const state = st.session?.state ?? "unknown";
  const warnings = st.session?.warnings ?? [];
  // Mirror the backend's state guards so controls are disabled when the
  // service would reject the request (see "Session Model" in DESIGN.md):
  // a new scan is only accepted outside scanning/analyzing/executing, and
  // execute/export only from a ready or done session. Cancel is only
  // meaningful while something is in progress.
  const inProgress = (s: string) =>
    s === "scanning" || s === "analyzing" || s === "executing";
  const canScan = !inProgress(state);
  const canAct = state === "ready" || state === "done";
  const canCancel = inProgress(state);
  // Metadata progress and the provider-cache lookups behind it share one
  // label while a scan is being planned and analyzed.
  const metadataLabel = metadataProgressLabel(
    st.metadataProgress?.completed ?? 0,
    st.metadataProgress?.total ?? 0,
    st.session?.cache_hits ?? 0,
    st.session?.cache_misses ?? 0,
  );

  let body;
  if (st.configOpen) {
    body = (
      <ConfigView
        configHash={st.session?.config_hash ?? null}
        onClose={() => patch({ configOpen: false })}
        pushToast={pushToast}
      />
    );
  } else if (st.showScanForm) {
    body = (
      <ScanForm
        key={st.session ? "session-loaded" : "booting"}
        defaultRoot={st.session?.scan_root ?? ""}
        defaultHome={st.session?.home ?? ""}
        configHash={st.session?.config_hash ?? null}
        hasSuggestions={st.suggestionsTotal > 0}
        startDisabled={!canScan}
        onStart={startScan}
        onResume={() => void enterBrowse()}
      />
    );
  } else if (st.phase === "scanning") {
    body = (
      <main className="run-view">
        <div className="run-status">
          <span className="state-busy">{stateLabel(state)} ...</span>
          <div className="toolbar">
            <button
              type="button"
              onClick={() => void cancel()}
              disabled={!canCancel}
              title={
                canCancel ? undefined : 'nothing is in progress to cancel'
              }
            >
              cancel
            </button>
          </div>
        </div>
        <ScanStages stages={st.stages} />
        {metadataLabel && (
          <span className="progress-line">{metadataLabel}</span>
        )}
      </main>
    );
  } else if (st.phase === "running" || st.phase === "report") {
    body = (
      <RunView
        state={state}
        run={st.run}
        report={st.report}
        error={st.runError}
        cancelDisabled={!canCancel}
        onCancel={() => void cancel()}
        onDone={() => void leaveRun()}
      />
    );
  } else {
    body = (
      <>
        {configDirty && (
          <p className="banner warn" role="status">
            Configuration changed after this scan: the suggestions still use the
            scan's settings. Start a new scan to apply the changes.
          </p>
        )}
        <Browse
          suggestions={st.suggestions}
          kinds={st.kinds}
          filter={st.filter}
          totalSuggestions={st.session?.suggestions_total ?? st.suggestionsTotal}
          filteredSuggestions={st.suggestionsTotal}
          selectedCount={st.session?.selection_count ?? 0}
          manualCount={st.session?.manual_overrides_total ?? 0}
          metadataProgress={st.metadataProgress}
          cacheHits={st.session?.cache_hits ?? 0}
          cacheMisses={st.session?.cache_misses ?? 0}
          actionsDisabled={!canAct}
          root={st.session?.scan_root ?? null}
          onFilterChange={applyFilter}
          onLoadMore={() => void loadMore()}
          onToggle={toggle}
          onSelectFiltered={selectFiltered}
          onClearFiltered={clearFiltered}
          onRun={() => void run()}
          onExport={() => void exportPlan()}
          onRefreshMetadata={(id, clearOverrides) =>
            void refreshMetadata(id, clearOverrides)
          }
          onSetMetadata={(id, edit) => void setMetadata(id, edit)}
          onRebuild={(id) => void rebuildSuggestion(id)}
        />
      </>
    );
  }

  return (
    <div className="app">
      <Header
        state={state}
        root={st.session?.scan_root ?? null}
        busy={st.busy}
        scanDisabled={!canScan}
        onNewScan={() =>
          patch({ showScanForm: true, phase: "idle", configOpen: false })
        }
        onConfig={() => patch({ configOpen: true })}
      />
      {warnings.length > 0 && (
        <div className="banner notice" role="status">
          <p>Scan warnings:</p>
          <ul>
            {warnings.map((w, i) => (
              <li key={`${i}-${w}`}>{w}</li>
            ))}
          </ul>
        </div>
      )}
      {body}
      {st.pendingAction && (
        <div
          className="modal-backdrop"
          role="presentation"
          onClick={dismissPending}
        >
          <div
            className="modal"
            role="dialog"
            aria-modal="true"
            onClick={(e) => e.stopPropagation()}
          >
            <h2>Configuration changed since this scan</h2>
            <p>
              Destinations and provider settings still reflect the scan's
              configuration; saved changes apply to the next scan.
            </p>
            <div className="modal-actions">
              <button
                type="button"
                autoFocus
                onClick={() => void reScanFromModal()}
              >
                re-scan
              </button>
              <button type="button" onClick={() => void proceedPending()}>
                {st.pendingAction} anyway
              </button>
              <button type="button" onClick={dismissPending}>
                cancel
              </button>
            </div>
          </div>
        </div>
      )}
      <Toasts toasts={st.toasts} onDismiss={dismissToast} />
    </div>
  );
}
