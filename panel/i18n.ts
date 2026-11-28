const RAW = `t.overview|Overview|总览
t.sessions|Sessions|会话
t.tunnels|Ports|端口
t.deploy|Deploy|部署
t.settings|Settings|设置
au|Auto|自动
pa|Manual|手动
dm|Demo|演示
rl|Live|真实
bl|Balance|余额
br|Usage rate|消耗速度
et|Lasts about|预计可用
rt|Running|运行中
cg|Spending limits|花费保护
ga|On|已开启
gp|Paused|已暂停
gw|Near limit|接近上限
gi|OK|正常
ld|Loading…|正在加载…
uk|Unknown|未知
sl|Not current|数据未更新
bz|Idle · not billing|空闲 · 不计费
ot|External|外部会话
od|Started outside Colab Deck and still billing:|以下会话在 Colab Deck 外部启动，仍在持续计费：
qa|Quick actions|快捷操作
ng|New GPU|新建 GPU
dg|Deploy model|部署模型
ns|New session|新建会话
ac|Hardware|硬件
hm|High-RAM|高内存
std|Standard|标准
mv|Min GPU memory (MiB)|最低 GPU 显存（MiB）
mr|Stop after (hours)|最长运行（小时）
im|Stop when idle (min)|闲置停止（分钟）
ms|Spend limit (CU)|花费上限（CU）
bf|Keep at least (CU)|最低余额（CU）
sn|Session name|会话名称
sv|Start|启动
cc|Cancel|取消
stopBill|Stop & end billing|停止并结束计费
cs|Stop this session?|停止这个会话？
ps|Stopping…|正在停止…
e2|+2 hours|延长 2 小时
cw|Turn off auto-stop|关闭自动停止
cfw|Turn off auto-stop? It will keep billing.|关闭后会一直计费，确定吗？
aw|Turn on auto-stop|开启自动停止
oc|Open in Colab|在 Colab 中打开
rk|Restart kernel|重启内核
cfrk|Restart? Running code will stop.|确定重启？正在运行的代码会中断。
fw|Open port|转发端口
ar|Auto-stop on|自动停止已开
un|No auto-stop|未设自动停止
nos|No running sessions.|暂无运行中的会话。
tn|Closing a port doesn't stop the session. It keeps billing.|关闭端口不会停止会话，仍会继续计费。
lp|Local port|本地端口
rp|Remote port (not 8080)|远程端口（不能用 8080）
hp|Health check path|健康检查路径
op|Open port|开启转发
cl|Close|关闭
cu|Copy|复制
cr|Copy curl|复制 curl
ou|Open|打开
not|No open ports.|暂无端口转发。
sr|Search|搜索
in|Check fit|检查显存
ht|HF token (kept in memory only)|HF Token（仅驻留内存）
st|Use|使用
dp|Deploy|开始部署
stp|Stop|停止
nor|No deployments.|暂无部署。
ip|Install plugin|安装插件
ib|Install|安装
bm|Mode|模式
sd|Demo (no charges)|演示（不计费）
acc|Accounts|账号
act|Current|当前
sw|Switch|切换
cfs|Confirm switch|确认切换
sww|Switching closes your open ports. Sessions on the old account keep their auto-stop.|切换会关闭已开的端口，原账号的会话仍会按时自动停止。
aa|Add account|添加账号
df|Preferences|偏好
mc|Max sessions at once|最多同时运行
ss|Save|保存
dc|Environment|环境检查
ok|✓ Saved|✓ 已保存
dn|✓ Done|✓ 已完成
rm|✓ Removed|✓ 已移除
is|✓ Installed|✓ 已安装
cx|✓ Closed|✓ 已关闭
wk|Working…|处理中…
cp|✓ Copied|✓ 已复制
reconnected|✓ Reconnected|✓ 已重新连接
lo|Disconnected · retrying in {s}|连接断开 · {s} 后重试
of|Disconnected|连接已断开
ry|Retry|重试
dt|Details|详情
rf|Refresh|刷新
bt|Balance over time|余额走势
p1|A100 comes in 40 GB and 80 GB. For 80 GB, set min GPU memory to 70000.|A100 分为 40 GB 与 80 GB；如需 80 GB，请将最低 GPU 显存设为 70000 MiB。
pd|Auto-stop is on by default.|默认开启自动停止。
se|Session|会话
hs|HF token set|已设置 HF Token
pl|Git URL or local path|Git 链接或本地路径
up|Running for|已运行
sp|Spent|已花费
ai|Account ID (e.g. pro-2)|账号 ID（如 pro-2）
al|Display name|显示名称
an|Leave blank to auto-name|留空自动命名
pr|Found|已就绪
nf|Not found|未找到
mg|Missing|未安装
ar0|Created on first use|首次使用时自动生成
sk|SSH key:|SSH 密钥：
pt|Path:|路径：
za|None running|无运行中
bo|Browser|浏览器
ho|Responding|服务正常
he|Not responding|服务异常
sf|Stop failed — still billing!|停止失败，仍在计费！
ed|Recently ended|最近结束
st0|Setup needed|需要设置
sh0|Install the Colab CLI and sign in with Google.|请安装 Colab CLI 并登录 Google 账号。
ts|Run on|部署到
so|Stop session if it fails|失败时停止会话
si|Before you switch|切换前确认
s1|Ports to close|将关闭的端口
s2|Auto-stop on|有自动停止
s3|No auto-stop (billing)|无自动停止（持续计费）
s4|External|外部
pe|Local port must be 1024–65535. Remote port can't be 8080.|本地端口需在 1024–65535 之间，远程端口不能用 8080。
bn|Enter a number 0 or higher.|请输入不小于 0 的数字。
bc|Enter 1–8.|请输入 1 到 8。
me|measured|实测
es|est.|估算
ec|External CLI|外部 CLI
ex|external|外部
eh|Colab session ({hw})|Colab 会话（{hw}）
eu|Unnamed Colab session|未命名 Colab 会话
ep|External session · {hw}|外部会话 · {hw}
wn|{n} with auto-stop|{n} 个已设自动停止
vn|{a}/{n} auto-stop|{a}/{n} 自动停止
om|Out of GPU memory|GPU 显存不足
gs|Google sign-in|Google 登录
gd|Google (default)|Google（默认）
ud|Updated {t}|更新于 {t}
fl|Settings file:|配置文件：
fb|settings.json has an error. Using your last good settings.|配置文件有误，暂时沿用上次的有效设置。
p2|Refresh while running (s)|运行时刷新（秒）
p3|Refresh when idle (s)|空闲时刷新（秒）
rc|Remove account?|移除账号？
tu|Waiting for port|等待端口
c1|1h|1小时
c2|6h|6小时
c3|24h|1天
c4|7d|7天
c5|All|全部
cn|Now|现在
ce|Not enough data in this range.|该时段数据不足。
wr|Run time {c} of {t}|运行时长 {c} / {t}
ws|Spent {c} of {t} CU|已花费 {c} / {t} CU
wb|Balance {c} CU (keep {t})|余额 {c} CU（最低 {t}）
wu|{l}: unknown|{l}：未知
lr|Run time|运行时长
ls|Spend|花费
lb|Balance|余额
s.up|Connected|已连接
s.starting|Starting…|正在启动…
s.failed|Failed|失败
s.stopped|Stopped|已停止
s.running|Running|运行中
s.ready|Ready|就绪
s.cancelling|Cancelling…|正在取消…
s.cancelled|Cancelled|已取消
s.cancel_failed|Cancel failed|取消失败
s.user_stop|Stopped by you|手动停止
s.guard_spend|Spend limit reached|达到花费上限
s.guard_runtime|Time limit reached|达到时长上限
s.guard_balance|Balance too low|余额过低
s.watchdog_idle|Idle too long|闲置过久
s.watchdog_deadline|Auto-stop time reached|到点自动停止
s.provision|Couldn't start|启动失败
s.vanished|Ended outside Colab Deck|已在外部结束
e.cli_error|Colab CLI returned an error.|Colab CLI 返回错误。
e.bridge_closed|Lost connection to Antigravity.|与 Antigravity 的连接已断开。
e.bridge_timeout|Antigravity didn't respond. Try again.|Antigravity 没有响应，请重试。
e.bridge_init|Couldn't start the panel. Reopen it.|面板启动失败，请重新打开。
e.bridge_no_tools|This panel can't run actions here.|当前环境不支持面板操作。
e.bridge_rpc|Something went wrong. Try again.|操作失败，请重试。`;

