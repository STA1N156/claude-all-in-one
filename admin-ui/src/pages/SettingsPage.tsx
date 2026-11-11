import { useEffect, useRef, useState, type FormEvent } from 'react'
import { Info, Loader2 } from 'lucide-react'

import { Button } from '@/components/ui/button'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { ErrorNote } from '@/components/ui/error-note'
import { AutoModeCard } from '@/features/settings/components/AutoModeCard'
import {
  BatchGuardCard,
  batchGuardPatch,
  batchGuardToForm,
  type BatchGuardForm,
} from '@/features/settings/components/BatchGuardCard'
import { LocalCacheCard } from '@/features/settings/components/LocalCacheCard'
import {
  PoolBreakerCard,
  poolBreakerPatch,
  poolBreakerToForm,
  type PoolBreakerForm,
} from '@/features/settings/components/PoolBreakerCard'
import { EnvNormalizeCard } from '@/features/settings/components/EnvNormalizeCard'
import {
  buildEnvNormalizePatch,
  envNormalizeFormFromSettings,
  hasEnvNormalizeErrors,
  validateEnvNormalizeForm,
  type EnvNormalizeForm,
} from '@/features/settings/envNormalize'
import { SchedulerAlgorithmCard } from '@/features/settings/components/SchedulerAlgorithmCard'
import { WorkerEffectiveCard } from '@/features/settings/components/WorkerEffectiveCard'
import { useSettings, useUpdateSettings } from '@/features/settings/hooks'
import {
  buildSchedulerPatch,
  schedulerFormFromSettings,
  validateSchedulerForm,
  type SchedulerForm,
} from '@/features/settings/scheduler'
import { useGroups } from '@/features/groups/hooks'
import type {
  AutoModeClassifier,
  SystemSettings,
  SystemSettingsPatch,
} from '@/features/settings/types'
import { useI18n } from '@/lib/i18n'

const inputClass =
  'w-full rounded-2xl border bg-input px-4 py-2.5 text-sm text-foreground outline-none transition-colors placeholder:text-muted-foreground'

/** 一行分组暖机策略的表单态(数字以字符串存储,提交时解析)。 */
interface WarmupPolicyRow {
  group: string
  rpm: string
  hours: string
}

/** 表单内部状态：数字字段均以字符串存储，提交时解析。 */
interface FormState {
  default_proxy: string
  /** 出口池:文本框,每行一个代理 URL(提交时拆分成数组)。 */
  egress_pool: string
  rate_limit_cooldown_secs: string
  suspended_cooldown_secs: string
  empty_response_cooldown_secs: string
  empty_response_window_secs: string
  empty_response_threshold: string
  affinity_ttl_secs: string
  max_failures: string
  max_switch_attempts: string
  /** RPM 闸等待预算(毫秒)。 */
  rpm_wait_ms: string
  quota_poll_enabled: boolean
  /** 低优先新号暖机(全局两期,按分组策略未覆盖的组走这里)。 */
  warmup_enabled: boolean
  warmup_phase1_hours: string
  warmup_phase1_rpm: string
  warmup_phase2_hours: string
  warmup_phase2_rpm: string
  /** 按分组的暖机策略编辑行。 */
  warmup_policies: WarmupPolicyRow[]
  image_enabled: boolean
  image_max_long_edge: string
  image_max_pixels_single: string
  image_max_pixels_multi: string
  image_multi_threshold: string
  tools_in_prefix: boolean
  cache_point: boolean
  agent_continuation: boolean
  thinking_signature: boolean
  q_endpoint: boolean
  /** 历史 thinking 保留轮数(0=全丢/默认;N>0=最近 N 轮;负=全保留)。 */
  history_thinking_turns: string
  /** CC 自动模式安全分类器:off / haiku / allow。 */
  auto_mode_classifier: AutoModeClassifier
  auto_mode_classifier_model: string
  /** 保底只给总输入 ≥ 本值的请求(0 = 全部保底)。 */
  /** 批量任务拦截(表单形态见 BatchGuardCard)。 */
  batch_guard: BatchGuardForm
  /** 号池熔断(表单形态见 PoolBreakerCard)。 */
  pool_breaker: PoolBreakerForm
}

