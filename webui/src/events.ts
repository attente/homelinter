// Thin wrapper around the browser EventSource for the /events stream.
// The browser reconnects automatically per the EventSource spec; this
// wrapper only normalizes named events and surfaces failures so the app
// can fall back to session polling when the stream is unavailable.

import type { SseEvent, SseEventName } from './types';

const EVENTS_URL = '/api/v1/events';

export class EventSourceClient {
  private es: EventSource | null = null;
  private listeners: Set<(ev: SseEvent) => void> = new Set();
  private failed = false;

  start(): void {
    if (this.es) return;
    const es = new EventSource(EVENTS_URL);
    this.failed = false;
    es.onopen = () => {
      this.failed = false;
    };
    es.onerror = () => {
      // The browser attempts reconnects automatically; we only note
      // the failure so the session poller can keep the UI honest.

      this.failed = true;
    };
    for (const name of NAMED_EVENTS) {
      es.addEventListener(name, (ev: MessageEvent) => {
        this.dispatchName(name, ev.data);
      });
    }
    // The service never emits unnamed messages; default handler ices.
    es.onmessage = (ev: MessageEvent) => {
      this.dispatchName('message', ev.data);
    };
    this.es = es;
  }

  stop(): void {
    if (this.es) {
      this.es.close();
      this.es = null;
    }
  }

  onEvent(fn: (ev: SseEvent) => void): () => void {
    this.listeners.add(fn);
    return () => {
      this.listeners.delete(fn);
    };
  }

  hasFailed(): boolean {
    return this.failed;
  }

  private dispatchName(name: string, raw: string): void {
    let data: unknown = null;
    try {
      data = JSON.parse(raw);
    } catch {
      data = raw;
    }
    const ev: SseEvent = { name, data: data as SseEvent['data'] };
    for (const fn of this.listeners) {
      fn(ev);
    }
  }
}

const NAMED_EVENTS: SseEventName[] = [
  'session.state',
  'scan.progress',
  'scan.complete',
  'suggestion.batch',
  'batch.validated',
  'execution.op',
  'execution.complete',
  'config.updated',
  'cache.cleared',
];