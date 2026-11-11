import { Segment, type SegmentOption } from '@/components/ui/segment'
import { useI18n } from '@/lib/i18n'

const inputClass =
  'w-full rounded-2xl border bg-input px-4 py-2.5 text-sm outline-none transition-colors placeholder:text-muted-foreground'

interface KiroRoutingFieldsProps {
  /** '' = 默认（跟随全局，现状是新端点）；'q' = 老端点；其余为排查用的显式取值。 */
  endpoint: string
  /** 打开弹窗时号上的原值。排查用的取值（runtime / q-rest）按原值决定是否显示，切走了也还能切回来。 */
  originalEndpoint: string
  onEndpointChange: (value: string) => void
  /** FREE 号也派付费模型（opus 等）。 */
  allowPaidModels: boolean
  onAllowPaidModelsChange: (value: boolean) => void
  /** 中档优先目标负载的输入框文本；空串 = 关闭。 */
  preferredLoad: string
  onPreferredLoadChange: (value: string) => void
}

/**
 * Kiro 号专属的三个调度字段：上游端点、FREE 号付费模型开关、中档优先目标负载。
 *
 * 端点给「默认 / 老端点 / 新协议」三档；号上已经写了 `runtime` / `q-rest` 这类排查值时，
 * 把它作为第三个选项显示出来，不让弹窗把现值藏掉（否则用户以为是默认，一保存就被改写）。
 * 三个字段都是热改，约 30 秒内经 worker 同步生效，不用重启。
 */
export function KiroRoutingFields({
  endpoint,
  originalEndpoint,
  onEndpointChange,
  allowPaidModels,
  onAllowPaidModelsChange,
  preferredLoad,
  onPreferredLoadChange,
}: KiroRoutingFieldsProps) {
  const { t } = useI18n()

  const endpointOptions: SegmentOption<string>[] = [
    { value: '', label: t('accounts.kiroEndpoint.default') },
    { value: 'q', label: t('accounts.kiroEndpoint.q') },
    { value: 'responses', label: t('accounts.kiroEndpoint.responses') },
  ]
  if (originalEndpoint !== '' && originalEndpoint !== 'q' && originalEndpoint !== 'responses') {
    endpointOptions.push({ value: originalEndpoint, label: originalEndpoint })
  }

  return (
    <>
      <div className="space-y-1.5">
        <span className="text-xs font-medium text-muted-foreground">
          {t('accounts.field.kiroEndpoint')}
        </span>
        <div className="flex flex-wrap items-center gap-2">
          <Segment options={endpointOptions} value={endpoint} onChange={onEndpointChange} />
        </div>
        <p className="text-xs text-muted-foreground">{t('accounts.field.kiroEndpointHint')}</p>
      </div>

      <div className="space-y-1.5">
        <span className="text-xs font-medium text-muted-foreground">
          {t('accounts.field.allowPaidModels')}
        </span>
        <div className="flex flex-wrap items-center gap-2">
          <Segment
            options={[
              { value: 'off', label: t('accounts.queue.off') },
              { value: 'on', label: t('accounts.queue.on') },
            ]}
            value={allowPaidModels ? 'on' : 'off'}
            onChange={(v) => onAllowPaidModelsChange(v === 'on')}
          />
        </div>
        <p className="text-xs text-muted-foreground">{t('accounts.field.allowPaidModelsHint')}</p>
      </div>

      <div className="space-y-1.5">
        <label
          htmlFor="edit-account-preferred-load"
          className="text-xs font-medium text-muted-foreground"
        >
          {t('accounts.field.preferredLoad')}
        </label>
        {/* 用文本框 + decimal 键盘而不是 type=number:number 框遇到 "1,5" 这类非法输入
            会把值变成空串(= 关闭),还会因 step/max 拦住整张表单的保存。 */}
        <input
          id="edit-account-preferred-load"
          type="text"
          inputMode="decimal"
          autoComplete="off"
          value={preferredLoad}
          onChange={(event) => onPreferredLoadChange(event.target.value)}
          placeholder={t('accounts.field.preferredLoadPlaceholder')}
          className={inputClass}
        />
        <p className="text-xs text-muted-foreground">{t('accounts.field.preferredLoadHint')}</p>
      </div>
    </>
  )
}
