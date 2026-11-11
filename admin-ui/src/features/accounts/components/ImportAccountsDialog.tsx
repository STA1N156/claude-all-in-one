import { useEffect, useRef, useState, type ChangeEvent, type FormEvent } from 'react'
import { useQueryClient } from '@tanstack/react-query'
import {
  AlertCircle,
  CheckCircle2,
  Clock,
  Info,
  Loader2,
  ShieldCheck,
  Trash2,
  Upload,
  XCircle,
} from 'lucide-react'

import { Button } from '@/components/ui/button'
import { Modal } from '@/components/ui/modal'
import { Select } from '@/components/ui/select'
import { useGroups } from '@/features/groups/hooks'
import { useSettings } from '@/features/settings/hooks'
import { extractErrorMessage, getErrorStatus } from '@/lib/api'
import { useI18n } from '@/lib/i18n'
import { queryKeys } from '@/lib/query-keys'

import { deleteAccount, fetchAccountQuotaNow } from '../api'
import { useImportAccounts, useImportApiKeys } from '../hooks'
import { formatCredits } from '../lib'
import type { AccountQuota, ImportAccountsResult } from '../types'

const inputClass =
  'w-full rounded-2xl border bg-input px-4 py-2.5 text-sm outline-none transition-colors placeholder:text-muted-foreground'

/** 导入后 worker 未同步(404)时的重试节奏:admin 已主动捅 /sync,通常首试即中;
 * 这里兜底覆盖 worker 繁忙/降级场景,上限 ≈ 一个 30s 周期。 */
const SYNC_RETRY_DELAY_MS = 4_000
const SYNC_MAX_ATTEMPTS = 8

/** 单账号验活状态机(对齐 kiro.rs batch-import-dialog 的逐条状态)。 */
type VerifyState =
  | { phase: 'pending' }
  | { phase: 'waiting' } // worker 尚未同步到该账号,重试中
  | { phase: 'verifying' }
  | { phase: 'ok'; quota: AccountQuota }
  | { phase: 'noQuota' } // 可刷新但上游无配额数据
  | { phase: 'failed'; error: string }

const TERMINAL_PHASES = new Set(['ok', 'noQuota', 'failed'])

/** 本次真写过库的动作 —— 只有这些号值得验活(duplicate/skipped 什么都没改)。 */
const WRITTEN_ACTIONS = new Set(['created', 'merged'])

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms))
}

/** 把网关代理 URL 显示成「序号. host:port」(密码段已被后端掩码,host 仍可识别)。 */
function gatewayLabel(url: string, i: number): string {
  let host = url
  try {
    host = new URL(url).host || url
  } catch {
    host = url
  }
  return `${i + 1}. ${host}`
}

interface ImportAccountsDialogProps {
  open: boolean
  onClose: () => void
}

/** 导入账号导出 JSON(完整导入 + 智能合并)。
 *
 * 导入提交后**立刻出结果**:新增 / 库里已有 / 没收下三个数醒目摊开。
 * 验活(逐号刷 token + 查配额)**不再自动跑** —— 它是串行只读探测,每个号
 * 1~5 秒、worker 未同步还要 4s×8 重试,号多就是分钟级,把人卡在对话框里
 * (2026-10-16 用户反馈:「号越来越多,太慢了」)。改成结果页上的一个按钮,
 * 想看再点;不点也不影响:worker 后台预热照样会回填档位/用量并标死号。 */
