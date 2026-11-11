import { describe, expect, it } from 'vitest'

import { batchGuardPatch, batchGuardToForm, linesToList } from './components/BatchGuardCard'
import type { SystemSettings } from './types'

// 只用到批量拦截相关字段;其余字段与本测试无关。
const base = {} as SystemSettings

describe('batchGuard 表单', () => {
  it('旧后端不返回时按后端默认显示,且不产生任何 patch', () => {
    const form = batchGuardToForm(base)
    expect(form).toEqual({
      mode: 'observe',
      window_secs: '600',
      threshold: '40',
      block_secs: '21600',
      prefix_chars: '80',
      block_prefixes: '',
      allow_prefixes: '',
    })
    expect(batchGuardPatch(form, base)).toEqual({})
  })

  it('只回传变了的键;名单去空白去空行去重', () => {
    const form = {
      ...batchGuardToForm(base),
      mode: 'enforce' as const,
      threshold: '30',
      block_prefixes: '  You are a strict, calibrated senior VFX art director \n\nYou are a strict, calibrated senior VFX art director',
    }
    expect(batchGuardPatch(form, base)).toEqual({
      batch_guard_mode: 'enforce',
      batch_guard_threshold: 30,
      batch_guard_block_prefixes: ['You are a strict, calibrated senior VFX art director'],
    })
  })

  it('非法数字不提交', () => {
    const form = { ...batchGuardToForm(base), window_secs: '2.5', block_secs: '-5' }
    expect(batchGuardPatch(form, base)).toEqual({})
  })

  it('清空名单要显式提交空数组', () => {
    const original = { ...base, batch_guard_block_prefixes: ['x'] } as SystemSettings
    const form = { ...batchGuardToForm(original), block_prefixes: '' }
    expect(batchGuardPatch(form, original)).toEqual({ batch_guard_block_prefixes: [] })
  })

  it('linesToList', () => {
    expect(linesToList(' a \nb\n\na ')).toEqual(['a', 'b'])
  })
})
