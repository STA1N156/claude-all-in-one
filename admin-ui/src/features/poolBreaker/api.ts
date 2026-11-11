import { api } from '@/lib/api'

import type { PoolBreakerStatus } from './types'

/** GET /admin/api/pool-breaker → 号池熔断状态(库内真值,worker 判定后 30 秒内落库)。 */
export async function fetchPoolBreaker(): Promise<PoolBreakerStatus> {
  const response = await api.get<PoolBreakerStatus>('/pool-breaker')
  return response.data
}

/** POST /admin/api/pool-breaker/reset → 手动恢复接单(worker 30 秒内生效)。 */
export async function resetPoolBreaker(): Promise<PoolBreakerStatus> {
  const response = await api.post<PoolBreakerStatus>('/pool-breaker/reset')
  return response.data
}
