import { useCallback, useState } from 'react';
import { api } from '../../lib/tauri';
import type {
  CoreApiKeyAdminView,
  CoreSchedulerStatus,
  CoreStatus,
  CoreUserAdminView,
  CoreVideoJobAdminView,
} from '../../types';

export interface CoreAdminSessionState {
  adminApiKey: string;
  status: CoreStatus | null;
  scheduler: CoreSchedulerStatus | null;
  users: CoreUserAdminView[];
  keys: CoreApiKeyAdminView[];
  videoJobs: CoreVideoJobAdminView[];
  loading: boolean;
  error: string;
}

export const emptyCoreAdminSession: CoreAdminSessionState = {
  adminApiKey: '',
  status: null,
  scheduler: null,
  users: [],
  keys: [],
  videoJobs: [],
  loading: false,
  error: '',
};

function publicError(error: unknown): string {
  const message = String(error).replace(/^Error:\s*/, '').trim();
  return message ? message.slice(0, 180) : 'Core 管理请求失败';
}

export function useCoreAdminSession() {
  const [session, setSession] = useState<CoreAdminSessionState>(emptyCoreAdminSession);

  const setAdminApiKey = useCallback((adminApiKey: string) => {
    setSession((current) => ({ ...current, adminApiKey, error: '' }));
  }, []);

  const refreshStatus = useCallback(async () => {
    try {
      const status = await api.core.status();
      setSession((current) => ({ ...current, status, error: '' }));
      return status;
    } catch (error) {
      setSession((current) => ({ ...current, error: `读取 Core 状态失败：${publicError(error)}` }));
      throw error;
    }
  }, []);

  const loadAll = useCallback(async (key = session.adminApiKey) => {
    const normalized = key.trim();
    if (!normalized) {
      setSession((current) => ({ ...current, error: '请输入 Core 管理员 API Key' }));
      return false;
    }
    setSession((current) => ({ ...current, loading: true, error: '' }));
    try {
      const [users, keys, scheduler, videoJobs] = await Promise.all([
        api.core.usersList(normalized),
        api.core.apiKeysList(normalized),
        api.core.schedulerStatus(normalized),
        api.core.videoJobsList(normalized, null, 100),
      ]);
      setSession((current) => ({
        ...current,
        adminApiKey: normalized,
        users,
        keys,
        scheduler,
        videoJobs,
        loading: false,
        error: '',
      }));
      return true;
    } catch (error) {
      // 刷新失败保留旧列表，避免暂时断线时把工作台渲染成空白。
      setSession((current) => ({ ...current, loading: false, error: publicError(error) }));
      return false;
    }
  }, [session.adminApiKey]);

  const clearSession = useCallback(() => {
    setSession(emptyCoreAdminSession);
  }, []);

  return {
    ...session,
    hasAdminKey: session.adminApiKey.trim().length > 0,
    setAdminApiKey,
    refreshStatus,
    loadAll,
    clearSession,
  };
}
