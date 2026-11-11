/** 号池熔断状态(与后端 `admin/pool_breaker.rs` 的 GET 回包一致)。 */
export type PoolBreakerEventKind = 'suspended' | 'downgraded'

export interface PoolBreakerEvent {
  account: string
  kind: PoolBreakerEventKind
  /** unix 毫秒。 */
  at: number
}

export interface PoolBreakerStatus {
  tripped: boolean
  /** unix 毫秒;从未熔断过为 null。 */
  tripped_at: number | null
  events: PoolBreakerEvent[]
  /** 上次手动恢复的时间(unix 毫秒)。 */
  reset_at: number | null
  /** false = 熔断还只在 worker 内存里,没进库(刚触发 30 秒内,或写库一直失败)。 */
  persisted?: boolean
}