function settingsToForm(s: SystemSettings): FormState {
  return {
    default_proxy: s.default_proxy ?? '',
    egress_pool: (s.egress_pool ?? []).join('\n'),
    rate_limit_cooldown_secs: String(s.rate_limit_cooldown_secs),
    suspended_cooldown_secs: String(s.suspended_cooldown_secs),
    empty_response_cooldown_secs: String(s.empty_response_cooldown_secs),
    empty_response_window_secs: String(s.empty_response_window_secs),
    empty_response_threshold: String(s.empty_response_threshold),
    affinity_ttl_secs: String(s.affinity_ttl_secs),
    max_failures: String(s.max_failures),
    max_switch_attempts: String(s.max_switch_attempts),
    // 以下字段旧版本后端可能不返回,与 default_thinking_effort 同口径地补基线默认。
    rpm_wait_ms: String(s.rpm_wait_ms ?? 10000),
    quota_poll_enabled: s.quota_poll_enabled,
    warmup_enabled: s.warmup_enabled ?? true,
    warmup_phase1_hours: String(s.warmup_phase1_hours ?? 2),
    warmup_phase1_rpm: String(s.warmup_phase1_rpm ?? 2),
    warmup_phase2_hours: String(s.warmup_phase2_hours ?? 24),
    warmup_phase2_rpm: String(s.warmup_phase2_rpm ?? 6),
    warmup_policies: Object.entries(s.warmup_group_policies ?? {}).map(([group, p]) => ({
      group,
      rpm: String(p.rpm),
      hours: String(p.hours),
    })),
    image_enabled: s.image_enabled,
    image_max_long_edge: String(s.image_max_long_edge),
    image_max_pixels_single: String(s.image_max_pixels_single),
    image_max_pixels_multi: String(s.image_max_pixels_multi),
    image_multi_threshold: String(s.image_multi_threshold),
    tools_in_prefix: s.tools_in_prefix,
    cache_point: s.cache_point,
    agent_continuation: s.agent_continuation,
    thinking_signature: s.thinking_signature ?? true,
    q_endpoint: s.q_endpoint ?? false,
    // 同口径:旧版本后端不返回时按基线 0(全丢)显示。
    history_thinking_turns: String(s.history_thinking_turns ?? 0),
    // 旧版本后端不返回这三项 → 按后端基线默认显示(与 buildPatch 的比较口径一致)。
    auto_mode_classifier: s.auto_mode_classifier ?? 'off',
    auto_mode_classifier_model: s.auto_mode_classifier_model ?? 'claude-haiku-4-5',
    batch_guard: batchGuardToForm(s),
    pool_breaker: poolBreakerToForm(s),
  }
}

