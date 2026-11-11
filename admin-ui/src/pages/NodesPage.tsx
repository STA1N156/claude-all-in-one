import { useEffect, useRef, useState } from 'react'
import {
  AlertTriangle,
  CheckCircle2,
  Copy,
  Loader2,
  Plug,
  PlugZap,
  Server,
  SquareTerminal,
  Trash2,
} from 'lucide-react'

import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { ErrorNote } from '@/components/ui/error-note'
import { Modal } from '@/components/ui/modal'
import { StatCard } from '@/components/ui/stat-card'
import {
  useCreateNode,
  useDeleteNode,
  useExecNode,
  useNodes,
  useResetNodeBinding,
  useUpdateNode,
} from '@/features/nodes/hooks'
import type { CreateNodeResult, ExecResult, NodeRow } from '@/features/nodes/types'
import { extractErrorMessage } from '@/lib/api'
import { copyText } from '@/lib/clipboard'
import { useI18n } from '@/lib/i18n'

const inputClass =
  'w-full rounded-2xl border bg-input px-4 py-2.5 text-sm outline-none transition-colors placeholder:text-muted-foreground'

/** epoch 秒 → "15s / 3m / 2h" 的相对值。子机心跳 15s 一次,绝对时间没意义。 */
function ago(ts: number | null | undefined): string {
  if (ts === null || ts === undefined || ts === 0) return '—'
  const s = Math.max(0, Math.floor(Date.now() / 1000) - ts)
  if (s < 60) return `${s}s`
  if (s < 3600) return `${Math.floor(s / 60)}m`
  if (s < 86400) return `${Math.floor(s / 3600)}h`
  return `${Math.floor(s / 86400)}d`
}

function CopyButton({ text, label }: { text: string; label: string }) {
  const [copied, setCopied] = useState(false)
  return (
    <Button
      variant="outline"
      onClick={() => {
        // 后台常跑在 http 内网地址上,navigator.clipboard 在那里是 undefined(非安全上下文),
        // 直接调会静默没反应 —— 走带 execCommand 回退的 copyText。
        void copyText(text).then((ok) => {
          if (!ok) return
          setCopied(true)
          window.setTimeout(() => setCopied(false), 1500)
        })
      }}
    >
      {copied ? <CheckCircle2 className="h-4 w-4 text-success" /> : <Copy className="h-4 w-4" />}
      {label}
    </Button>
  )
}

/** 新建子机后的一次性令牌 + 安装命令。关掉就再也看不到(库里只有摘要)。 */
function NewNodeDialog({ result, onClose }: { result: CreateNodeResult; onClose: () => void }) {
  const { t } = useI18n()
  return (
    <Modal open onClose={onClose} title={t('nodes.new.title')}>
      <div className="mt-4 space-y-4">
        <p className="flex items-start gap-1.5 rounded-2xl border border-dashed border-warning/40 bg-warning/5 p-3 text-xs text-muted-foreground">
          <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0 text-warning" />
          {t('nodes.new.once')}
        </p>
        {!result.hub_ready && (
          <p className="rounded-2xl border border-dashed border-destructive/40 bg-destructive/5 p-3 text-xs text-destructive">
            {t('nodes.new.hubUnknown')}
          </p>
        )}
        {result.hub_ready && !result.bundle_ready && (
          <p className="rounded-2xl border border-dashed border-warning/40 bg-warning/5 p-3 text-xs text-amber-700 dark:text-amber-300">
            {t('nodes.new.bundleMissing')}
          </p>
        )}
        <div className="space-y-3">
          <p className="text-xs font-medium text-muted-foreground">{t('nodes.new.installCmd')}</p>
          {/* 用 textarea 而不是 pre:手动框选/全选复制都一字不差(pre 的折行在部分
              终端里会带错位的换行);只读、自适应行高、不抢焦点。
              有两条命令时(有 sudo / 无 sudo)各给一块;退回「先传包」旧形态时只有一条。 */}
          {(result.install_cmd_user
            ? [
                { label: t('nodes.new.cmdSudo'), cmd: result.install_cmd },
                { label: t('nodes.new.cmdUser'), cmd: result.install_cmd_user },
              ]
            : [{ label: '', cmd: result.install_cmd }]
          ).map((c) => (
            <div key={c.label || 'single'} className="space-y-1.5">
              {c.label && <p className="text-xs text-muted-foreground">{c.label}</p>}
              <textarea
                readOnly
                rows={result.install_cmd_user ? 2 : 4}
                value={c.cmd}
                onFocus={(event) => event.target.select()}
                className={`${inputClass} font-mono text-xs leading-5`}
              />
              <div className="flex gap-2 pt-0.5">
                <CopyButton text={c.cmd} label={t('nodes.new.copyCmd')} />
              </div>
            </div>
          ))}
          <div className="flex gap-2 pt-1">
            <CopyButton text={result.token} label={t('nodes.new.copyToken')} />
          </div>
        </div>
        <p className="text-xs text-muted-foreground">
          {result.bundle_ready ? t('nodes.new.stepsOneliner') : t('nodes.new.steps')}
        </p>
        <div className="flex justify-end pt-1">
          <Button onClick={onClose}>{t('nodes.new.close')}</Button>
        </div>
      </div>
    </Modal>
  )
}

