import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { hasNativeDirectoryPicker, pickDirectory } from './native';

let win: Record<string, unknown> = {};

beforeEach(() => {
  win = {};
  (globalThis as unknown as Record<string, unknown>).window = win;
});

afterEach(() => {
  delete (globalThis as unknown as Record<string, unknown>).window;
  vi.restoreAllMocks();
});

describe('hasNativeDirectoryPicker', () => {
  it('is false in a plain browser', () => {
    expect(hasNativeDirectoryPicker()).toBe(false);
  });

  it('is true when the shell injects its invoke function', () => {
    win.__TAURI_INTERNALS__ = { invoke: vi.fn() };
    expect(hasNativeDirectoryPicker()).toBe(true);
  });
});

describe('pickDirectory', () => {
  it('asks the dialog plugin for one directory and returns its path', async () => {
    const invoke = vi.fn().mockResolvedValue('/Users/me/Photos');
    win.__TAURI_INTERNALS__ = { invoke };

    await expect(pickDirectory('choose scan folder')).resolves.toBe(
      '/Users/me/Photos',
    );
    expect(invoke).toHaveBeenCalledWith('plugin:dialog|open', {
      options: {
        directory: true,
        multiple: false,
        title: 'choose scan folder',
      },
    });
  });

  it('accepts a single path in an array and null for cancellation', async () => {
    const invoke = vi
      .fn()
      .mockResolvedValueOnce(['/Users/me/Music'])
      .mockResolvedValueOnce(null);
    win.__TAURI_INTERNALS__ = { invoke };

    await expect(pickDirectory('pick')).resolves.toBe('/Users/me/Music');
    await expect(pickDirectory('pick')).resolves.toBeNull();
  });

  it('throws when the page has no native picker', async () => {
    await expect(pickDirectory('pick')).rejects.toThrow(
      'native directory picker unavailable',
    );
  });

  it('propagates a failed shell call', async () => {
    win.__TAURI_INTERNALS__ = {
      invoke: vi.fn().mockRejectedValue(new Error('dialog failed')),
    };
    await expect(pickDirectory('pick')).rejects.toThrow('dialog failed');
  });
});
