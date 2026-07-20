interface DoorTestHooks {
  afterIngestionStat?: (path: string) => void;
  afterMetadataStat?: (path: string) => void;
  beforeStatePublish?: () => void;
  beforeStaleRecovery?: (
    owner: { pid: number; token: string },
  ) => void | Promise<void>;
  afterStaleOwnerRead?: (
    owner: { pid: number; token: string },
  ) => void | Promise<void>;
}

/** Internal fault injection. This module is intentionally absent from index.ts. */
export const testHooks: DoorTestHooks = {};

export function resetTestHooks(): void {
  delete testHooks.afterIngestionStat;
  delete testHooks.afterMetadataStat;
  delete testHooks.beforeStatePublish;
  delete testHooks.beforeStaleRecovery;
  delete testHooks.afterStaleOwnerRead;
}
