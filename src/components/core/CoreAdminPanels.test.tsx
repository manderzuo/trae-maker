import { describe, expect, it } from 'vitest';

import {
  coreJobTone,
  coreQuotaSummary,
  normalizeCoreScopes,
  shouldRequireDangerConfirmation,
  shouldShowReplayAction,
} from './coreModel';

describe('CORE admin panels', () => {
  it('renders the four quota states without treating held as settled', () => {
    expect(coreQuotaSummary({ available: 12, held: 3, settled: 5 })).toEqual({
      total: 20,
      available: 12,
      held: 3,
      settled: 5,
    });
  });

  it('keeps scopes exact and de-duplicated', () => {
    expect(normalizeCoreScopes(['chat:invoke', 'videos:submit', 'chat:invoke'])).toEqual([
      'chat:invoke',
      'videos:submit',
    ]);
  });

  it('never exposes a replay action for unknown or reconcile-required jobs', () => {
    expect(shouldShowReplayAction({ state: 'succeeded', reconcile_required: false })).toBe(true);
    expect(shouldShowReplayAction({ state: 'unknown', reconcile_required: false })).toBe(false);
    expect(shouldShowReplayAction({ state: 'failed', reconcile_required: true })).toBe(false);
  });

  it('requires explicit confirmation for irreversible admin mutations', () => {
    expect(shouldRequireDangerConfirmation('revoke_key')).toBe(true);
    expect(shouldRequireDangerConfirmation('disable_user')).toBe(true);
    expect(shouldRequireDangerConfirmation('refresh')).toBe(false);
    expect(coreJobTone('unknown')).toBe('amber');
  });
});
