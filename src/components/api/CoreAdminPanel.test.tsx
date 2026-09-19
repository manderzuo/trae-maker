import { describe, expect, it } from 'vitest';
import { coreJobTone, normalizeCoreScopes } from './CoreAdminPanel';

describe('CoreAdminPanel helpers', () => {
  it('normalizes and de-duplicates comma-separated scopes', () => {
    expect(normalizeCoreScopes(' video.submit, chat,video.submit ,, ')).toEqual([
      'video.submit',
      'chat',
    ]);
  });

  it('uses a conservative tone for durable job states', () => {
    expect(coreJobTone('succeeded')).toBe('green');
    expect(coreJobTone('unknown')).toBe('amber');
    expect(coreJobTone('failed')).toBe('red');
    expect(coreJobTone('unexpected')).toBe('slate');
  });
});
