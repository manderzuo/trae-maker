import { describe, expect, it } from 'vitest';
import { coreJobTone, keyQuotaActionLabel, normalizeCoreScopes } from './CoreAdminPanel';

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

  it('describes key-budget actions without exposing key material', () => {
    expect(keyQuotaActionLabel('grant')).toBe('发放 Key 额度');
    expect(keyQuotaActionLabel('allocate')).toBe('迁移 legacy 额度');
    expect(keyQuotaActionLabel('balance')).toBe('查看 Key 余额');
  });
});
