import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { saveTextFile } from './download';

interface FakeAnchor {
  href: string;
  download: string;
  click: () => void;
  remove: () => void;
}

interface PickedOptions {
  suggestedName?: string;
  types?: { description: string; accept: Record<string, string[]> }[];
}

type Globals = Record<string, unknown>;

let anchors: FakeAnchor[] = [];
let win: Record<string, unknown> = {};

beforeEach(() => {
  anchors = [];
  win = {};
  const globals = globalThis as unknown as Globals;
  globals.window = win;
  globals.document = {
    body: { appendChild: () => {} },
    createElement: () => {
      const anchor: FakeAnchor = {
        href: '',
        download: '',
        click: vi.fn(),
        remove: vi.fn(),
      };
      anchors.push(anchor);
      return anchor;
    },
  };
});

afterEach(() => {
  const globals = globalThis as unknown as Record<string, unknown>;
  delete globals.window;
  delete globals.document;
  vi.restoreAllMocks();
});

describe('saveTextFile', () => {
  it('downloads through an anchor when the save picker is unavailable', async () => {
    const createObjectURL = vi
      .spyOn(URL, 'createObjectURL')
      .mockReturnValue('blob:test');
    const revokeObjectURL = vi
      .spyOn(URL, 'revokeObjectURL')
      .mockImplementation(() => {});

    const saved = await saveTextFile('report.txt', 'hello', 'text/plain');

    expect(saved).toBe(true);
    expect(createObjectURL).toHaveBeenCalledTimes(1);
    expect(revokeObjectURL).toHaveBeenCalledWith('blob:test');
    expect(anchors).toHaveLength(1);
    expect(anchors[0].download).toBe('report.txt');
    expect(anchors[0].click).toHaveBeenCalledTimes(1);
    expect(anchors[0].href).toBe('blob:test');
  });

  it('writes through the save picker and passes the suggested name and type', async () => {
    let picked: PickedOptions | undefined;
    let written: Blob | undefined;
    const closed = vi.fn(async () => {});
    win.showSaveFilePicker = function (
      this: unknown,
      options: PickedOptions,
    ) {
      // The picker is a Window operation; an unbound call fails in Chromium.
      expect(this).toBe(win);
      picked = options;
      return Promise.resolve({
        createWritable: async () => ({
          write: async (chunk: Blob) => {
            written = chunk;
          },
          close: closed,
        }),
      });
    };

    const saved = await saveTextFile(
      'report.txt',
      'hello',
      'text/plain',
      'execution report',
    );

    expect(saved).toBe(true);
    expect(picked?.suggestedName).toBe('report.txt');
    expect(picked?.types).toEqual([
      { description: 'execution report', accept: { 'text/plain': ['.txt'] } },
    ]);
    expect(await written?.text()).toBe('hello');
    expect(closed).toHaveBeenCalledTimes(1);
    expect(anchors).toHaveLength(0);
  });

  it('resolves false without a notice when the user cancels the picker', async () => {
    win.showSaveFilePicker = () =>
      Promise.reject(Object.assign(new Error('cancelled'), { name: 'AbortError' }));

    await expect(saveTextFile('report.txt', 'hello')).resolves.toBe(false);
    expect(anchors).toHaveLength(0);
  });

  it('rejects when the picker fails for another reason', async () => {
    win.showSaveFilePicker = () =>
      Promise.reject(new Error('picker unavailable'));

    await expect(saveTextFile('report.txt', 'hello')).rejects.toThrow(
      'picker unavailable',
    );
  });
});
