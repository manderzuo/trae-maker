import { describe, expect, it } from 'vitest';

import {
  DEFAULT_GATEWAY_LIMITS,
  buildGatewaySettingsPayload,
  normalizeGatewayLimitDefaults,
  validateGatewayLimitDefaults,
} from './InterfaceConfig';
import type { GatewaySettings } from '../../types';

const currentSettings: GatewaySettings = {
  port: 9000,
  default_model: 'custom-model',
  listen_host: '0.0.0.0',
  cors_origins: 'https://example.com',
  asset_public_base_url: 'https://assets.example.com/v1',
  updated_at: 123,
};

describe('InterfaceConfig limit helpers', () => {
  it('fills missing global defaults with the documented safe defaults', () => {
    expect(normalizeGatewayLimitDefaults(undefined)).toEqual(DEFAULT_GATEWAY_LIMITS);
  });

  it('rejects zero and oversized global values before saving', () => {
    expect(
      validateGatewayLimitDefaults({
        ...DEFAULT_GATEWAY_LIMITS,
        max_inflight: 0,
      }),
    ).toContain('文字请求并发');
    expect(
      validateGatewayLimitDefaults({
        ...DEFAULT_GATEWAY_LIMITS,
        asset_bytes_per_hour: 10 * 1024 * 1024 * 1024 + 1,
      }),
    ).toContain('素材容量');
  });

  it('adds limit defaults without dropping existing gateway settings', () => {
    const next = buildGatewaySettingsPayload(currentSettings, {
      ...DEFAULT_GATEWAY_LIMITS,
      max_inflight: 8,
    });

    expect(next).toMatchObject({
      port: 9000,
      default_model: 'custom-model',
      listen_host: '0.0.0.0',
      cors_origins: 'https://example.com',
      asset_public_base_url: 'https://assets.example.com/v1',
      updated_at: 123,
      limit_defaults: {
        ...DEFAULT_GATEWAY_LIMITS,
        max_inflight: 8,
      },
    });
  });
});
