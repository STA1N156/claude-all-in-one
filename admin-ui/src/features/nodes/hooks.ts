import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'

import { queryKeys } from '@/lib/query-keys'

import { createNode, deleteNode, execOnNode, fetchNodes, resetNodeBinding, updateNode } from './api'

/**
 * 子机列表 + hub 实时状态。10s 轮询:在线/离线直接决定这台子机上的号此刻能不能被选中,
 * 比账号运行态(15s)稍紧一点,排查断线时不至于盯着过期的面板。
 */
export function useNodes() {
  return useQuery({
    queryKey: queryKeys.nodes.list(),
    queryFn: fetchNodes,
    refetchInterval: 10_000,
  })
}

export function useCreateNode() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (name: string) => createNode(name),
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: queryKeys.nodes.root })
    },
  })
}

export function useUpdateNode() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, patch }: { id: string; patch: { name?: string; disabled?: boolean } }) =>
      updateNode(id, patch),
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: queryKeys.nodes.root })
    },
  })
}

/** 吊销。号绑在子机上不能迁移,所以账号域也要刷新(那些号就此不可用了)。 */
export function useDeleteNode() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, force }: { id: string; force: boolean }) => deleteNode(id, force),
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: queryKeys.nodes.root })
      void queryClient.invalidateQueries({ queryKey: queryKeys.accounts.root })
    },
  })
}

export function useResetNodeBinding() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: string) => resetNodeBinding(id),
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: queryKeys.nodes.root })
    },
  })
}

/** 远端 bash:一次性动作(最长 140s),不刷任何查询。signal 用于关对话框时取消。 */
export function useExecNode() {
  return useMutation({
    mutationFn: ({
      id,
      command,
      timeoutSecs,
      signal,
    }: {
      id: string
      command: string
      timeoutSecs: number
      signal?: AbortSignal
    }) => execOnNode(id, command, timeoutSecs, signal),
  })
}
