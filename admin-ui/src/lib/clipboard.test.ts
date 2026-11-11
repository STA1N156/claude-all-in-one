// @vitest-environment happy-dom
import { describe, expect, it, vi } from 'vitest'

import { copyText } from './clipboard'

/**
 * 后台常跑在 http:// 内网地址上,那里 `navigator.clipboard` 是 **undefined**(非安全上下文)。
 * 历史上三处复制按钮直接调它 → 点了静默没反应(「按钮点不动」)。这里锁住回退路径。
 */
describe('copyText', () => {
  it('clipboard API 可用时走 Clipboard API', async () => {
    const writeText = vi.fn().mockResolvedValue(undefined)
    vi.stubGlobal('navigator', { clipboard: { writeText } })
    await expect(copyText('abc')).resolves.toBe(true)
    expect(writeText).toHaveBeenCalledWith('abc')
    vi.unstubAllGlobals()
  })

  it('clipboard 不可用(http 内网)时回退 execCommand', async () => {
    vi.stubGlobal('navigator', {})
    const exec = vi.fn().mockReturnValue(true)
    document.execCommand = exec
    await expect(copyText('fallback-me')).resolves.toBe(true)
    expect(exec).toHaveBeenCalledWith('copy')
    // 回退用的小 textarea 用完要清掉,不留垃圾节点。
    expect(document.querySelector('textarea')).toBeNull()
  })

  it('两条路都失败时返回 false(调用方据此不给「已复制」假象)', async () => {
    vi.stubGlobal('navigator', {})
    document.execCommand = vi.fn().mockImplementation(() => {
      throw new Error('denied')
    })
    await expect(copyText('x')).resolves.toBe(false)
  })
})
