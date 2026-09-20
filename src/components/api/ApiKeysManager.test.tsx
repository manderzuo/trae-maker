import { describe, expect, it } from 'vitest';

import {
  DEFAULT_KEY_CAPABILITIES,
  buildKeyPolicyUpdate,
  effectiveKeyLimitSummary,
  normalizeKeyCapabilities,
  normalizeKeyLimits,
  validateKeyLimits,
} from './ApiKeysManager';
import type { ApiKeyEntry, GatewayLimitDefaults } from '../../types';

const globalLimits: GatewayLimitDefaults = {
  max_inflight: 32,
  max_video_jobs: 32,
  asset_uploads_per_minute: 30,
  asset_bytes_per_hour: 256 * 1024 * 1024,
  video_submissions_per_minute: 3,
};

const legacyKey: ApiKeyEntry = {
  id: 'key-1',
  name: 'legacy',
  key: 'ck_secret-value',
  enabled: true,
  daily_limit: 0,
  created_at: 0,
  used_date: '',
  used_today: 0,
  allowed_accounts: ['account-a'],
  schedule_mode: 'dedicated',
  dedicated_account: 'account-a',
  daily_stats: [],
};

describe('ApiKeysManager policy helpers', () => {
  it('defaults legacy keys to inherited limits and all capabilities', () => {
    expect(normalizeKeyLimits(undefined)).toEqual({
      max_inflight: null,
      max_video_jobs: null,
      asset_uploads_per_minute: null,
      asset_bytes_per_hour: null,
      video_submissions_per_minute: null,
      daily_requests: 0,
      daily_tokens: 0,
    });
    expect(normalizeKeyCapabilities(undefined)).toEqual(DEFAULT_KEY_CAPABILITIES);
    expect(normalizeKeyCapabilities([])).toEqual([]);
  });

  it('builds a save payload without dropping scheduling configuration', () => {
    const next = buildKeyPolicyUpdate(
      legacyKey,
      {
        max_inflight: 2,
        max_video_jobs: 1,
        asset_uploads_per_minute: null,
        asset_bytes_per_hour: 1024,
        video_submissions_per_minute: 1,
        daily_requests: 5,
        daily_tokens: 10_000,
      },
      ['chat', 'assets'],
    );

    expect(next.limits).toMatchObject({
      max_inflight: 2,
      max_video_jobs: 1,
      asset_uploads_per_minute: null,
      asset_bytes_per_hour: 1024,
      video_submissions_per_minute: 1,
      daily_requests: 5,
      daily_tokens: 10_000,
    });
    expect(next.capabilities).toEqual(['chat', 'assets']);
    expect(next.allowed_accounts).toEqual(['account-a']);
    expect(next.schedule_mode).toBe('dedicated');
    expect(next.dedicated_account).toBe('account-a');
  });

  it('rejects invalid daily and nullable limit values before saving', () => {
    expect(
      validateKeyLimits({
        ...normalizeKeyLimits(undefined),
        daily_requests: -1,
      }),
    ).toContain('每日请求');
    expect(
      validateKeyLimits({
        ...normalizeKeyLimits(undefined),
        max_inflight: 257,
      }),
    ).toContain('并发');
    expect(
      validateKeyLimits({
        ...normalizeKeyLimits(undefined),
        asset_bytes_per_hour: 0,
      }),
    ).toContain('素材容量');
  });

  it('shows effective values capped by global defaults', () => {
    const summary = effectiveKeyLimitSummary(
      {
        ...normalizeKeyLimits(undefined),
        max_inflight: 64,
        max_video_jobs: 4,
        asset_uploads_per_minute: null,
        asset_bytes_per_hour: 512 * 1024 * 1024,
        video_submissions_per_minute: 1,
      },
      globalLimits,
    );

    expect(summary.max_inflight).toBe(32);
    expect(summary.max_video_jobs).toBe(4);
    expect(summary.asset_uploads_per_minute).toBe(30);
    expect(summary.asset_bytes_per_hour).toBe(256 * 1024 * 1024);
    expect(summary.video_submissions_per_minute).toBe(1);
  });
});
