import { describe, expect, it } from 'vitest'

import {
  accountUsageStatus,
  cacheHitRate,
  creditsPerRequest,
  filterAccountUsage,
  sumCredits,
} from './lib'
import type { AccountUsage } from './types'

function acct(account_id: string, over: Partial<AccountUsage> = {}): AccountUsage {
  return {
    account_id,
    requests: 10,
    success_requests: 8,
    input_tokens: 1000,
    output_tokens: 50,
    cache_read_tokens: 600,
    metering_credit: 4,
    last_used_at: 1_780_000_000,
    provider: 'kiro',
    group_name: 'G0',
    disabled: false,
    subscription_title: 'KIRO PRO+',
    ...over,
  }
}

describe('按账号积分:状态', () => {
  it('元信息为 null = 已删除,优先于 disabled', () => {
    expect(accountUsageStatus(acct('a', { provider: null, disabled: null }))).toBe('deleted')
    expect(accountUsageStatus(acct('a', { disabled: true }))).toBe('disabled')
    expect(accountUsageStatus(acct('a'))).toBe('active')
  })
})

describe('按账号积分:过滤', () => {
  const rows = [
    acct('Lark.spatial-1o-icloud.com'),
    acct('cur-1', { provider: 'cursor' }),
    acct('dario-1', { provider: 'claude-dario' }),
    acct('gone-1', { provider: null, disabled: null }),
  ]

  it('只留 kiro 与已删除的号,保持后端顺序', () => {
    expect(filterAccountUsage(rows, '').map((r) => r.account_id)).toEqual([
      'Lark.spatial-1o-icloud.com',
      'gone-1',
    ])
  })

  it('搜索按子串、忽略大小写、去首尾空白', () => {
    expect(filterAccountUsage(rows, '  lark ').map((r) => r.account_id)).toEqual([
      'Lark.spatial-1o-icloud.com',
    ])
    expect(filterAccountUsage(rows, 'cur')).toEqual([])
  })
})

describe('按账号积分:派生指标', () => {
  it('每请求积分按成功请求数算,无成功请求为 null', () => {
    expect(creditsPerRequest(acct('a'))).toBe(0.5)
    expect(creditsPerRequest(acct('a', { success_requests: 0 }))).toBeNull()
  })

  it('命中率 = 缓存读 / 输入总量,无输入为 null', () => {
    expect(cacheHitRate(acct('a'))).toBe(0.6)
    expect(cacheHitRate(acct('a', { input_tokens: 0 }))).toBeNull()
  })

  it('合计积分', () => {
    expect(sumCredits([acct('a'), acct('b', { metering_credit: 1.5 })])).toBe(5.5)
    expect(sumCredits([])).toBe(0)
  })
})
