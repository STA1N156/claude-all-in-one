import type { AccountUsage } from './types'

/** 按账号积分表的状态三态:账号行被删(元信息全 null)单独成一类,别和「停用」混。 */
export type AccountUsageStatus = 'active' | 'disabled' | 'deleted'

export function accountUsageStatus(row: AccountUsage): AccountUsageStatus {
  if (row.provider === null) return 'deleted'
  return row.disabled ? 'disabled' : 'active'
}

/**
 * 按账号积分表要展示的行:Kiro 号 + 已删除的号(provider 未知,历史上只有 Kiro 有积分)。
 * cursor / dario 没有积分概念(恒 0),列出来只是噪音。`search` 按 account_id 子串、忽略大小写。
 * 不改顺序 —— 后端已按积分降序。
 */
export function filterAccountUsage(rows: AccountUsage[], search: string): AccountUsage[] {
  const q = search.trim().toLowerCase()
  return rows.filter(
    (r) =>
      (r.provider === null || r.provider === 'kiro') &&
      (q === '' || r.account_id.toLowerCase().includes(q)),
  )
}

/** 每次成功请求平均积分(失败请求不计费、不入分母);没有成功请求 → null(显示 —)。 */
export function creditsPerRequest(row: AccountUsage): number | null {
  return row.success_requests > 0 ? row.metering_credit / row.success_requests : null
}

/**
 * 缓存命中率(模拟口径)= 缓存模拟器给出的缓存读 / 输入总量(caio 的 input_tokens 记的是总上下文)。
 * 不用 real_cache_read:Kiro 已基本不回报真实命中,按号看恒为 0。
 */
export function cacheHitRate(row: AccountUsage): number | null {
  return row.input_tokens > 0 ? row.cache_read_tokens / row.input_tokens : null
}

export function sumCredits(rows: AccountUsage[]): number {
  return rows.reduce((acc, r) => acc + r.metering_credit, 0)
}