export type Locale = "en" | "zh-CN";

export interface LocalizedText {
  en?: string;
  "zh-CN"?: string;
  [key: string]: string | undefined;
}

export interface BalancePoint {
  at_secs: number;
  balance_cu: number;
}

export interface ChartCoord {
  x: number;
  y: number;
  at: number;
  v: number;
}

export type ChartProjection =
  | { visible: false; coords: ChartCoord[]; filtered: BalancePoint[] }
  | { visible: true; sparse: true; coords: ChartCoord[]; filtered: BalancePoint[]; tMin: number; tMax: number }
  | {
      visible: true;
      sparse: false;
      coords: ChartCoord[];
      filtered: BalancePoint[];
      tMin: number;
      tMax: number;
      minV: number;
      maxV: number;
      delta: number;
    };

export const STRINGS: { en: Record<string, string>; "zh-CN": Record<string, string> } = {
  en: {},
  "zh-CN": {},
};

for (const l of RAW.split("\n")) {
  const [k, e, z] = l.split("|");
  STRINGS.en[k] = e;
  STRINGS["zh-CN"][k] = z || e;
}

export const tr = (l: string, k: string, v?: Record<string, unknown> | null): string => {
  const dict = l === "zh-CN" ? STRINGS["zh-CN"] : STRINGS.en;
  let s = dict[k] ?? STRINGS.en[k] ?? k;
  if (v) for (const [a, b] of Object.entries(v)) s = s.replaceAll(`{${a}}`, `${b}`);
  return s;
};