/** 远端 bash 终端:命令以 caio 用户在子机上执行(环境变量已清空),限时限量。 */
function ExecDialog({ node, onClose }: { node: NodeRow; onClose: () => void }) {
  const { t } = useI18n()
  const exec = useExecNode()
  const [command, setCommand] = useState('')
  const [timeoutSecs, setTimeoutSecs] = useState(30)
  const [result, setResult] = useState<ExecResult | null>(null)
  const [error, setError] = useState<string | null>(null)
  // 关对话框 = 取消在跑的命令(链路会一路断到子机,exec 的进程组会被杀掉)。
  const abortRef = useRef<AbortController | null>(null)
  const close = () => {
    abortRef.current?.abort()
    onClose()
  }

  const run = () => {
    if (exec.isPending || command.trim() === '') return
    setError(null)
    setResult(null)
    const controller = new AbortController()
    abortRef.current = controller
    exec.mutate(
      { id: node.id, command, timeoutSecs, signal: controller.signal },
      {
        onSuccess: setResult,
        onError: (err) => {
          if (controller.signal.aborted) return
          setError(extractErrorMessage(err))
        },
      },
    )
  }

  return (
    <Modal
      open
      onClose={close}
      title={`${t('nodes.exec.title')} · ${node.name !== '' ? node.name : node.id}`}
      className="max-w-2xl"
    >
      <div className="mt-4 space-y-3">
        <p className="flex items-start gap-1.5 rounded-2xl border border-dashed border-warning/40 bg-warning/5 p-3 text-xs text-muted-foreground">
          <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0 text-warning" />
          {t('nodes.exec.warn')}
        </p>
        <textarea
          rows={3}
          value={command}
          onChange={(event) => setCommand(event.target.value)}
          onKeyDown={(event) => {
            if ((event.ctrlKey || event.metaKey) && event.key === 'Enter') run()
          }}
          placeholder="journalctl -u caio-dario -n 50 --no-pager"
          autoFocus
          className={`${inputClass} font-mono`}
        />
        <div className="flex items-center gap-2">
          <select
            value={timeoutSecs}
            onChange={(event) => setTimeoutSecs(Number(event.target.value))}
            className="rounded-2xl border bg-input px-3 py-2 text-sm outline-none"
            title={t('nodes.exec.timeout')}
          >
            {[10, 30, 60, 120].map((s) => (
              <option key={s} value={s}>
                {t('nodes.exec.timeout')} {s}s
              </option>
            ))}
          </select>
          <Button onClick={run} disabled={exec.isPending || command.trim() === ''}>
            {exec.isPending ? (
              <Loader2 className="h-4 w-4 animate-spin" />
            ) : (
              <SquareTerminal className="h-4 w-4" />
            )}
            {t('nodes.exec.run')}
          </Button>
          <span className="text-xs text-muted-foreground">Ctrl/⌘+Enter</span>
        </div>
        {error !== null && <p className="text-sm text-destructive">{error}</p>}
        {result !== null && (
          <div className="space-y-2">
            <div className="flex flex-wrap items-center gap-2 text-xs">
              {result.timed_out ? (
                <Badge variant="destructive">{t('nodes.exec.timedOut')}</Badge>
              ) : (
                <Badge variant={result.exit_code === 0 ? 'success' : 'destructive'}>
                  {t('nodes.exec.exitCode')} {result.exit_code ?? '—'}
                </Badge>
              )}
              <span className="text-muted-foreground">{(result.duration_ms / 1000).toFixed(1)}s</span>
              {(result.stdout_truncated || result.stderr_truncated) && (
                <span className="text-amber-700 dark:text-amber-300">{t('nodes.exec.truncated')}</span>
              )}
              {result.output_incomplete && (
                <span className="text-amber-700 dark:text-amber-300">{t('nodes.exec.incomplete')}</span>
              )}
            </div>
            {result.stdout !== '' && (
              <pre className="max-h-72 overflow-auto rounded-2xl border bg-input p-3 font-mono text-xs leading-5 whitespace-pre-wrap break-all">
                {result.stdout}
              </pre>
            )}
            {result.stderr !== '' && (
              <pre className="max-h-72 overflow-auto rounded-2xl border border-destructive/30 bg-destructive/5 p-3 font-mono text-xs leading-5 whitespace-pre-wrap break-all">
                {result.stderr}
              </pre>
            )}
            {result.stdout === '' && result.stderr === '' && (
              <p className="text-xs text-muted-foreground">{t('nodes.exec.noOutput')}</p>
            )}
          </div>
        )}
      </div>
    </Modal>
  )
}

