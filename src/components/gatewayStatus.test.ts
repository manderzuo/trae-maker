import { describe, expect, it } from 'vitest';

import { shouldIgnoreStaleGatewayToggle } from './gatewayStatus';

describe('gateway toggle state guard', () => {
  it('ignores a stale start click when the gateway is already running', () => {
    expect(shouldIgnoreStaleGatewayToggle(false, true)).toBe(true);
  });

  it('ignores a stale stop click when the gateway has already stopped', () => {
    expect(shouldIgnoreStaleGatewayToggle(true, false)).toBe(true);
  });

  it('allows a toggle when the displayed state matches the runtime', () => {
    expect(shouldIgnoreStaleGatewayToggle(false, false)).toBe(false);
    expect(shouldIgnoreStaleGatewayToggle(true, true)).toBe(false);
  });
});
