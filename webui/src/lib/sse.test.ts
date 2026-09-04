import { describe, expect, it } from 'vitest';
import { isReconnectable, parseEventData } from './sse';

describe('sse', () => {
  it('parses JSON payloads and keeps raw text on parse failure', () => {
    expect(parseEventData('{"state":"ready"}')).toEqual({ state: 'ready' });
    expect(parseEventData('not json')).toBe('not json');
  });

  it('flags snapshot events that may repeat on reconnect', () => {
    expect(isReconnectable('session.state')).toBe(true);
    expect(isReconnectable('suggestion.batch')).toBe(true);
    expect(isReconnectable('execution.op')).toBe(false);
    expect(isReconnectable('scan.progress')).toBe(false);
  });
});