const F = Number.isFinite;
const { floor: FL, max: MX, min: MN } = Math;

export const fmtDur = (s: number | null | undefined, l: string): string => {
  if (typeof s !== "number" || !F(s) || s < 0) return "—";
  const n = FL(s), d = FL(n / 86400), h = FL((n % 86400) / 3600), m = FL((n % 3600) / 60);
  const [D, H, M0, M1, S0] = l === "zh-CN" ? [" 天", " 小时", " 分", " 分钟", " 秒"] : ["d", "h", "m", "m", "s"];
  return d ? `${d}${D}${h ? ` ${h}${H}` : ""}` : h ? `${h}${H}${m ? ` ${m}${M0}` : ""}` : m ? `${m}${M1}` : `${n}${S0}`;
};

export const fmtCu = (n: number | null | undefined): string =>
  typeof n !== "number" || !F(n) ? "—" : n.toFixed(2);

export const fmtErr = (l: string, k: string | null | undefined, o?: LocalizedText | null): string =>
  o?.[l] || o?.en || tr(l, k && STRINGS.en[`e.${k}`] ? `e.${k}` : "e.cli_error");

export function projectChart(
  pts: readonly BalancePoint[] | null | undefined,
  win: number,
  now?: number
): ChartProjection {
  const all = (pts || [])
    .filter((p): p is BalancePoint => Boolean(p) && F(p.at_secs) && F(p.balance_cu))
    .sort((a, b) => a.at_secs - b.at_secs);
  if (!all.length) return { visible: false, coords: [], filtered: [] };
  const end = MX(now || 0, all[all.length - 1].at_secs);
  const cut = win > 0 ? end - win : 0;
  const flt = win > 0 ? all.filter((p) => p.at_secs >= cut) : all;
  if (flt.length < 2) {
    return { visible: true, sparse: true, coords: [], filtered: flt, tMin: cut || all[0].at_secs, tMax: end };
  }
  const tMin = flt[0].at_secs;
  const tMax = MX(flt[flt.length - 1].at_secs, tMin + 1);
  const vs = flt.map((p) => p.balance_cu);
  const minV = MN(...vs);
  const maxV = MX(...vs);
  const sV = MX(0.01, maxV - minV);
  return {
    visible: true,
    sparse: false,
    filtered: flt,
    tMin,
    tMax,
    minV,
    maxV,
    delta: vs[vs.length - 1] - vs[0],
    coords: flt.map(({ at_secs: at, balance_cu: v }) => ({
      x: +(((at - tMin) / (tMax - tMin)) * 320).toFixed(1),
      y: +(62 - ((v - minV) / sV) * 56).toFixed(1),
      at,
      v,
    })),
  };
}
