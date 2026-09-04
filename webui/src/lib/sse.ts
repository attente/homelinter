// Pure SSE payload helpers, testable without a DOM.

export function parseEventData(raw: string): unknown {
  try {
    return JSON.parse(raw);
  } catch {
    return raw;
  }
}

export function isReconnectable(name: string): boolean {
  // The browser reconnects EventSource automatically; the initial snapshot
  // on a fresh stream replays session state and the suggestion batch.



  return name === 'session.state' || name === 'suggestion.batch';
}