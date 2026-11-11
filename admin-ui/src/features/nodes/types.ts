/** 子机(dario 节点):与 `/admin/api/nodes*` 对应的类型。 */

/** 子机上 dario 管理器里一个号的状态(由子机 `/info` 透上来,结构随管理器变动,按可选读)。 */
export interface NodeAccountInfo {
  id?: string
  running?: boolean
  port?: number | null
  needsRelogin?: boolean
  lastUsedAt?: number | null
}

/** 子机自报的运行态:版本、负载、管理器里各号的状态。拉不到时为 null。 */
export interface NodeInfo {
  version?: string
  hostname?: string
  uptime_secs?: number
  loadavg?: string | null
  mem_total_kb?: number | null
  mem_available_kb?: number | null
  /** 管理器 `/accounts` 的原样回传;管理器不可达时是 `{ error: "…" }`。 */
  supervisor?: { accounts?: NodeAccountInfo[]; error?: string } | null
}

export interface NodeRow {
  id: string
  name: string
  /** 后台「排空」:不再往这台子机派新对话,删号/换码照常。 */
  disabled: boolean
  /** 令牌是否已绑定某台机器(首连时绑定,防一个令牌装两台)。 */
  bound: boolean
  created_at: number
  last_seen: number | null
  last_ip: string | null
  version: string | null
  /** 挂在这台子机上的号数。 */
  accounts: number
  online: boolean
  /** hub 看到的来源 IP = 这台子机的真实出口 IP。 */
  peer_ip?: string | null
  hostname?: string | null
  rtt_ms?: number | null
  connected_at?: number | null
  info?: NodeInfo | null
  /**
   * 对账:子机上有、主机库里没有的号 = **残留凭证**。
   * 它是将来「双持有」的火种(号在新机器重登后,旧机器上那份还在),也是
   * 「删号时子机离线、补删又没执行」的痕迹。null = 这次拉不到子机的清单,无法判断。
   */
  orphan_accounts?: string[] | null
  /** 对账:主机库里有、子机上没有的号 = 令牌已不在那台机器上,号其实已经死了。 */
  missing_accounts?: string[] | null
  /** 子机离线期间删掉的号,等它重连后补删。一直不为空 = 那台机器再没回来过。 */
  pending_deletions?: string[]
}

export interface NodesResponse {
  hub: {
    /** hub 本地口能不能访问。false = hub 容器没起,所有子机号都会被排除在选号之外。 */
    reachable: boolean
    /** 自签证书指纹(子机用它认主机);打安装包时要用。 */
    spki: string | null
    /** 子机侧看到的「主机:端口」,来自 hub 启动参数 `--public-addr`。 */
    public_addr: string | null
    /** 主机是否配了远端 bash 钥匙(node_exec_key);没配就不显示「终端」。 */
    exec_enabled: boolean
  }
  nodes: NodeRow[]
}

/** 吊销的返回:`dead_accounts` = 就此永久作废、只能删掉的号。 */
export interface RevokeNodeResult {
  id: string
  revoked: boolean
  dead_accounts: string[]
}

/** 新建子机的返回:令牌**只在这一次出现**,库里只存摘要。 */
export interface CreateNodeResult {
  id: string
  name: string
  /** 完整令牌(自带主机与指纹);hub 还没跑过时退化成短令牌。 */
  token: string
  token_short: string
  install_cmd: string
  /** 无 sudo 形态(用户态安装);只有「一串 curl」形态才有,退回旧形态时为 null。 */
  install_cmd_user?: string | null
  /** 安装票据(命令里的 URL)的下载有效期秒数;过期只能吊销重建。 */
  ticket_ttl_secs?: number
  /** false = 主机地址/指纹还不知道,安装时要自己带 --master/--fp。 */
  hub_ready: boolean
  /** false = 主机上还没放安装包(data/node-bundle/caio-node.run),命令退回「先传包再执行」形态。 */
  bundle_ready: boolean
}

/** 远端 bash(子机 /exec)的返回:输出按 256KB 截断,超时的命令已被杀掉。 */
export interface ExecResult {
  /** null = 被信号杀掉(含超时后的 kill)。 */
  exit_code: number | null
  timed_out: boolean
  stdout: string
  stderr: string
  stdout_truncated: boolean
  stderr_truncated: boolean
  /** true = 收割宽限到期、读者被中止:输出可能不完整(区别于 truncated 的「完整但只留头部」)。 */
  output_incomplete: boolean
  duration_ms: number
}
