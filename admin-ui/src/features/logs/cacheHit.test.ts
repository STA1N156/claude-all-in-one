import { describe, expect, it } from 'vitest'

import { cacheHitView } from './cacheHit'

describe('日志命中列口径', () => {
  it('上游回报了真实命中 → 直接用', () => {
    expect(cacheHitView({ real_cache_read_tokens: 800, metering_credit: 0, credit_hit_ratio: null })).toEqual({
      kind: 'real',
      tokens: 800,
    })
  })

  it('Kiro 请求按积分反推,≥0.5 算热', () => {
    expect(cacheHitView({ real_cache_read_tokens: 0, metering_credit: 3.26, credit_hit_ratio: 1 })).toEqual({
      kind: 'credit',
      ratio: 1,
      warm: true,
    })
    expect(cacheHitView({ real_cache_read_tokens: 0, metering_credit: 11.8, credit_hit_ratio: 0.02 })).toMatchObject({
      kind: 'credit',
      warm: false,
    })
  })

  it('Kiro 请求但不满足反推前提 → 无信号,不报 miss', () => {
    expect(cacheHitView({ real_cache_read_tokens: 0, metering_credit: 9, credit_hit_ratio: null }).kind).toBe('unknown')
  })

  it('旧后端没有反推字段 / 非 Kiro 无积分 → miss(旧行为)', () => {
    expect(cacheHitView({ real_cache_read_tokens: 0, metering_credit: 3, credit_hit_ratio: undefined }).kind).toBe('miss')
    expect(cacheHitView({ real_cache_read_tokens: 0, metering_credit: 0, credit_hit_ratio: null }).kind).toBe('miss')
  })
})
