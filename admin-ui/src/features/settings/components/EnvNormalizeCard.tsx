import { useId } from 'react'

import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { useI18n } from '@/lib/i18n'
import { cn } from '@/lib/utils'

import {
  ENV_NORMALIZE_FAMILIES,
  toggleFamily,
  type EnvNormalizeErrors,
  type EnvNormalizeForm,
} from '../envNormalize'

const inputClass =
  'w-full rounded-2xl border bg-input px-4 py-2.5 text-sm text-foreground outline-none transition-colors placeholder:text-muted-foreground'

/**
 * 系统设置里的「报文环境标准化」段：把客户端报文里的系统版本、时区、locale、Git 用户名
 * 换成按号固定的标准人设（gw_core::envnorm）。受控组件，随主表单一起保存。
 */
export function EnvNormalizeCard({
  form,
  errors,
  disabled,
  onChange,
}: {
  form: EnvNormalizeForm | null
  errors: EnvNormalizeErrors
  disabled: boolean
  onChange: (next: EnvNormalizeForm) => void
}) {
  const { t } = useI18n()
  const idBase = useId()
  const update = (patch: Partial<EnvNormalizeForm>) => {
    if (form !== null && !disabled) onChange({ ...form, ...patch })
  }
  const tzId = `${idBase}-tz`
  const localeId = `${idBase}-locale`

  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">{t('settings.section.envNormalize')}</CardTitle>
      </CardHeader>
      <CardContent className="space-y-5">
        <p className="text-xs text-muted-foreground">{t('settings.envNormalize.desc')}</p>
        <div className="space-y-2">
          <span className="block text-sm font-medium">{t('settings.envNormalize.providers')}</span>
          <div className="flex flex-wrap gap-x-6 gap-y-2">
            {ENV_NORMALIZE_FAMILIES.map((f) => (
              <label key={f} className="flex cursor-pointer items-center gap-2 text-sm">
                <input
                  type="checkbox"
                  checked={form?.providers.includes(f) ?? false}
                  onChange={(e) => form !== null && !disabled && onChange(toggleFamily(form, f, e.target.checked))}
                  disabled={disabled}
                  className="h-4 w-4 shrink-0 rounded"
                />
                {t(`settings.envNormalize.family.${f}`)}
              </label>
            ))}
          </div>
          <p className="text-xs text-amber-600 dark:text-amber-300">{t('settings.envNormalize.cacheNote')}</p>
        </div>
        <div className="grid gap-4 sm:grid-cols-2">
          <div className="space-y-1.5">
            <label htmlFor={tzId} className="text-xs font-medium text-muted-foreground">
              {t('settings.envNormalize.timezone')}
            </label>
            <input
              id={tzId}
              value={form?.timezone ?? ''}
              placeholder="Asia/Singapore"
              onChange={(e) => update({ timezone: e.target.value })}
              disabled={disabled}
              aria-invalid={errors.timezone === true}
              className={cn(inputClass, errors.timezone && '!border-rose-500 dark:!border-rose-400')}
            />
            <p className={cn('text-xs', errors.timezone ? 'text-rose-600 dark:text-rose-300' : 'text-muted-foreground')}>
              {t(errors.timezone ? 'settings.envNormalize.error.timezone' : 'settings.envNormalize.timezoneHint')}
            </p>
          </div>
          <div className="space-y-1.5">
            <label htmlFor={localeId} className="text-xs font-medium text-muted-foreground">
              {t('settings.envNormalize.locale')}
            </label>
            <input
              id={localeId}
              value={form?.locale ?? ''}
              placeholder="en-US"
              onChange={(e) => update({ locale: e.target.value })}
              disabled={disabled}
              aria-invalid={errors.locale === true}
              className={cn(inputClass, errors.locale && '!border-rose-500 dark:!border-rose-400')}
            />
            <p className={cn('text-xs', errors.locale ? 'text-rose-600 dark:text-rose-300' : 'text-muted-foreground')}>
              {t(errors.locale ? 'settings.envNormalize.error.locale' : 'settings.envNormalize.localeHint')}
            </p>
          </div>
        </div>
      </CardContent>
    </Card>
  )
}
