import { describe, expect, it } from 'vitest'

import { poolBreakerPatch, poolBreakerToForm } from './components/PoolBreakerCard'
import type { SystemSettings } from './types'

const base = {} as SystemSettings

describe('poolBreaker 表单', () => {
  it('旧后端不返回时按后端默认(enforce / 30 分钟 / 3 个号)显示,且不产生 patch', () => {
    const form = poolBreakerToForm(base)
    expect(form).toEqual({ mode: 'enforce', window_minutes: '30', threshold: '3' })
    expect(poolBreakerPatch(form, base)).toEqual({})
  })

  it('分钟换算成秒,只回传变了的键,非法数字不提交', () => {
    const form = { mode: 'observe' as const, window_minutes: '60', threshold: 'x' }
    expect(poolBreakerPatch(form, base)).toEqual({ pool_breaker_mode: 'observe', pool_breaker_window_secs: 3600 })
  })
})
