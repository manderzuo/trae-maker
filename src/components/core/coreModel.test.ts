import { describe, expect, it } from 'vitest';

import {
  coreMigrationTone,
  coreQuotaSummary,
  normalizeCoreScopes,
  shouldAllowMigrationApply,
} from './coreModel';

describe('CORE workspace model helpers', () => {
  it('keeps available, held and settled separate', () => {
    expect(coreQuotaSummary({ available: 40, held: 10, settled: 50 })).toEqual({
      total: 100,
      available: 40,
      held: 10,
      settled: 50,
    });
  });

  it('normalizes scopes without creating duplicate permissions', () => {
    expect(normalizeCoreScopes([' videos:submit ', 'chat:invoke', 'videos:submit', '']))
      .toEqual(['chat:invoke', 'videos:submit']);
  });

  it('uses a warning tone for migration findings', () => {
    expect(coreMigrationTone({ errors: [], unmapped_keys: [] })).toBe('green');
    expect(coreMigrationTone({ errors: ['invalid JSON'], unmapped_keys: [] })).toBe('red');
    expect(coreMigrationTone({ errors: [], unmapped_keys: ['legacy-key'] })).toBe('amber');
  });

  it('only enables migration apply after a clean inspect report', () => {
    expect(shouldAllowMigrationApply({ errors: [], unmapped_keys: [] })).toBe(true);
    expect(shouldAllowMigrationApply({ errors: ['missing legacy file'], unmapped_keys: [] })).toBe(false);
    expect(shouldAllowMigrationApply({ errors: [], unmapped_keys: ['key-1'] })).toBe(false);
  });
});