/** 只返回与服务器当前值不同的字段（增量 PATCH）。 */
function buildPatch(form: FormState, original: SystemSettings): SystemSettingsPatch {
  const patch: SystemSettingsPatch = {}

  // default_proxy: 空字符串 = 重置为 null
  const proxyValue = form.default_proxy.trim()
  const serverProxy = original.default_proxy ?? ''
  if (proxyValue !== serverProxy) {
    patch.default_proxy = proxyValue === '' ? null : proxyValue
  }

  // egress_pool: 文本框(每行一个 URL)→ 数组;空 = 重置为 null。
  // 注:GET 返回的池 URL 密码段被掩码(***),未改动时下面比较为 false → 不回传掩码值。
  const poolLines = form.egress_pool
    .split('\n')
    .map((l) => l.trim())
    .filter((l) => l !== '')
  const serverPool = original.egress_pool ?? []
  const poolChanged =
    poolLines.length !== serverPool.length || poolLines.some((l, i) => l !== serverPool[i])
  if (poolChanged) {
    patch.egress_pool = poolLines.length === 0 ? null : poolLines
  }

  // 整数字段
  const intFields: Array<[keyof FormState, keyof SystemSettings]> = [
    ['rate_limit_cooldown_secs', 'rate_limit_cooldown_secs'],
    ['suspended_cooldown_secs', 'suspended_cooldown_secs'],
    ['empty_response_cooldown_secs', 'empty_response_cooldown_secs'],
    ['empty_response_window_secs', 'empty_response_window_secs'],
    ['empty_response_threshold', 'empty_response_threshold'],
    ['affinity_ttl_secs', 'affinity_ttl_secs'],
    ['max_failures', 'max_failures'],
    ['max_switch_attempts', 'max_switch_attempts'],
    ['image_max_long_edge', 'image_max_long_edge'],
    ['image_max_pixels_single', 'image_max_pixels_single'],
    ['image_max_pixels_multi', 'image_max_pixels_multi'],
    ['image_multi_threshold', 'image_multi_threshold'],
  ]
  for (const [fk, sk] of intFields) {
    const parsed = parseInt(form[fk] as string, 10)
    if (!isNaN(parsed) && parsed !== (original[sk] as number)) {
      ;(patch as Record<string, unknown>)[sk] = parsed
    }
  }

  // 布尔字段
  const boolFields: Array<keyof SystemSettings & keyof FormState> = [
    'quota_poll_enabled',
    'image_enabled',
    'tools_in_prefix',
    'cache_point',
    'agent_continuation',
    'thinking_signature',
    'q_endpoint',
  ]
  for (const k of boolFields) {
    if (form[k] !== original[k]) {
      ;(patch as Record<string, unknown>)[k] = form[k]
    }
  }

  // 新增字段(旧版本后端可能不返回):与「基线默认」比较,避免对旧后端捎带未知字段
  // (与 default_thinking_effort 的 ?? 兜底比较同口径)。
  // 解析用 Number+isInteger 而非 parseInt:'1e3' / '2.5' 这类输入不该被静默截断
  // (对抗审核 [中]);非法值=不提交该字段(与既有 NaN 跳过口径一致)。
  const strictInt = (s: string): number | null => {
    const n = Number(s)
    return s.trim() !== '' && Number.isInteger(n) && n >= 0 ? n : null
  }
  const newIntFields: Array<[keyof FormState, keyof SystemSettings, number]> = [
    ['rpm_wait_ms', 'rpm_wait_ms', 10000],
    ['warmup_phase1_hours', 'warmup_phase1_hours', 2],
    ['warmup_phase1_rpm', 'warmup_phase1_rpm', 2],
    ['warmup_phase2_hours', 'warmup_phase2_hours', 24],
    ['warmup_phase2_rpm', 'warmup_phase2_rpm', 6],
  ]
  for (const [fk, sk, def] of newIntFields) {
    const parsed = strictInt(form[fk] as string)
    if (parsed !== null && parsed !== ((original[sk] as number | undefined) ?? def)) {
      ;(patch as Record<string, unknown>)[sk] = parsed
    }
  }
  if (form.warmup_enabled !== (original.warmup_enabled ?? true)) {
    patch.warmup_enabled = form.warmup_enabled
  }

  // 按分组暖机策略:编辑行 → map(空组名/非法数字行丢弃;同组名后行覆盖前行,
  // 但 UI 已在下拉里排除其他行已用的组名,正常路径到不了覆盖)。
  const policyMap: Record<string, { rpm: number; hours: number }> = {}
  for (const row of form.warmup_policies) {
    const group = row.group.trim()
    const rpm = strictInt(row.rpm)
    const hours = strictInt(row.hours)
    if (group === '' || rpm === null || hours === null) continue
    policyMap[group] = { rpm, hours }
  }
  const serverPolicies = original.warmup_group_policies ?? {}
  const samePolicies =
    Object.keys(policyMap).length === Object.keys(serverPolicies).length &&
    Object.entries(policyMap).every(([g, p]) => {
      const sp = serverPolicies[g]
      return sp !== undefined && sp.rpm === p.rpm && sp.hours === p.hours
    })
  if (!samePolicies) {
    // 空表发 **显式空 map** 而不是 null:null 的语义是「删 overlay 回 YAML 基线」,
    // 若 YAML 配了非空策略,删完全部行保存后策略会复活(对抗审核 [中])。
    patch.warmup_group_policies = policyMap
  }

  // 自动模式分类器三项:同一口径(与补过默认值的基线比,不跟原始响应比)。
  if (form.auto_mode_classifier !== (original.auto_mode_classifier ?? 'off')) {
    patch.auto_mode_classifier = form.auto_mode_classifier
  }
  {
    const model = form.auto_mode_classifier_model.trim()
    if (model !== '' && model !== (original.auto_mode_classifier_model ?? 'claude-haiku-4-5')) {
      patch.auto_mode_classifier_model = model
    }
  }

  // 批量任务拦截:只回传变了的键(比较口径与 batchGuardToForm 的默认值一致)。
  Object.assign(patch, batchGuardPatch(form.batch_guard, original))
  // 号池熔断:同口径。
  Object.assign(patch, poolBreakerPatch(form.pool_breaker, original))

  // history_thinking_turns:允许负数(-1=全部保留),不能用 strictInt(它拦负数)。
  // 解析与提交口径同上面的新增字段:非法输入不提交该字段;旧后端不返回时按基线 0 比较。
  const strictSignedInt = (s: string): number | null => {
    const n = Number(s)
    return s.trim() !== '' && Number.isInteger(n) ? n : null
  }
  const historyTurns = strictSignedInt(form.history_thinking_turns)
  if (historyTurns !== null && historyTurns !== (original.history_thinking_turns ?? 0)) {
    patch.history_thinking_turns = historyTurns
  }

  return patch
}

