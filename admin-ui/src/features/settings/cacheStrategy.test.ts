import { describe, expect, it } from 'vitest'

import {
  buildCacheStrategyPatch,
  cacheStrategyFormFromSettings,
  hasCacheStrategyErrors,
  validateCacheStrategyForm,
} from './cacheStrategy'
import { same } from './components/WorkerEffectiveCard'
import type { SystemSettings } from './types'

/** 旧版本后端：不带任何缓存策略字段。 */
function legacySettings(): SystemSettings {
  return {
    default_proxy: null,
    egress_pool: null,
    cache_read_multiplier: 1,
    cache_cap_ratio: 0.95,
    cache_floor_ratio: 0.5,
    cache_sim_ttl_secs: 1800,
    cache_max_sessions: 4096,
    rate_limit_cooldown_secs: 60,
    suspended_cooldown_secs: 3600,
    empty_response_cooldown_secs: 60,
    empty_response_window_secs: 60,
    empty_response_threshold: 3,
    affinity_ttl_secs: 1800,
    max_failures: 5,
    max_switch_attempts: 2,
    quota_poll_enabled: true,
    image_enabled: true,
    image_max_long_edge: 2000,
    image_max_pixels_single: 3750000,
    image_max_pixels_multi: 1150000,
    image_multi_threshold: 20,
    tools_in_prefix: false,
    cache_point: false,
    agent_continuation: false,
    thinking_signature: true,
    q_endpoint: false,
    default_thinking_effort: 'medium',
    history_thinking_turns: -1,
  }
}

function currentSettings(): SystemSettings {
  return {
    ...legacySettings(),
    cache_strategy: 'sim',
    cache_credit_max_output: 1500,
    cache_credit_discount_only: false,
    cache_credit_enterprise: true,
    cache_io_correction: false,
    cache_credit_models: {
      'claude-opus-5.5': { miss: 1.23, hit: 0.365, out: 0.44, base: -0.012 },
      'claude-sonnet-5.5': { miss: 0.815, hit: 0.246, out: 0.256, base: -0.014 },
    },
  }
}

describe('cacheStrategy form', () => {
  it('旧后端：表单取基线默认，原样提交不产生任何字段', () => {
    const server = legacySettings()
    const form = cacheStrategyFormFromSettings(server)
    expect(form.cache_strategy).toBe('sim')
    expect(form.cache_credit_max_output).toBe('1500')
    expect(form.cache_io_correction).toBe(false)
    expect(form.cache_credit_enterprise).toBe(true)
    expect(form.models).toEqual([])
    expect(buildCacheStrategyPatch(form, server)).toEqual({})
    expect(hasCacheStrategyErrors(validateCacheStrategyForm(form))).toBe(false)
  })

  it('未改动不提交；行顺序变化不算改动', () => {
    const server = currentSettings()
    const form = cacheStrategyFormFromSettings(server)
    expect(buildCacheStrategyPatch(form, server)).toEqual({})
    const reordered = { ...form, models: [...form.models].reverse() }
    expect(buildCacheStrategyPatch(reordered, server)).toEqual({})
  })

  it('只提交改动的字段，参数表整张提交', () => {
    const server = currentSettings()
    const form = cacheStrategyFormFromSettings(server)
    const edited = {
      ...form,
      cache_strategy: 'credit_correct' as const,
      cache_credit_discount_only: true,
      cache_credit_enterprise: false,
      cache_io_correction: true,
      models: form.models.map((r) => (r.model === 'claude-opus-5.5' ? { ...r, hit: '0.35' } : r)),
    }
    expect(buildCacheStrategyPatch(edited, server)).toEqual({
      cache_strategy: 'credit_correct',
      cache_credit_discount_only: true,
      cache_credit_enterprise: false,
      cache_io_correction: true,
      cache_credit_models: {
        'claude-opus-5.5': { miss: 1.23, hit: 0.35, out: 0.44, base: -0.012 },
        'claude-sonnet-5.5': { miss: 0.815, hit: 0.246, out: 0.256, base: -0.014 },
      },
    })
  })

  it('恢复内置参数发 null', () => {
    const server = currentSettings()
    const form = { ...cacheStrategyFormFromSettings(server), resetModels: true }
    expect(buildCacheStrategyPatch(form, server)).toEqual({ cache_credit_models: null })
  })

  it('校验规则与后端一致，非法值不提交', () => {
    const server = currentSettings()
    const base = cacheStrategyFormFromSettings(server)
    const row = { model: 'm', miss: '1', hit: '0.3', out: '0.1', base: '0' }
    const form = {
      ...base,
      cache_credit_max_output: '100001',
      models: [
        { ...row, model: ' ' },
        { ...row, hit: '1' },
        { ...row, model: 'n', out: '-1' },
        { ...row, model: 'o', base: '2' },
        { ...row, model: 'p', miss: 'x' },
        { ...row, model: 'q' },
        { ...row, model: 'q' },
      ],
    }
    const e = validateCacheStrategyForm(form)
    expect(e.maxOutput).toBe('outOfRange')
    expect(e.rows).toEqual({ 0: 'model', 1: 'order', 2: 'out', 3: 'base', 4: 'number', 6: 'duplicate' })
    expect(buildCacheStrategyPatch(form, server)).toEqual({})
    expect(validateCacheStrategyForm({ ...base, cache_credit_max_output: '1.5' }).maxOutput).toBe('invalid')
  })
})

describe('WorkerEffectiveCard same()', () => {
  it('表格类字段按内容比较，浮点有容差', () => {
    expect(same({ a: { miss: 0.9 } }, { a: { miss: 0.9000000000000001 } })).toBe(true)
    expect(same({ a: { miss: 0.9 } }, { a: { miss: 0.8 } })).toBe(false)
    expect(same({ a: 1 }, { a: 1, b: 2 })).toBe(false)
    expect(same(['x', 'y'], ['x', 'y'])).toBe(true)
    expect(same(['x'], { 0: 'x' })).toBe(false)
    expect(same(null, {})).toBe(false)
    expect(same('sim', 'sim')).toBe(true)
  })
})
