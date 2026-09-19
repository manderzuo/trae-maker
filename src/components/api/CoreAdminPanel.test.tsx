import { describe, expect, it } from 'vitest';
import { normalizeCoreScopes } from './CoreAdminPanel';

describe('CoreAdminPanel helpers', () => {
  it('normalizes and de-duplicates comma-separated scopes', () => {
    expect(normalizeCoreScopes(' video.submit, chat,video.submit ,, ')).toEqual([
      'video.submit',
      'chat',
    ]);
  });
});