export default function SettingsPage() {
  const { t } = useI18n()
  const { data, isLoading, error: loadError } = useSettings()
  const { data: groups } = useGroups()
  const mutation = useUpdateSettings()

  const [form, setForm] = useState<FormState | null>(null)
  /** 调度器段(算法 + 参数)的表单态;字段元数据与增量 patch 逻辑在 features/settings/scheduler.ts。 */
  const [schedulerForm, setSchedulerForm] = useState<SchedulerForm | null>(null)
  /** 报文环境标准化段;表单逻辑在 features/settings/envNormalize.ts。 */
  const [envNormalizeForm, setEnvNormalizeForm] = useState<EnvNormalizeForm | null>(null)
  const [saved, setSaved] = useState(false)
  const [formError, setFormError] = useState<string | null>(null)
  const savedTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null)

  // 首次加载完成时初始化表单
  useEffect(() => {
    if (data !== undefined && form === null) {
      setForm(settingsToForm(data))
      setSchedulerForm(schedulerFormFromSettings(data))
      setEnvNormalizeForm(envNormalizeFormFromSettings(data))
    }
  }, [data, form])

  const set = (key: keyof FormState, value: string | boolean) => {
    setSaved(false)
    setForm((prev) => (prev === null ? prev : { ...prev, [key]: value }))
  }

  const setScheduler = <K extends keyof SchedulerForm>(key: K, value: SchedulerForm[K]) => {
    setSaved(false)
    setSchedulerForm((prev) => (prev === null ? prev : { ...prev, [key]: value }))
  }
  const schedulerErrors =
    schedulerForm !== null && data !== undefined ? validateSchedulerForm(schedulerForm, data) : {}
  const envNormalizeErrors = envNormalizeForm !== null ? validateEnvNormalizeForm(envNormalizeForm) : {}
  const changeEnvNormalize = (next: EnvNormalizeForm) => {
    setSaved(false)
    setEnvNormalizeForm(next)
  }

  // 分组暖机策略行的增删改(不走 set():它是数组不是标量)。
  const setPolicyRow = (idx: number, patch: Partial<WarmupPolicyRow>) => {
    setSaved(false)
    setForm((prev) =>
      prev === null
        ? prev
        : {
            ...prev,
            warmup_policies: prev.warmup_policies.map((r, i) =>
              i === idx ? { ...r, ...patch } : r,
            ),
          },
    )
  }
  const addPolicyRow = () => {
    setSaved(false)
    setForm((prev) =>
      prev === null
        ? prev
        : { ...prev, warmup_policies: [...prev.warmup_policies, { group: '', rpm: '2', hours: '24' }] },
    )
  }
  const removePolicyRow = (idx: number) => {
    setSaved(false)
    setForm((prev) =>
      prev === null
        ? prev
        : { ...prev, warmup_policies: prev.warmup_policies.filter((_, i) => i !== idx) },
    )
  }

  // 下拉选项 = 现有分组 ∪ 策略里已在用的组名(组被删后策略仍可辨认/移除)。
  const groupOptions = Array.from(
    new Set([...(groups ?? []).map((g) => g.name), ...(form?.warmup_policies ?? []).map((r) => r.group)]),
  ).filter((n) => n !== '')

  const handleSubmit = (event: FormEvent) => {
    event.preventDefault()
    if (!form || !schedulerForm || !envNormalizeForm || !data || mutation.isPending) return

    // 两期顺序守卫(对抗审核 [中]):phase2 < phase1 时 phase2 永远命不中
    // (号龄到 phase1 后直接毕业),存进去就是一份静默失效的配置。
    const p1 = Number(form.warmup_phase1_hours)
    const p2 = Number(form.warmup_phase2_hours)
    if (Number.isInteger(p1) && Number.isInteger(p2) && p2 < p1) {
      setFormError(t('settings.field.warmupPhaseOrderError'))
      return
    }
    // 调度器参数:后端 PUT 对越界值回 400 且整次保存作废,在前端先拦。
    if (Object.keys(schedulerErrors).length > 0) {
      setFormError(t('settings.schedulerAlgo.invalidSubmit'))
      return
    }
    if (hasEnvNormalizeErrors(envNormalizeErrors)) {
      setFormError(t('settings.envNormalize.invalidSubmit'))
      return
    }
    setFormError(null)

    const patch = {
      ...buildPatch(form, data),
      ...buildSchedulerPatch(schedulerForm, data),
      ...buildEnvNormalizePatch(envNormalizeForm, data),
    }
    if (Object.keys(patch).length === 0) {
      setSaved(true)
      return
    }

    mutation.mutate(patch, {
      onSuccess: (newData) => {
        setForm(settingsToForm(newData))
        setSchedulerForm(schedulerFormFromSettings(newData))
        setEnvNormalizeForm(envNormalizeFormFromSettings(newData))
        setSaved(true)
        if (savedTimerRef.current !== null) clearTimeout(savedTimerRef.current)
        savedTimerRef.current = setTimeout(() => setSaved(false), 3000)
      },
    })
  }

  return (
    <div className="space-y-6">
      {/* 页头 */}
      <header className="flex flex-wrap items-end justify-between gap-4">
        <div>
          <p className="eyebrow">Settings</p>
          <h1 className="mt-2 font-display text-4xl font-black tracking-[-0.04em]">{t('settings.title')}</h1>
          <p className="mt-2 text-sm text-muted-foreground">{t('settings.subtitle')}</p>
        </div>
      </header>

      {/* 静态提示：进程拓扑需重启 */}
      <div className="flex items-start gap-2 rounded-xl bg-muted/50 px-4 py-3 text-xs text-muted-foreground">
        <Info className="mt-0.5 h-3.5 w-3.5 shrink-0" />
        <span>{t('settings.restartNote')}</span>
      </div>

      {/* 加载失败 */}
      {loadError !== null && loadError !== undefined && <ErrorNote error={loadError} />}

      {/* 「我保存的到底生效没有」——放在改设置的同一页，不用切页也不用 SSH。 */}
      <WorkerEffectiveCard workers={data?.workers} desired={data} />


      <LocalCacheCard />

      <form onSubmit={handleSubmit} className="space-y-6">
        {/* ── 代理 ── */}
        <Card>
          <CardHeader>
            <CardTitle className="text-base">{t('settings.section.proxy')}</CardTitle>
          </CardHeader>
          <CardContent className="space-y-4">
            <div className="space-y-1.5">
              <label className="text-xs font-medium text-muted-foreground">
                {t('settings.field.defaultProxy')}
              </label>
              <input
                type="text"
                value={form?.default_proxy ?? ''}
                onChange={(e) => set('default_proxy', e.target.value)}
                placeholder={t('settings.field.defaultProxyPlaceholder')}
                spellCheck={false}
                autoComplete="off"
                disabled={isLoading || form === null}
                className={inputClass}
              />
            </div>
            <div className="space-y-1.5">
              <label className="text-xs font-medium text-muted-foreground">
                {t('settings.field.egressPool')}
              </label>
              <textarea
                value={form?.egress_pool ?? ''}
                onChange={(e) => set('egress_pool', e.target.value)}
                placeholder={t('settings.field.egressPoolPlaceholder')}
                spellCheck={false}
                autoComplete="off"
                rows={3}
                disabled={isLoading || form === null}
                className={`${inputClass} font-mono`}
              />
              <p className="text-xs text-muted-foreground">{t('settings.field.egressPoolHint')}</p>
            </div>
          </CardContent>
        </Card>

        {/* ── 报文环境标准化 ── */}
        <EnvNormalizeCard
          form={envNormalizeForm}
          errors={envNormalizeErrors}
          disabled={isLoading || envNormalizeForm === null}
          onChange={changeEnvNormalize}
        />

        {/* ── 自动模式安全分类器(省钱开关) ── */}
        <AutoModeCard
          mode={form?.auto_mode_classifier ?? 'off'}
          model={form?.auto_mode_classifier_model ?? ''}
          disabled={isLoading || form === null}
          onChange={(patch) => {
            setSaved(false)
            setForm((prev) => (prev === null ? prev : { ...prev, ...patch }))
          }}
        />

        {/* ── 批量任务拦截 ── */}
        <BatchGuardCard
          form={form?.batch_guard ?? null}
          disabled={isLoading || form === null}
          onChange={(next) => {
            setSaved(false)
            setForm((prev) => (prev === null ? prev : { ...prev, batch_guard: next }))
          }}
        />

        {/* ── 号池熔断 ── */}
        <PoolBreakerCard
          form={form?.pool_breaker ?? null}
          disabled={isLoading || form === null}
          onChange={(next) => {
            setSaved(false)
            setForm((prev) => (prev === null ? prev : { ...prev, pool_breaker: next }))
          }}
        />

        {/* ── 调度 ── */}
        <Card>
          <CardHeader>
            <CardTitle className="text-base">{t('settings.section.scheduler')}</CardTitle>
          </CardHeader>
          <CardContent className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
            <div className="space-y-1.5">
              <label className="text-xs font-medium text-muted-foreground">
                {t('settings.field.rateLimitCooldownSecs')}
              </label>
              <input
                type="number"
                step={1}
                min={0}
                value={form?.rate_limit_cooldown_secs ?? ''}
                onChange={(e) => set('rate_limit_cooldown_secs', e.target.value)}
                disabled={isLoading || form === null}
                className={inputClass}
              />
            </div>
            <div className="space-y-1.5">
              <label className="text-xs font-medium text-muted-foreground">
                {t('settings.field.suspendedCooldownSecs')}
              </label>
              <input
                type="number"
                step={1}
                min={0}
                value={form?.suspended_cooldown_secs ?? ''}
                onChange={(e) => set('suspended_cooldown_secs', e.target.value)}
                disabled={isLoading || form === null}
                className={inputClass}
              />
            </div>
            <div className="space-y-1.5">
              <label className="text-xs font-medium text-muted-foreground">
                {t('settings.field.emptyResponseCooldownSecs')}
              </label>
              <input
                type="number"
                step={1}
                min={0}
                value={form?.empty_response_cooldown_secs ?? ''}
                onChange={(e) => set('empty_response_cooldown_secs', e.target.value)}
                disabled={isLoading || form === null}
                className={inputClass}
              />
            </div>
            <div className="space-y-1.5">
              <label className="text-xs font-medium text-muted-foreground">
                {t('settings.field.emptyResponseWindowSecs')}
              </label>
              <input
                type="number"
                step={1}
                min={0}
                value={form?.empty_response_window_secs ?? ''}
                onChange={(e) => set('empty_response_window_secs', e.target.value)}
                disabled={isLoading || form === null}
                className={inputClass}
              />
            </div>
            <div className="space-y-1.5">
              <label className="text-xs font-medium text-muted-foreground">
                {t('settings.field.emptyResponseThreshold')}
              </label>
              <input
                type="number"
                step={1}
                min={0}
                value={form?.empty_response_threshold ?? ''}
                onChange={(e) => set('empty_response_threshold', e.target.value)}
                disabled={isLoading || form === null}
                className={inputClass}
              />
            </div>
            <div className="space-y-1.5">
              <label className="text-xs font-medium text-muted-foreground">
                {t('settings.field.affinityTtlSecs')}
              </label>
              <input
                type="number"
                step={1}
                min={0}
                value={form?.affinity_ttl_secs ?? ''}
                onChange={(e) => set('affinity_ttl_secs', e.target.value)}
                disabled={isLoading || form === null}
                className={inputClass}
              />
            </div>
            <div className="space-y-1.5">
              <label className="text-xs font-medium text-muted-foreground">
                {t('settings.field.maxFailures')}
              </label>
              <input
                type="number"
                step={1}
                min={0}
                value={form?.max_failures ?? ''}
                onChange={(e) => set('max_failures', e.target.value)}
                disabled={isLoading || form === null}
                className={inputClass}
              />
            </div>
            <div className="space-y-1.5">
              <label className="text-xs font-medium text-muted-foreground">
                {t('settings.field.maxSwitchAttempts')}
              </label>
              <input
                type="number"
                step={1}
                min={1}
                value={form?.max_switch_attempts ?? ''}
                onChange={(e) => set('max_switch_attempts', e.target.value)}
                disabled={isLoading || form === null}
                className={inputClass}
              />
              <p className="text-xs text-muted-foreground">
                {t('settings.field.maxSwitchAttemptsHint')}
              </p>
            </div>
            <div className="space-y-1.5">
              <label className="text-xs font-medium text-muted-foreground">
                {t('settings.field.rpmWaitMs')}
              </label>
              <input
                type="number"
                step={1}
                min={0}
                value={form?.rpm_wait_ms ?? ''}
                onChange={(e) => set('rpm_wait_ms', e.target.value)}
                disabled={isLoading || form === null}
                className={inputClass}
              />
              <p className="text-xs text-muted-foreground">{t('settings.field.rpmWaitMsHint')}</p>
            </div>
            <label className="flex cursor-pointer items-center gap-3 sm:col-span-2 lg:col-span-3">
              <input
                type="checkbox"
                checked={form?.quota_poll_enabled ?? false}
                onChange={(e) => set('quota_poll_enabled', e.target.checked)}
                disabled={isLoading || form === null}
                className="h-4 w-4 rounded"
              />
              <span className="text-sm font-medium">{t('settings.field.quotaPollEnabled')}</span>
            </label>
          </CardContent>
        </Card>

        {/* ── 调度器(算法 + 参数) ── */}
        <SchedulerAlgorithmCard
          form={schedulerForm}
          errors={schedulerErrors}
          disabled={isLoading || schedulerForm === null}
          onChange={setScheduler}
        />

        {/* ── 新号暖机 ── */}
        <Card>
          <CardHeader>
            <CardTitle className="text-base">{t('settings.section.warmup')}</CardTitle>
          </CardHeader>
          <CardContent className="space-y-4">
            <label className="flex cursor-pointer items-start gap-3">
              <input
                type="checkbox"
                checked={form?.warmup_enabled ?? true}
                onChange={(e) => set('warmup_enabled', e.target.checked)}
                disabled={isLoading || form === null}
                className="mt-0.5 h-4 w-4 rounded"
              />
              <span>
                <span className="block text-sm font-medium">{t('settings.field.warmupEnabled')}</span>
                <span className="block text-xs text-muted-foreground">
                  {t('settings.field.warmupEnabledHint')}
                </span>
              </span>
            </label>

            <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
              <div className="space-y-1.5">
                <label className="text-xs font-medium text-muted-foreground">
                  {t('settings.field.warmupPhase1Hours')}
                </label>
                <input
                  type="number"
                  step={1}
                  min={0}
                  value={form?.warmup_phase1_hours ?? ''}
                  onChange={(e) => set('warmup_phase1_hours', e.target.value)}
                  disabled={isLoading || form === null}
                  className={inputClass}
                />
              </div>
              <div className="space-y-1.5">
                <label className="text-xs font-medium text-muted-foreground">
                  {t('settings.field.warmupPhase1Rpm')}
                </label>
                <input
                  type="number"
                  step={1}
                  min={1}
                  value={form?.warmup_phase1_rpm ?? ''}
                  onChange={(e) => set('warmup_phase1_rpm', e.target.value)}
                  disabled={isLoading || form === null}
                  className={inputClass}
                />
              </div>
              <div className="space-y-1.5">
                <label className="text-xs font-medium text-muted-foreground">
                  {t('settings.field.warmupPhase2Hours')}
                </label>
                <input
                  type="number"
                  step={1}
                  min={0}
                  value={form?.warmup_phase2_hours ?? ''}
                  onChange={(e) => set('warmup_phase2_hours', e.target.value)}
                  disabled={isLoading || form === null}
                  className={inputClass}
                />
              </div>
              <div className="space-y-1.5">
                <label className="text-xs font-medium text-muted-foreground">
                  {t('settings.field.warmupPhase2Rpm')}
                </label>
                <input
                  type="number"
                  step={1}
                  min={1}
                  value={form?.warmup_phase2_rpm ?? ''}
                  onChange={(e) => set('warmup_phase2_rpm', e.target.value)}
                  disabled={isLoading || form === null}
                  className={inputClass}
                />
              </div>
            </div>

            {/* 按分组策略:命中的组完全接管(单期);hours=0 = 该组关闭暖机 */}
            <div className="space-y-2">
              <span className="block text-sm font-medium">{t('settings.field.warmupPolicies')}</span>
              <p className="text-xs text-muted-foreground">
                {t('settings.field.warmupPoliciesHint')}
              </p>
              {(form?.warmup_policies ?? []).map((row, idx) => (
                <div key={idx} className="flex flex-wrap items-center gap-2">
                  <select
                    value={row.group}
                    onChange={(e) => setPolicyRow(idx, { group: e.target.value })}
                    disabled={isLoading || form === null}
                    className={`${inputClass} w-44`}
                  >
                    <option value="" disabled>
                      {t('settings.field.warmupPolicyGroupPlaceholder')}
                    </option>
                    {groupOptions
                      .filter(
                        (g) =>
                          g === row.group ||
                          !(form?.warmup_policies ?? []).some((r, i) => i !== idx && r.group === g),
                      )
                      .map((g) => (
                        <option key={g} value={g}>
                          {g}
                        </option>
                      ))}
                  </select>
                  <input
                    type="number"
                    step={1}
                    min={1}
                    value={row.rpm}
                    onChange={(e) => setPolicyRow(idx, { rpm: e.target.value })}
                    disabled={isLoading || form === null}
                    placeholder={t('settings.field.warmupPolicyRpmPlaceholder')}
                    className={`${inputClass} w-28`}
                  />
                  <input
                    type="number"
                    step={1}
                    min={0}
                    value={row.hours}
                    onChange={(e) => setPolicyRow(idx, { hours: e.target.value })}
                    disabled={isLoading || form === null}
                    placeholder={t('settings.field.warmupPolicyHoursPlaceholder')}
                    className={`${inputClass} w-32`}
                  />
                  <Button
                    type="button"
                    variant="ghost"
                    size="sm"
                    onClick={() => removePolicyRow(idx)}
                    disabled={isLoading || form === null}
                  >
                    {t('settings.field.warmupPolicyRemove')}
                  </Button>
                </div>
              ))}
              <Button
                type="button"
                variant="outline"
                size="sm"
                onClick={addPolicyRow}
                disabled={isLoading || form === null}
              >
                {t('settings.field.warmupPolicyAdd')}
              </Button>
            </div>
          </CardContent>
        </Card>

        {/* ── 图像压缩 ── */}
        <Card>
          <CardHeader>
            <CardTitle className="text-base">{t('settings.section.image')}</CardTitle>
          </CardHeader>
          <CardContent className="space-y-4">
            {/* 启用开关 */}
            <label className="flex cursor-pointer items-center gap-3">
              <input
                type="checkbox"
                checked={form?.image_enabled ?? false}
                onChange={(e) => set('image_enabled', e.target.checked)}
                disabled={isLoading || form === null}
                className="h-4 w-4 rounded"
              />
              <span className="text-sm font-medium">{t('settings.field.imageEnabled')}</span>
            </label>

            <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
              <div className="space-y-1.5">
                <label className="text-xs font-medium text-muted-foreground">
                  {t('settings.field.imageMaxLongEdge')}
                </label>
                <input
                  type="number"
                  step={1}
                  min={0}
                  value={form?.image_max_long_edge ?? ''}
                  onChange={(e) => set('image_max_long_edge', e.target.value)}
                  disabled={isLoading || form === null}
                  className={inputClass}
                />
              </div>
              <div className="space-y-1.5">
                <label className="text-xs font-medium text-muted-foreground">
                  {t('settings.field.imageMaxPixelsSingle')}
                </label>
                <input
                  type="number"
                  step={1}
                  min={0}
                  value={form?.image_max_pixels_single ?? ''}
                  onChange={(e) => set('image_max_pixels_single', e.target.value)}
                  disabled={isLoading || form === null}
                  className={inputClass}
                />
              </div>
              <div className="space-y-1.5">
                <label className="text-xs font-medium text-muted-foreground">
                  {t('settings.field.imageMaxPixelsMulti')}
                </label>
                <input
                  type="number"
                  step={1}
                  min={0}
                  value={form?.image_max_pixels_multi ?? ''}
                  onChange={(e) => set('image_max_pixels_multi', e.target.value)}
                  disabled={isLoading || form === null}
                  className={inputClass}
                />
              </div>
              <div className="space-y-1.5">
                <label className="text-xs font-medium text-muted-foreground">
                  {t('settings.field.imageMultiThreshold')}
                </label>
                <input
                  type="number"
                  step={1}
                  min={0}
                  value={form?.image_multi_threshold ?? ''}
                  onChange={(e) => set('image_multi_threshold', e.target.value)}
                  disabled={isLoading || form === null}
                  className={inputClass}
                />
              </div>
            </div>
          </CardContent>
        </Card>

        {/* ── 思维链 ── */}
        <Card>
          <CardHeader>
            <CardTitle className="text-base">{t('settings.section.thinking')}</CardTitle>
          </CardHeader>
          <CardContent className="space-y-3">
            <div className="space-y-1.5">
              <label className="text-xs font-medium text-muted-foreground">
                {t('settings.field.historyThinkingTurns')}
              </label>
              <input
                type="number"
                step={1}
                value={form?.history_thinking_turns ?? ''}
                onChange={(e) => set('history_thinking_turns', e.target.value)}
                disabled={isLoading || form === null}
                className={inputClass}
              />
              <p className="text-xs text-muted-foreground">
                {t('settings.field.historyThinkingTurnsHint')}
              </p>
            </div>
          </CardContent>
        </Card>

        {/* ── 实验性 ── */}
        <Card>
          <CardHeader>
            <CardTitle className="text-base">{t('settings.section.experimental')}</CardTitle>
          </CardHeader>
          <CardContent className="space-y-4">
            <label className="flex cursor-pointer items-start gap-3">
              <input
                type="checkbox"
                checked={form?.tools_in_prefix ?? false}
                onChange={(e) => set('tools_in_prefix', e.target.checked)}
                disabled={isLoading || form === null}
                className="mt-0.5 h-4 w-4 rounded"
              />
              <span>
                <span className="block text-sm font-medium">
                  {t('settings.field.toolsInPrefix')}
                </span>
                <span className="block text-xs text-muted-foreground">
                  {t('settings.field.toolsInPrefixHint')}
                </span>
              </span>
            </label>
            <label className="flex cursor-pointer items-start gap-3">
              <input
                type="checkbox"
                checked={form?.cache_point ?? false}
                onChange={(e) => set('cache_point', e.target.checked)}
                disabled={isLoading || form === null}
                className="mt-0.5 h-4 w-4 rounded"
              />
              <span>
                <span className="block text-sm font-medium">
                  {t('settings.field.cachePoint')}
                </span>
                <span className="block text-xs text-muted-foreground">
                  {t('settings.field.cachePointHint')}
                </span>
              </span>
            </label>
            <label className="flex cursor-pointer items-start gap-3">
              <input
                type="checkbox"
                checked={form?.agent_continuation ?? false}
                onChange={(e) => set('agent_continuation', e.target.checked)}
                disabled={isLoading || form === null}
                className="mt-0.5 h-4 w-4 rounded"
              />
              <span>
                <span className="block text-sm font-medium">
                  {t('settings.field.agentContinuation')}
                </span>
                <span className="block text-xs text-muted-foreground">
                  {t('settings.field.agentContinuationHint')}
                </span>
              </span>
            </label>
            <label className="flex cursor-pointer items-start gap-3">
              <input
                type="checkbox"
                checked={form?.thinking_signature ?? true}
                onChange={(e) => set('thinking_signature', e.target.checked)}
                disabled={isLoading || form === null}
                className="mt-0.5 h-4 w-4 rounded"
              />
              <span>
                <span className="block text-sm font-medium">
                  {t('settings.field.thinkingSignature')}
                </span>
                <span className="block text-xs text-muted-foreground">
                  {t('settings.field.thinkingSignatureHint')}
                </span>
              </span>
            </label>
            <label className="flex cursor-pointer items-start gap-3">
              <input
                type="checkbox"
                checked={form?.q_endpoint ?? false}
                onChange={(e) => set('q_endpoint', e.target.checked)}
                disabled={isLoading || form === null}
                className="mt-0.5 h-4 w-4 rounded"
              />
              <span>
                <span className="block text-sm font-medium">
                  {t('settings.field.qEndpoint')}
                </span>
                <span className="block text-xs text-muted-foreground">
                  {t('settings.field.qEndpointHint')}
                </span>
              </span>
            </label>
          </CardContent>
        </Card>

        {/* 保存区 */}
        {mutation.isError && (
          <ErrorNote error={mutation.error} labelKey="common.actionFailed" />
        )}
        {formError !== null && (
          <p className="text-sm text-rose-600 dark:text-rose-300">{formError}</p>
        )}

        <div className="flex items-center justify-end gap-3">
          {saved && (
            <span className="text-sm text-emerald-700 dark:text-emerald-300">{t('settings.saved')}</span>
          )}
          <Button type="submit" disabled={isLoading || form === null || mutation.isPending}>
            {mutation.isPending && <Loader2 className="h-4 w-4 animate-spin" />}
            {mutation.isPending ? t('settings.saving') : t('settings.save')}
          </Button>
        </div>
      </form>
    </div>
  )
}
