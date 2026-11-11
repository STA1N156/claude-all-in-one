import type { RequestLogRow } from './types'

/** 积分反推命中比例达到这个值就算「热」(与后端 `CREDIT_WARM_THRESHOLD` 一致)。 */
export const CREDIT_WARM_THRESHOLD = 0.5

/**
 * 日志「命中」列的展示口径(先中先得):
 * - `real`:上游回报了真实缓存读(目前只有 cursor 的部分模型会报);
 * - `credit`:Kiro 不回报真实命中,改按实扣积分反推命中比例(后端算好的 `credit_hit_ratio`);
 * - `unknown`:是 Kiro 请求(有积分)但不满足反推前提(长输出 / 输入太小 / 参数表没有该模型);
 * - `miss`:其余 —— 上游明确没命中,或旧后端没有反推字段。
 */
export type CacheHitView =
  | { kind: 'real'; tokens: number }
  | { kind: 'credit'; ratio: number; warm: boolean }
  | { kind: 'unknown' }
  | { kind: 'miss' }

export function cacheHitView(
  row: Pick<RequestLogRow, 'real_cache_read_tokens' | 'metering_credit' | 'credit_hit_ratio'>,
): CacheHitView {
  if (row.real_cache_read_tokens > 0) return { kind: 'real', tokens: row.real_cache_read_tokens }
  const ratio = row.credit_hit_ratio
  if (typeof ratio === 'number' && Number.isFinite(ratio)) {
    return { kind: 'credit', ratio, warm: ratio >= CREDIT_WARM_THRESHOLD }
  }
  if (row.metering_credit > 0 && row.credit_hit_ratio === null) return { kind: 'unknown' }
  return { kind: 'miss' }
}
