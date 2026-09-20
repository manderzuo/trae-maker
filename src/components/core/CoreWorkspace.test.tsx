import { describe, expect, it } from 'vitest';

import {
  CORE_WORKSPACE_SECTIONS,
  clearCoreAdminSession,
  emptyCoreAdminSession,
} from './CoreWorkspace';

describe('CoreWorkspace shell', () => {
  it('exposes the five independent CORE sections', () => {
    expect(CORE_WORKSPACE_SECTIONS.map((section) => section.key)).toEqual([
      'overview',
      'users',
      'quota',
      'jobs',
      'migration',
    ]);
  });

  it('clears the administrator key and sensitive lists together', () => {
    const cleared = clearCoreAdminSession({
      ...emptyCoreAdminSession,
      adminApiKey: 'core-secret',
      users: [{ id: 'u1' } as never],
      keys: [{ id: 'k1' } as never],
      videoJobs: [{ id: 'job1' } as never],
    });
    expect(cleared.adminApiKey).toBe('');
    expect(cleared.users).toEqual([]);
    expect(cleared.keys).toEqual([]);
    expect(cleared.videoJobs).toEqual([]);
  });
});
