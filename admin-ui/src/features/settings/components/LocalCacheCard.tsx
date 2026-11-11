import { useEffect, useState, type FormEvent } from 'react'

import { Button } from '@/components/ui/button'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { ErrorNote } from '@/components/ui/error-note'
import { useI18n } from '@/lib/i18n'
import { useSettings, useUpdateSettings } from '../hooks'
import type { SystemSettings } from '../types'

const inputClass = 'w-full rounded-2xl border bg-input px-4 py-2.5 text-sm outline-none'
const fromSettings = (s: SystemSettings) => ({
  enabled: s.cache_enabled ?? true,
  minutes: String(Math.ceil((s.cache_sim_ttl_secs ?? 300) / 60)),
  percent: String(Number(((s.cache_cap_ratio ?? 0.9) * 100).toFixed(6))),
})

export function LocalCacheCard() {
  const { t } = useI18n()
  const { data } = useSettings()
  const mutation = useUpdateSettings()
  const [form, setForm] = useState<ReturnType<typeof fromSettings> | null>(null)
  const [saved, setSaved] = useState(false)
  const [invalid, setInvalid] = useState(false)

  useEffect(() => {
    if (data && form === null) setForm(fromSettings(data))
  }, [data, form])

  const set = (patch: Partial<NonNullable<typeof form>>) => {
    setSaved(false)
    setInvalid(false)
    setForm((prev) => prev && { ...prev, ...patch })
  }
  const save = (event: FormEvent) => {
    event.preventDefault()
    if (!form || mutation.isPending) return
    const minutes = Number(form.minutes)
    const percent = Number(form.percent)
    if (!Number.isInteger(minutes) || minutes < 1 || minutes > 10080
      || form.percent.trim() === '' || !Number.isFinite(percent) || percent < 0 || percent > 100) {
      setInvalid(true)
      return
    }
    mutation.mutate({
      cache_enabled: form.enabled,
      cache_sim_ttl_secs: minutes * 60,
      cache_cap_ratio: percent / 100,
      cache_strategy: 'sim',
      cache_read_multiplier: 1,
      cache_floor_ratio: 0,
      cache_floor_min_input: 0,
      cache_io_correction: false,
    }, { onSuccess: (settings) => { setForm(fromSettings(settings)); setSaved(true) } })
  }
  const disabled = form === null || mutation.isPending

  return (
    <Card>
      <CardHeader><CardTitle className="text-base">{t('settings.localCache.title')}</CardTitle></CardHeader>
      <CardContent>
        <form onSubmit={save} className="space-y-5">
          <div className="space-y-2">
            <label className="flex items-center gap-3 text-sm font-medium">
              <button
                id="local-cache-enabled" type="button" role="switch"
                aria-checked={form?.enabled ?? true} aria-label={t('settings.localCache.enabled')}
                disabled={disabled} onClick={() => set({ enabled: !form?.enabled })}
                className={`relative h-6 w-11 shrink-0 rounded-full transition-colors disabled:opacity-50 ${form?.enabled !== false ? 'bg-primary' : 'bg-muted-foreground/30'}`}
              >
                <span className={`absolute top-0.5 h-5 w-5 rounded-full bg-background transition-transform ${form?.enabled !== false ? 'left-0.5 translate-x-5' : 'left-0.5'}`} />
              </button>
              {t('settings.localCache.enabled')}
            </label>
            <p className="text-xs leading-relaxed text-muted-foreground">{t('settings.localCache.hint')}</p>
          </div>
          <div className="space-y-2">
            <label htmlFor="local-cache-minutes" className="text-sm font-medium">{t('settings.localCache.minutes')}</label>
            <input id="local-cache-minutes" type="number" min={1} max={10080} step={1} required
              value={form?.minutes ?? ''} onChange={(e) => set({ minutes: e.target.value })}
              disabled={disabled} className={inputClass} />
            <p className="text-xs text-muted-foreground">{t('settings.localCache.minutesHint')}</p>
          </div>
          <div className="space-y-2">
            <label htmlFor="local-cache-percent" className="text-sm font-medium">{t('settings.localCache.percent')}</label>
            <input id="local-cache-percent" type="number" min={0} max={100} step="any" required
              value={form?.percent ?? ''} onChange={(e) => set({ percent: e.target.value })}
              disabled={disabled} className={inputClass} />
            <p className="text-xs text-muted-foreground">{t('settings.localCache.percentHint')}</p>
          </div>
          {invalid && <p role="alert" className="text-sm text-destructive">{t('settings.localCache.invalid')}</p>}
          {mutation.error && <ErrorNote error={mutation.error} />}
          <div className="flex items-center gap-3">
            <Button type="submit" disabled={disabled}>{t('settings.localCache.save')}</Button>
            {saved && <span role="status" className="text-sm text-muted-foreground">{t('settings.localCache.saved')}</span>}
          </div>
        </form>
      </CardContent>
    </Card>
  )
}
