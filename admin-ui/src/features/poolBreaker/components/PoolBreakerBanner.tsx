import { useState } from 'react'
import { OctagonAlert } from 'lucide-react'

import { Button } from '@/components/ui/button'
import { useI18n } from '@/lib/i18n'

import { usePoolBreaker, useResetPoolBreaker } from '../hooks'

function hhmm(ms: number): string {
  const d = new Date(ms)
  return `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`
}

/**
 * 号池熔断横幅:熔断中时钉在每个页面顶上,带「恢复接单」按钮。
 *
 * 熔断只能手动恢复(用户 2026-11-09 定),所以这里是唯一的恢复入口;按钮要二次确认,
 * 免得在没查清原因前误点 —— 恢复后同一个批量负载还在的话,会接着封号。
 */
export function PoolBreakerBanner() {
  const { t } = useI18n()
  const status = usePoolBreaker()
  const reset = useResetPoolBreaker()
  // 记下「恢复的是哪一次熔断」(按熔断时间)。只用布尔值的话,横幅隐藏后组件仍挂着、
  // 状态不清,下一次熔断时按钮还是灰的,只能刷新页面才能恢复(审查 11-09)。
  const [resetFor, setResetFor] = useState<number | null>(null)
  const data = status.data
  if (!data?.tripped) {
    return null
  }
  const justReset = resetFor !== null && resetFor === data.tripped_at
  const onReset = () => {
    if (!window.confirm(t('poolBreaker.confirm'))) return
    const target = data.tripped_at
    reset.mutate(undefined, { onSuccess: () => setResetFor(target) })
  }
  return (
    <div
      role="alert"
      className="shrink-0 border-b border-rose-300 bg-rose-50 px-4 py-3 text-rose-900 dark:border-rose-500/40 dark:bg-rose-950/60 dark:text-rose-100 sm:px-6"
    >
      <div className="mx-auto flex w-full max-w-[1440px] flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
        <div className="min-w-0 space-y-1">
          <p className="flex items-center gap-2 text-sm font-semibold">
            <OctagonAlert className="h-4 w-4 shrink-0" />
            {t('poolBreaker.title', {
              time: data.tripped_at ? hhmm(data.tripped_at) : '--:--',
              n: data.events.length,
            })}
          </p>
          <p className="text-xs">{t('poolBreaker.desc')}</p>
          <ul className="flex flex-wrap gap-x-3 gap-y-1 text-xs">
            {data.events.map((e) => (
              <li key={`${e.account}-${e.at}`} className="break-all font-mono">
                {hhmm(e.at)} {e.account}({t(`poolBreaker.kind.${e.kind}`)})
              </li>
            ))}
          </ul>
          {data.persisted === false && <p className="text-xs">{t('poolBreaker.notPersisted')}</p>}
          {justReset && <p className="text-xs font-medium">{t('poolBreaker.resetPending')}</p>}
          {reset.isError && <p className="text-xs font-medium">{t('poolBreaker.resetFailed')}</p>}
        </div>
        <Button
          variant="destructive"
          size="sm"
          className="shrink-0 self-start"
          disabled={reset.isPending || justReset}
          onClick={onReset}
        >
          {t('poolBreaker.reset')}
        </Button>
      </div>
    </div>
  )
}