function NodeCard({ node, execEnabled }: { node: NodeRow; execEnabled: boolean }) {
  const { t } = useI18n()
  const update = useUpdateNode()
  const remove = useDeleteNode()
  const resetBinding = useResetNodeBinding()
  const [confirmRevoke, setConfirmRevoke] = useState(false)
  const [execOpen, setExecOpen] = useState(false)
  const [error, setError] = useState<string | null>(null)
  /** 吊销后就此作废、只能删掉的号(后端原样回传)。 */
  const [deadAccounts, setDeadAccounts] = useState<string[]>([])

  const info = node.info ?? null
  const supervisor = info?.supervisor ?? null
  const supAccounts = supervisor?.accounts ?? []
  const running = supAccounts.filter((a) => a.running === true).length
  const orphans = node.orphan_accounts ?? []
  const missing = node.missing_accounts ?? []
  const pending = node.pending_deletions ?? []
  const fail = (err: unknown) => setError(extractErrorMessage(err))

  return (
    <Card>
      <CardHeader className="flex flex-row items-start justify-between gap-3">
        <div className="min-w-0 space-y-1">
          <CardTitle className="flex items-center gap-2 truncate">
            <Server className="h-4 w-4 shrink-0 text-muted-foreground" />
            <span className="truncate">{node.name !== '' ? node.name : node.id}</span>
          </CardTitle>
          <p className="font-mono text-xs text-muted-foreground">{node.id}</p>
        </div>
        <div className="flex shrink-0 flex-wrap items-center justify-end gap-1.5">
          {node.online ? (
            <Badge variant="success">{t('nodes.state.online')}</Badge>
          ) : (
            <Badge variant="destructive">{t('nodes.state.offline')}</Badge>
          )}
          {node.disabled && <Badge variant="warning">{t('nodes.state.drained')}</Badge>}
          {!node.bound && <Badge variant="muted">{t('nodes.state.unbound')}</Badge>}
        </div>
      </CardHeader>
      <CardContent className="space-y-3">
        <dl className="grid grid-cols-2 gap-x-4 gap-y-2 text-xs sm:grid-cols-3">
          <div>
            <dt className="text-muted-foreground">{t('nodes.col.ip')}</dt>
            <dd className="font-mono">{node.peer_ip ?? node.last_ip ?? '—'}</dd>
          </div>
          <div>
            <dt className="text-muted-foreground">{t('nodes.col.accounts')}</dt>
            <dd>
              {node.accounts}
              {supervisor !== null && supervisor.error === undefined && (
                <span className="text-muted-foreground"> ({t('nodes.col.running')} {running})</span>
              )}
            </dd>
          </div>
          <div>
            <dt className="text-muted-foreground">{t('nodes.col.lastSeen')}</dt>
            <dd>{ago(node.last_seen)}</dd>
          </div>
          <div>
            <dt className="text-muted-foreground">{t('nodes.col.rtt')}</dt>
            <dd>{node.rtt_ms !== null && node.rtt_ms !== undefined ? `${node.rtt_ms} ms` : '—'}</dd>
          </div>
          <div>
            <dt className="text-muted-foreground">{t('nodes.col.version')}</dt>
            <dd className="truncate font-mono">{node.version ?? '—'}</dd>
          </div>
          <div>
            <dt className="text-muted-foreground">{t('nodes.col.host')}</dt>
            <dd className="truncate font-mono">{node.hostname ?? '—'}</dd>
          </div>
        </dl>

        {info?.loadavg != null && (
          <p className="text-xs text-muted-foreground">
            {t('nodes.col.load')} {info.loadavg}
            {info.mem_available_kb != null && info.mem_total_kb != null && (
              <>
                {' · '}
                {t('nodes.col.mem')}{' '}
                {Math.round(info.mem_available_kb / 1024)}/{Math.round(info.mem_total_kb / 1024)} MB
              </>
            )}
          </p>
        )}
        {supervisor?.error !== undefined && (
          <p className="text-xs text-destructive">
            {t('nodes.supervisorDown')} {supervisor.error}
          </p>
        )}

        {/* 对账:「一个号的令牌只能有一个持有者」是整套设计的前提,这里把它变成看得见的。 */}
        {orphans.length > 0 && (
          <p className="rounded-2xl border border-dashed border-warning/40 bg-warning/5 p-3 text-xs text-amber-700 dark:text-amber-300">
            {t('nodes.recon.orphan')} <span className="font-mono">{orphans.join(', ')}</span>
          </p>
        )}
        {missing.length > 0 && (
          <p className="rounded-2xl border border-dashed border-destructive/40 bg-destructive/5 p-3 text-xs text-destructive">
            {t('nodes.recon.missing')} <span className="font-mono">{missing.join(', ')}</span>
          </p>
        )}
        {pending.length > 0 && (
          <p className="text-xs text-muted-foreground">
            {t('nodes.recon.pending')} <span className="font-mono">{pending.join(', ')}</span>
          </p>
        )}

        {error !== null && <p className="text-xs text-destructive">{error}</p>}

        <div className="flex flex-wrap gap-2 pt-1">
          {node.online && execEnabled && (
            <Button
              variant="outline"
              onClick={() => {
                setError(null)
                setExecOpen(true)
              }}
            >
              <SquareTerminal className="h-4 w-4" />
              {t('nodes.action.terminal')}
            </Button>
          )}
          <Button
            variant="outline"
            disabled={update.isPending}
            onClick={() => {
              setError(null)
              update.mutate(
                { id: node.id, patch: { disabled: !node.disabled } },
                { onError: fail },
              )
            }}
          >
            {update.isPending ? (
              <Loader2 className="h-4 w-4 animate-spin" />
            ) : node.disabled ? (
              <PlugZap className="h-4 w-4" />
            ) : (
              <Plug className="h-4 w-4" />
            )}
            {node.disabled ? t('nodes.action.resume') : t('nodes.action.drain')}
          </Button>
          {node.bound && (
            <Button
              variant="outline"
              disabled={resetBinding.isPending}
              onClick={() => {
                setError(null)
                resetBinding.mutate(node.id, { onError: fail })
              }}
            >
              {t('nodes.action.resetBinding')}
            </Button>
          )}
          <Button
            variant="outline"
            disabled={remove.isPending}
            onClick={() => {
              setError(null)
              setConfirmRevoke(true)
            }}
          >
            <Trash2 className="h-4 w-4" />
            {t('nodes.action.revoke')}
          </Button>
        </div>
      </CardContent>

      <Modal
        open={confirmRevoke}
        onClose={() => setConfirmRevoke(false)}
        title={t('nodes.revoke.title')}
      >
        <div className="mt-4 space-y-3">
          <p className="text-sm">{t('nodes.revoke.body')}</p>
          {node.accounts > 0 && (
            <p className="rounded-2xl border border-dashed border-destructive/40 bg-destructive/5 p-3 text-xs text-destructive">
              {t('nodes.revoke.hasAccounts')} {node.accounts}
            </p>
          )}
          <div className="flex justify-end gap-2 pt-1">
            <Button variant="ghost" onClick={() => setConfirmRevoke(false)}>
              {t('common.cancel')}
            </Button>
            <Button
              variant="destructive"
              disabled={remove.isPending}
              onClick={() => {
                remove.mutate(
                  { id: node.id, force: node.accounts > 0 },
                  {
                    onSuccess: (res) => {
                      setConfirmRevoke(false)
                      setDeadAccounts(res.dead_accounts)
                    },
                    onError: (err) => {
                      setConfirmRevoke(false)
                      fail(err)
                    },
                  },
                )
              }}
            >
              {remove.isPending && <Loader2 className="h-4 w-4 animate-spin" />}
              {t('nodes.action.revoke')}
            </Button>
          </div>
        </div>
      </Modal>
      <Modal
        open={deadAccounts.length > 0}
        onClose={() => setDeadAccounts([])}
        title={t('nodes.revoke.doneTitle')}
      >
        <div className="mt-4 space-y-3">
          <p className="text-sm">{t('nodes.revoke.doneBody')}</p>
          <pre className="overflow-x-auto rounded-2xl border bg-input p-3 font-mono text-xs leading-5">
            {deadAccounts.join('\n')}
          </pre>
          <p className="text-xs text-muted-foreground">{t('nodes.revoke.doneCleanup')}</p>
          <div className="flex justify-end pt-1">
            <Button onClick={() => setDeadAccounts([])}>{t('nodes.new.close')}</Button>
          </div>
        </div>
      </Modal>
      {execOpen && <ExecDialog node={node} onClose={() => setExecOpen(false)} />}
    </Card>
  )
}

