import { describe, expect, it } from 'vitest';
import { detailLines } from './detail';
import type { Suggestion } from '../types';

function sample(over: Partial<Suggestion> = {}): Suggestion {
  return {
    id: 's-1',
    type: 'move',
    asset_kind: 'photo',
    target: '/a/b.jpg',
    new_path: '/a/p/2024/b.jpg',
    reason: 'organize',
    selected: false,
    status: 'pending',
    dependent_operations: [{ type: 'repair_symlink', link_path: '/a/L' }],
    ...over,
  };
}

describe('detail', () => {
  it('renders key fields in order', () => {
    const lines = detailLines(sample());
    expect(lines[0]).toEqual(['id', 's-1']);
    expect(lines[3]).toEqual(['status', 'pending']);
    expect(lines[4]).toEqual(['target', '/a/b.jpg']);
    expect(lines.some(([k]) => k === 'new path')).toBe(true);
  });


  it('labels dependent operations with their fields', () => {
    const lines = detailLines(sample());
    const dep = lines.find(([k]) => k === 'dependent');
    expect(dep).toBeTruthy();
    expect(dep![1]).toContain('repair_symlink');
    expect(dep![1]).toContain('link_path=/a/L');
  });

  it('flags rejected suggestions', () => {
    const lines = detailLines(sample({ status: 'rejected', rejection_reason: 'path conflict' }));
    const rej = lines.find(([k]) => k === 'rejected');
    expect(rej![1]).toBe('path conflict');
  });
});