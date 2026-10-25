import { describe, expect, it } from 'vitest';
import { previewSourcePath, resolveEnlargedView } from './preview';

const move = {
  type: 'move' as const,
  target: '/photos/IMG_1.jpg',
  new_path: '/photos/2024/03/IMG_1.jpg',
};

const del = {
  type: 'delete' as const,
  target: '/photos/copy.jpg',
  metadata: { duplicate_of: '/photos/original.jpg' },
};

describe('previewSourcePath', () => {
  it('previews the target of a suggestion that has not run', () => {
    expect(previewSourcePath({ ...move, status: 'pending' })).toBe(move.target);
    expect(previewSourcePath({ ...del, status: 'pending' })).toBe(del.target);
  });

  it('previews where the file ended up for executed suggestions', () => {
    expect(previewSourcePath({ ...move, status: 'executed' })).toBe(
      move.new_path,
    );
    expect(previewSourcePath({ ...del, status: 'executed' })).toBe(
      '/photos/original.jpg',
    );
  });

  it('treats a partial cross-filesystem move like an executed one', () => {
    expect(previewSourcePath({ ...move, status: 'partial' })).toBe(
      move.new_path,
    );
  });

  it('keeps the target when a move never left it', () => {
    for (const status of ['failed', 'skipped', 'rejected'] as const) {
      expect(previewSourcePath({ ...move, status })).toBe(move.target);
      expect(previewSourcePath({ ...del, status })).toBe(del.target);
    }
  });

  it('falls back to the target when the new path or keeper is unknown', () => {
    expect(previewSourcePath({ ...move, status: 'executed', new_path: null })).toBe(
      move.target,
    );
    expect(previewSourcePath({ ...move, status: 'executed', new_path: '' })).toBe(
      move.target,
    );
    expect(previewSourcePath({ ...del, status: 'executed', metadata: {} })).toBe(
      del.target,
    );
    expect(
      previewSourcePath({ ...del, status: 'executed', metadata: null }),
    ).toBe(del.target);
  });
});

describe('resolveEnlargedView', () => {
  it('keeps the requested view while it exists', () => {
    expect(resolveEnlargedView('preview', true, true)).toBe('preview');
    expect(resolveEnlargedView('location', true, true)).toBe('location');
  });

  it('offers each view on its own', () => {
    expect(resolveEnlargedView('preview', true, false)).toBe('preview');
    expect(resolveEnlargedView('location', false, true)).toBe('location');
  });

  it('falls back to the other view when the requested one vanished', () => {
    expect(resolveEnlargedView('preview', false, true)).toBe('location');
    expect(resolveEnlargedView('location', true, false)).toBe('preview');
  });

  it('reports no view when neither is available', () => {
    expect(resolveEnlargedView('preview', false, false)).toBeNull();
    expect(resolveEnlargedView('location', false, false)).toBeNull();
  });
});