/**
 * 子机(dario 节点)页。
 *
 * 一台子机 = 一台别的机器上的「节点程序 + dario 管理器 + 每号一个 dario 进程」。
 * 号绑在某台子机上就用那台机器的真实 IP 出网;子机离线时它的号在选号阶段被排除,
 * 请求自动走别的号。号不能迁移(令牌只能有一个持有者),所以吊销是不可逆操作。
 */
export default function NodesPage() {
  const { t } = useI18n()
  const { data, error, isLoading } = useNodes()
  const create = useCreateNode()
  const [addOpen, setAddOpen] = useState(false)
  const [name, setName] = useState('')
  const [created, setCreated] = useState<CreateNodeResult | null>(null)
  const [addError, setAddError] = useState<string | null>(null)

  useEffect(() => {
    if (addOpen) {
      setName('')
      setAddError(null)
    }
  }, [addOpen])

  const nodes = data?.nodes ?? []
  const online = nodes.filter((n) => n.online).length
  const nodeAccounts = nodes.reduce((sum, n) => sum + n.accounts, 0)

  return (
    <div className="space-y-6">
      <div className="flex flex-wrap items-end justify-between gap-3">
        <div>
          <p className="text-xs uppercase tracking-widest text-muted-foreground">Nodes</p>
          <h1 className="text-2xl font-semibold">{t('nodes.title')}</h1>
          <p className="mt-1 text-sm text-muted-foreground">{t('nodes.subtitle')}</p>
        </div>
        <Button onClick={() => setAddOpen(true)}>{t('nodes.action.add')}</Button>
      </div>

      {error != null && <ErrorNote error={error} />}

      {data !== undefined && !data.hub.reachable && (
        <p className="rounded-2xl border border-dashed border-destructive/40 bg-destructive/5 p-3 text-sm text-destructive">
          {t('nodes.hubDown')}
        </p>
      )}

      <div className="grid gap-4 sm:grid-cols-3">
        <StatCard
          icon={PlugZap}
          label={t('nodes.stat.online')}
          value={`${online}/${nodes.length}`}
          loading={isLoading}
        />
        <StatCard
          icon={Server}
          label={t('nodes.stat.accounts')}
          value={String(nodeAccounts)}
          loading={isLoading}
        />
        <StatCard
          icon={Plug}
          label={t('nodes.stat.hub')}
          value={data?.hub.public_addr ?? '—'}
          sub={data?.hub.spki ?? undefined}
          loading={isLoading}
        />
      </div>

      {!isLoading && nodes.length === 0 && (
        <p className="rounded-2xl border border-dashed p-6 text-center text-sm text-muted-foreground">
          {t('nodes.empty')}
        </p>
      )}

      <div className="grid gap-4 lg:grid-cols-2">
        {nodes.map((n) => (
          <NodeCard key={n.id} node={n} execEnabled={data?.hub.exec_enabled === true} />
        ))}
      </div>

      <Modal open={addOpen} onClose={() => setAddOpen(false)} title={t('nodes.add.title')}>
        <div className="mt-4 space-y-4">
          <div className="space-y-1.5">
            <label htmlFor="node-name" className="text-xs font-medium text-muted-foreground">
              {t('nodes.add.name')}
            </label>
            <input
              id="node-name"
              value={name}
              onChange={(event) => setName(event.target.value)}
              placeholder={t('nodes.add.namePlaceholder')}
              autoFocus
              className={inputClass}
            />
            <p className="text-xs text-muted-foreground">{t('nodes.add.hint')}</p>
          </div>
          {addError !== null && <p className="text-sm text-destructive">{addError}</p>}
          <div className="flex justify-end gap-2 pt-1">
            <Button variant="ghost" onClick={() => setAddOpen(false)}>
              {t('common.cancel')}
            </Button>
            <Button
              disabled={create.isPending}
              onClick={() => {
                if (create.isPending) return
                setAddError(null)
                create.mutate(name.trim(), {
                  onSuccess: (res) => {
                    setAddOpen(false)
                    setCreated(res)
                  },
                  onError: (err) => setAddError(extractErrorMessage(err)),
                })
              }}
            >
              {create.isPending && <Loader2 className="h-4 w-4 animate-spin" />}
              {t('nodes.add.submit')}
            </Button>
          </div>
        </div>
      </Modal>

      {created !== null && <NewNodeDialog result={created} onClose={() => setCreated(null)} />}
    </div>
  )
}
