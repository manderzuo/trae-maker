import type { CoreMigrationReport, CoreKeyQuotaBalanceResponse } from '../../types';

export interface CoreQuotaSummary {
  total: number;
  available: number;
  held: number;
  settled: number;
}

export function coreQuotaSummary(
  balance: Pick<CoreKeyQuotaBalanceResponse, 'available' | 'held' | 'settled'>,
): CoreQuotaSummary {
  const available = Number(balance.available) || 0;
  const held = Number(balance.held) || 0;
  const settled = Number(balance.settled) || 0;
  return {
    total: available + held + settled,
    available,
    held,
    settled,
  };
}

export function normalizeCoreScopes(scopes: string[]): string[] {
  return [...new Set(scopes.map((scope) => scope.trim()).filter(Boolean))].sort();
}

export function coreMigrationTone(
  report: Pick<CoreMigrationReport, 'errors' | 'unmapped_keys'>,
): 'green' | 'amber' | 'red' {
  if (report.errors.length > 0) return 'red';
  if (report.unmapped_keys.length > 0) return 'amber';
  return 'green';
}

export function shouldAllowMigrationApply(
  report: Pick<CoreMigrationReport, 'errors' | 'unmapped_keys'> &
    Partial<Pick<CoreMigrationReport, 'processing_video_count'>>,
): boolean {
  return report.errors.length === 0
    && report.unmapped_keys.length === 0
    && (report.processing_video_count ?? 0) === 0;
}

export function shouldShowReplayAction(job: { state: string; reconcile_required: boolean }): boolean {
  return !job.reconcile_required && job.state === 'succeeded';
}

export function shouldRequireDangerConfirmation(action: string): boolean {
  return action === 'revoke_key' || action === 'disable_user' || action === 'grant_quota' || action === 'migration_apply';
}

export function coreJobTone(state: string): 'green' | 'amber' | 'slate' | 'red' | 'blue' {
  if (state === 'succeeded' || state === 'canceled') return 'green';
  if (state === 'unknown' || state === 'cancel_requested') return 'amber';
  if (state === 'failed') return 'red';
  if (state === 'running') return 'blue';
  return 'slate';
}