export function ImportAccountsDialog({ open, onClose }: ImportAccountsDialogProps) {
  const { t } = useI18n()
  const groupsQuery = useGroups()
  const settingsQuery = useSettings()
  const gateways = settingsQuery.data?.egress_pool ?? []
  const mutation = useImportAccounts()
  const apiKeysMutation = useImportApiKeys()
  const queryClient = useQueryClient()
  const fileInputRef = useRef<HTMLInputElement>(null)
  // 运行序号(generation token):开/关对话框都自增,使仍在 await 中的旧验活/删除
  // 循环失效——不能用布尔 cancelled(重开会复位,4s 睡眠里的旧循环会"复活"写进新会话)。
  const runSeqRef = useRef(0)

  // 导入模式:'json' = KiroManager 导出 JSON;'apikeys' = 粘贴官方 API Key(ksk_)列表。
  const [mode, setMode] = useState<'json' | 'apikeys'>('json')
  const [group, setGroup] = useState('')
  const [json, setJson] = useState('')
  const [keys, setKeys] = useState('')
  const [egress, setEgress] = useState('auto')
  const [error, setError] = useState<string | null>(null)
  const [result, setResult] = useState<ImportAccountsResult | null>(null)

  const [verifyStates, setVerifyStates] = useState<Record<string, VerifyState>>({})
  const [verifying, setVerifying] = useState(false)
  /** 用户点过「验活」才展示进度条与逐条徽标(默认不跑)。 */
  const [verifyStarted, setVerifyStarted] = useState(false)
  const [removing, setRemoving] = useState(false)
  const [removedIds, setRemovedIds] = useState<Set<string>>(new Set())

  useEffect(() => {
    runSeqRef.current += 1
    if (open) {
      setMode('json')
      setGroup('')
      setJson('')
      setKeys('')
      setEgress('auto')
      setError(null)
      setResult(null)
      setVerifyStates({})
      setVerifying(false)
      setVerifyStarted(false)
      setRemoving(false)
      setRemovedIds(new Set())
    }
  }, [open])

  const handleFile = (event: ChangeEvent<HTMLInputElement>) => {
    const file = event.target.files?.[0]
    if (!file) return
    const reader = new FileReader()
    reader.onload = () => setJson(typeof reader.result === 'string' ? reader.result : '')
    reader.readAsText(file)
  }

  const invalidateAccountDomains = () => {
    void queryClient.invalidateQueries({ queryKey: queryKeys.accounts.root })
    void queryClient.invalidateQueries({ queryKey: queryKeys.groups.root })
  }

  /** 逐账号串行验活(只对本次写过的号);404 = worker 未同步 → 限次重试。
   * 只在用户点按钮时跑。验活是只读探测:关闭对话框即取消(运行序号失配)。 */
  const runVerification = async (items: ImportAccountsResult['items']) => {
    const run = runSeqRef.current
    const stale = () => runSeqRef.current !== run
    const targets = items.filter((i) => WRITTEN_ACTIONS.has(i.action))
    if (targets.length === 0) return
    setVerifying(true)
    setVerifyStates(
      Object.fromEntries(
        targets.map((i): [string, VerifyState] => [i.account_id, { phase: 'pending' }]),
      ),
    )
    try {
      for (const item of targets) {
        if (stale()) return
        const id = item.account_id
        setVerifyStates((prev) => ({ ...prev, [id]: { phase: 'verifying' } }))
        let state: VerifyState | null = null
        for (let attempt = 1; attempt <= SYNC_MAX_ATTEMPTS; attempt++) {
          try {
            const res = await fetchAccountQuotaNow(id)
            state =
              res.verified && res.quota
                ? { phase: 'ok', quota: res.quota }
                : { phase: 'noQuota' }
            break
          } catch (err) {
            if (getErrorStatus(err) === 404 && attempt < SYNC_MAX_ATTEMPTS) {
              // worker 尚未同步到该账号:等待后重试。
              if (stale()) return
              setVerifyStates((prev) => ({ ...prev, [id]: { phase: 'waiting' } }))
              await sleep(SYNC_RETRY_DELAY_MS)
              continue
            }
            state =
              getErrorStatus(err) === 404
                ? { phase: 'failed', error: t('accounts.import.waitingSyncTimeout') }
                : { phase: 'failed', error: extractErrorMessage(err) }
            break
          }
        }
        if (stale()) return
        setVerifyStates((prev) => ({ ...prev, [id]: state ?? { phase: 'noQuota' } }))
      }
    } finally {
      // 取消/完成都收尾:验活已更新配额缓存、可能标了死号禁用 → 刷新表格。
      // setVerifying 只在本运行仍有效时写,避免覆盖重开后新会话的状态。
      if (!stale()) setVerifying(false)
      invalidateAccountDomains()
    }
  }

  const submitting = mutation.isPending || apiKeysMutation.isPending

  const handleSubmit = (event: FormEvent) => {
    event.preventDefault()
    if (submitting) return
    // 立刻出结果,不自动验活(见文件头注释)。导入本身已经刷新了账号表格。
    const onSuccess = (data: ImportAccountsResult) => {
      setResult(data)
      invalidateAccountDomains()
    }
    const onError = (err: unknown) => setError(extractErrorMessage(err))

    if (mode === 'apikeys') {
      if (keys.trim() === '') {
        setError(t('accounts.import.keysEmpty'))
        return
      }
      setError(null)
      apiKeysMutation.mutate(
        { keys, group_name: group || undefined, egress },
        { onSuccess, onError },
      )
      return
    }

    if (json.trim() === '') {
      setError(t('accounts.import.empty'))
      return
    }
    setError(null)
    mutation.mutate(
      { json, group_name: group || undefined, egress },
      { onSuccess, onError },
    )
  }

  const conflictCount =
    result?.items.filter((i) => i.machine_id_conflict).length ?? 0

  // 三个醒目的数。老版本把「库里已有、无需更新」也算进 skipped,和「非法 ID /
  // 派生耗尽」混在一起,每天重复上号的人分不出哪次是真出问题了。
  //
  // 口径从 items 逐条算,不读汇总计数:items 每条都带 action 和 reason,老后端
  // (duplicate 字段还没有)把纯重复记成「skipped 且无 reason」,照这个口径一样分得开。
  const items = result?.items ?? []
  const newItems = items.filter((i) => i.action === 'created')
  const existingItems = items.filter(
    (i) =>
      i.action === 'merged' ||
      i.action === 'duplicate' ||
      (i.action === 'skipped' && !i.reason),
  )
  const failedItems = items.filter((i) => i.action === 'skipped' && !!i.reason)
  const newCount = newItems.length
  const renamedCount = newItems.filter((i) => i.derived_from).length
  const mergedCount = items.filter((i) => i.action === 'merged').length
  const existingCount = existingItems.length
  const failedCount = failedItems.length

  const targets = (result?.items ?? []).filter((i) => WRITTEN_ACTIONS.has(i.action))
  const terminalCount = targets.filter((i) =>
    TERMINAL_PHASES.has(verifyStates[i.account_id]?.phase ?? ''),
  ).length
  const okCount = targets.filter((i) => verifyStates[i.account_id]?.phase === 'ok').length
  const noQuotaCount = targets.filter(
    (i) => verifyStates[i.account_id]?.phase === 'noQuota',
  ).length
  const failCount = targets.filter(
    (i) => verifyStates[i.account_id]?.phase === 'failed',
  ).length
  // 仅"本次新建且验活失败"的账号可一键删除(merged 是已有账号,绝不连带删)。
  const failedCreatedIds = targets
    .filter(
      (i) =>
        i.action === 'created' &&
        verifyStates[i.account_id]?.phase === 'failed' &&
        !removedIds.has(i.account_id),
    )
    .map((i) => i.account_id)

  const handleRemoveFailed = async () => {
    if (removing || failedCreatedIds.length === 0) return
    const run = runSeqRef.current
    const stale = () => runSeqRef.current !== run
    setRemoving(true)
    setError(null)
    const removed = new Set(removedIds)
    try {
      for (const id of failedCreatedIds) {
        try {
          await deleteAccount(id)
          removed.add(id)
          if (stale()) return
          setRemovedIds(new Set(removed))
        } catch (err) {
          if (stale()) return
          setError(`${id}: ${extractErrorMessage(err)}`)
        }
      }
    } finally {
      if (!stale()) setRemoving(false)
      invalidateAccountDomains()
    }
  }

  // 只有**写操作**(导入提交/删除)进行中不允许关闭;验活是只读探测,关闭即取消
  // (审查 Architect#4/Minimalist#4:worker 离线时串行重试可达几十分钟,必须给取消路径)。
  const busy = submitting || removing
  const safeClose = () => {
    if (busy) return
    onClose()
  }

  const verifyBadge = (state: VerifyState | undefined, removed: boolean) => {
    if (removed) {
      return (
        <span className="inline-flex items-center gap-1 text-xs text-muted-foreground line-through">
          {t('accounts.import.removedTag')}
        </span>
      )
    }
    if (!state) return null
    switch (state.phase) {
      case 'verifying':
        return (
          <span className="inline-flex items-center gap-1 text-xs text-sky-600 dark:text-sky-400">
            <Loader2 className="h-3.5 w-3.5 animate-spin" />
            {t('accounts.import.verifying')}
          </span>
        )
      case 'waiting':
        return (
          <span className="inline-flex items-center gap-1 text-xs text-muted-foreground">
            <Loader2 className="h-3.5 w-3.5 animate-spin" />
            {t('accounts.import.verifyWaitingSync')}
          </span>
        )
      case 'ok': {
        const q = state.quota
        return (
          <span className="inline-flex items-center gap-1 text-xs text-emerald-600 dark:text-emerald-400">
            <CheckCircle2 className="h-3.5 w-3.5" />
            {t('accounts.import.verifyOk')}
            <span className="tabular-nums" title={q.label ?? undefined}>
              {formatCredits(q.remaining)}
              <span className="text-muted-foreground"> / {formatCredits(q.limit)}</span>
            </span>
          </span>
        )
      }
      case 'noQuota':
        return (
          <span className="inline-flex items-center gap-1 text-xs text-warning">
            <AlertCircle className="h-3.5 w-3.5" />
            {t('accounts.import.verifyNoQuota')}
          </span>
        )
      case 'failed':
        return (
          <span
            className="inline-flex max-w-64 items-center gap-1 text-xs text-destructive"
            title={state.error}
          >
            <XCircle className="h-3.5 w-3.5 shrink-0" />
            <span className="truncate">
              {t('accounts.import.verifyFail')}: {state.error}
            </span>
          </span>
        )
      case 'pending':
        return (
          <span className="inline-flex items-center gap-1 text-xs text-muted-foreground">
            <Clock className="h-3.5 w-3.5" />
            {t('accounts.import.verifyPending')}
          </span>
        )
      default:
        return null
    }
  }

  return (
    <Modal open={open} onClose={safeClose} title={t('accounts.import.title')} className="max-w-xl">
      {result !== null ? (
        // 导入结果:三个数醒目摊开,验活改成按需点。
        <div className="mt-4 space-y-4">
          <div className="grid grid-cols-3 gap-2">
            <div className="rounded-2xl bg-emerald-500/10 px-3 py-3 text-center">
              <div className="text-3xl font-semibold tabular-nums text-emerald-600 dark:text-emerald-400">
                {newCount}
              </div>
              <div className="mt-0.5 text-xs font-medium text-emerald-700/80 dark:text-emerald-400/80">
                {t('accounts.import.sumNew')}
              </div>
            </div>
            <div className="rounded-2xl bg-sky-500/10 px-3 py-3 text-center">
              <div className="text-3xl font-semibold tabular-nums text-sky-600 dark:text-sky-400">
                {existingCount}
              </div>
              <div className="mt-0.5 text-xs font-medium text-sky-700/80 dark:text-sky-400/80">
                {t('accounts.import.sumExisting')}
              </div>
            </div>
            <div
              className={`rounded-2xl px-3 py-3 text-center ${
                failedCount > 0 ? 'bg-destructive/10' : 'bg-muted'
              }`}
            >
              <div
                className={`text-3xl font-semibold tabular-nums ${
                  failedCount > 0 ? 'text-destructive' : 'text-muted-foreground'
                }`}
              >
                {failedCount}
              </div>
              <div
                className={`mt-0.5 text-xs font-medium ${
                  failedCount > 0 ? 'text-destructive' : 'text-muted-foreground'
                }`}
              >
                {t('accounts.import.sumFailed')}
              </div>
            </div>
          </div>
          {/* 数字下面一行小字,解释这批号的构成 —— 不用点开清单就知道发生了什么 */}
          <div className="space-y-1 text-xs text-muted-foreground">
            {renamedCount > 0 && (
              <p className="text-emerald-600 dark:text-emerald-400">
                {t('accounts.import.sumNewRenamed', { n: renamedCount })}
              </p>
            )}
            {mergedCount > 0 && <p>{t('accounts.import.sumExistingMerged', { n: mergedCount })}</p>}
            {newCount === 0 && failedCount === 0 && existingCount > 0 && (
              <p>{t('accounts.import.sumAllDuplicate')}</p>
            )}
            {failedCount > 0 && (
              <p className="text-destructive">{t('accounts.import.sumFailedHint')}</p>
            )}
          </div>
          {conflictCount > 0 && (
            <p className="flex items-start gap-1.5 text-xs text-warning">
              <Info className="mt-0.5 h-3.5 w-3.5 shrink-0" />
              <span>
                {conflictCount} {t('accounts.import.machineIdConflict')}
              </span>
            </p>
          )}

          {/* 验活进度:只在点过「验活」之后才出现 */}
          {targets.length > 0 && verifyStarted && (
            <div className="space-y-1.5">
              <div className="flex items-center justify-between text-xs font-medium text-muted-foreground">
                <span>{t('accounts.import.verifyTitle')}</span>
                <span className="tabular-nums">
                  {terminalCount} / {targets.length}
                </span>
              </div>
              <div className="h-2 w-full overflow-hidden rounded-full bg-black/[0.08] dark:bg-white/10">
                <div
                  className="h-2 rounded-full bg-acid transition-all"
                  style={{ width: `${(terminalCount / targets.length) * 100}%` }}
                />
              </div>
              <div className="flex gap-3 text-xs">
                <span className="text-emerald-600 dark:text-emerald-400">
                  ✓ {t('accounts.import.verifySummaryOk')}: {okCount}
                </span>
                <span className="text-warning">
                  ⚠ {t('accounts.import.verifySummaryNoQuota')}: {noQuotaCount}
                </span>
                <span className="text-destructive">
                  ✗ {t('accounts.import.verifySummaryFail')}: {failCount}
                </span>
              </div>
            </div>
          )}

          {/* 逐条清单:没收下的排最前(要处理的放最上面),其余按原序 */}
          <ul className="max-h-56 space-y-1.5 overflow-y-auto text-xs">
            {[...failedItems, ...newItems, ...existingItems].map((item) => (
              <li key={item.account_id} className="flex items-center justify-between gap-2">
                <span className="flex min-w-0 items-center gap-2">
                  <code className="truncate font-mono text-muted-foreground">
                    {item.account_id}
                  </code>
                  {item.derived_from != null && item.derived_from !== '' && (
                    <span className="shrink-0 text-muted-foreground">
                      ({t('accounts.import.renamedFrom')} {item.derived_from})
                    </span>
                  )}
                  <span
                    className={`shrink-0 ${
                      item.action === 'created'
                        ? 'text-emerald-600 dark:text-emerald-400'
                        : item.action === 'skipped' && item.reason
                          ? 'text-destructive'
                          : 'text-muted-foreground'
                    }`}
                  >
                    {item.action === 'created'
                      ? t('accounts.import.created')
                      : item.action === 'merged'
                        ? t('accounts.import.merged')
                        : item.action === 'duplicate' || !item.reason
                          ? t('accounts.import.duplicate')
                          : t('accounts.import.skipped')}
                  </span>
                  {item.action === 'skipped' && item.reason && (
                    <span className="truncate text-destructive" title={item.reason}>
                      {item.reason}
                    </span>
                  )}
                </span>
                <span className="shrink-0">
                  {WRITTEN_ACTIONS.has(item.action)
                    ? verifyBadge(verifyStates[item.account_id], removedIds.has(item.account_id))
                    : null}
                </span>
              </li>
            ))}
          </ul>

          {error !== null && <p className="text-sm text-destructive">{error}</p>}

          {verifyStarted && (
            <p className="flex items-center gap-1.5 text-xs text-muted-foreground">
              <Info className="h-3.5 w-3.5 shrink-0" />
              {t('accounts.import.verifyHint')}
            </p>
          )}
          <div className="flex items-center justify-between gap-2 pt-1">
            {/* 验活按需点:号多时很慢,所以不自动跑 */}
            {targets.length > 0 && !verifyStarted ? (
              <Button
                variant="ghost"
                onClick={() => {
                  setVerifyStarted(true)
                  void runVerification(result.items)
                }}
              >
                <ShieldCheck className="h-4 w-4" />
                {t('accounts.import.verifyStart')} ({targets.length})
              </Button>
            ) : (
              /* 验活失败的新建账号:一键删除(merged 不连带) */
              <span>
                {!verifying && failedCreatedIds.length > 0 && (
                  <Button
                    variant="ghost"
                    className="text-destructive"
                    disabled={removing}
                    onClick={() => void handleRemoveFailed()}
                  >
                    {removing ? (
                      <Loader2 className="h-4 w-4 animate-spin" />
                    ) : (
                      <Trash2 className="h-4 w-4" />
                    )}
                    {t('accounts.import.removeFailed')} ({failedCreatedIds.length})
                  </Button>
                )}
              </span>
            )}
            <Button onClick={safeClose} disabled={busy}>
              {verifying ? t('accounts.import.cancelVerify') : t('common.close')}
            </Button>
          </div>
        </div>
      ) : (
        <form onSubmit={handleSubmit} className="mt-4 space-y-4">
          {/* 导入方式:两栏等宽卡片。以前是个灰底小 tab,用户找不到 API Key 这条路,
              以为压根没实现 —— 入口显眼程度本身就是功能的一部分。 */}
          <div className="grid grid-cols-2 gap-2">
            {(
              [
                { value: 'json', label: 'modeJson', desc: 'modeJsonDesc' },
                { value: 'apikeys', label: 'modeApiKeys', desc: 'modeApiKeysDesc' },
              ] as const
            ).map((option) => (
              <button
                key={option.value}
                type="button"
                onClick={() => setMode(option.value)}
                aria-pressed={mode === option.value}
                className={`rounded-2xl border p-3 text-left transition-colors ${
                  mode === option.value
                    ? 'border-primary bg-primary/5 ring-1 ring-primary/40'
                    : 'hover:bg-muted/60'
                }`}
              >
                <span className="block text-sm font-medium">
                  {t(`accounts.import.${option.label}`)}
                </span>
                <span className="mt-0.5 block text-xs text-muted-foreground">
                  {t(`accounts.import.${option.desc}`)}
                </span>
              </button>
            ))}
          </div>

          {/* 目标分组 */}
          <div className="space-y-1.5">
            <label htmlFor="import-group" className="text-xs font-medium text-muted-foreground">
              {t('accounts.import.group')}
            </label>
            <Select
              id="import-group"
              value={group}
              onChange={(event) => setGroup(event.target.value)}
              className="w-full"
            >
              <option value="">{t('groups.ungrouped')}</option>
              {(groupsQuery.data ?? []).map((g) => (
                <option key={g.name} value={g.name}>
                  {g.name}
                </option>
              ))}
            </Select>
          </div>

          {/* 出口网关（上号时选：直连 / 自动均衡 / 指定网关） */}
          <div className="space-y-1.5">
            <label htmlFor="import-egress" className="text-xs font-medium text-muted-foreground">
              {t('accounts.field.egress')}
            </label>
            <Select
              id="import-egress"
              value={egress}
              onChange={(event) => setEgress(event.target.value)}
              className="w-full"
            >
              <option value="direct">{t('accounts.egress.direct')}</option>
              <option value="auto">{t('accounts.egress.auto')}</option>
              {gateways.map((url, i) => (
                <option key={i} value={String(i)}>
                  {gatewayLabel(url, i)}
                </option>
              ))}
            </Select>
          </div>

          {mode === 'json' ? (
            /* JSON 粘贴 + 文件选择 */
            <div className="space-y-1.5">
              <div className="flex items-center justify-between">
                <label htmlFor="import-json" className="text-xs font-medium text-muted-foreground">
                  {t('accounts.import.jsonLabel')}
                </label>
                <button
                  type="button"
                  onClick={() => fileInputRef.current?.click()}
                  className="inline-flex items-center gap-1 text-xs text-primary hover:underline"
                >
                  <Upload className="h-3 w-3" />
                  {t('accounts.import.chooseFile')}
                </button>
                <input
                  ref={fileInputRef}
                  type="file"
                  accept=".json,application/json"
                  className="hidden"
                  onChange={handleFile}
                />
              </div>
              <textarea
                id="import-json"
                value={json}
                onChange={(event) => setJson(event.target.value)}
                placeholder={t('accounts.import.jsonPlaceholder')}
                rows={8}
                spellCheck={false}
                autoComplete="off"
                className={`${inputClass} resize-none font-mono text-xs leading-5`}
              />
            </div>
          ) : (
            /* API Key 粘贴(每行一个 ksk_...) */
            <div className="space-y-1.5">
              <label htmlFor="import-keys" className="text-xs font-medium text-muted-foreground">
                {t('accounts.import.keysLabel')}
              </label>
              <textarea
                id="import-keys"
                value={keys}
                onChange={(event) => setKeys(event.target.value)}
                placeholder={t('accounts.import.keysPlaceholder')}
                rows={8}
                spellCheck={false}
                autoComplete="off"
                className={`${inputClass} resize-none font-mono text-xs leading-5`}
              />
            </div>
          )}

          {error !== null && <p className="text-sm text-destructive">{error}</p>}

          <p className="flex items-start gap-1.5 text-xs text-muted-foreground">
            <Info className="mt-0.5 h-3.5 w-3.5 shrink-0" />
            {mode === 'apikeys' ? t('accounts.import.keysHint') : t('accounts.import.hint')}
          </p>

          <div className="flex justify-end gap-2 pt-1">
            <Button variant="ghost" onClick={safeClose}>
              {t('common.cancel')}
            </Button>
            <Button type="submit" disabled={submitting}>
              {submitting && <Loader2 className="h-4 w-4 animate-spin" />}
              {submitting ? t('accounts.import.importing') : t('accounts.import.submit')}
            </Button>
          </div>
        </form>
      )}
    </Modal>
  )
}
