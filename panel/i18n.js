const RAW = `t.overview|Overview|总览
t.sessions|Sessions|会话
t.tunnels|Tunnels|端口转发
t.deploy|Deploy|模型与插件
t.settings|Settings|设置与账号
au|Auto|自动
pa|Paused|已暂停
dm|Demo|演示
rl|Real CLI|实机 CLI
bl|CU Balance|算力余额
br|Burn Rate|消耗速率
et|Est. left|预计剩余
rt|Active Runtimes|运行中会话
cg|Cost Guard|成本守卫
ga|Armed|已布防
gp|Paused|已暂停
gw|Warning|预警
gi|Standby|待命
uk|Unknown|未知
sl|Stale|已过期
bz|Idle · 0 CU/h|未计费
ot|Unmanaged|未托管运行时
od|Unmanaged (billing):|未托管(计费中)：
qa|Quick Actions|快捷操作
ng|New GPU|新建 GPU
dg|Deploy GGUF|部署 GGUF
ns|New Session|启动会话
ac|Accelerator|加速器
hm|High-RAM|高内存
std|Standard|标准
mv|Min VRAM|最低显存
mr|Auto-stop (h)|自动停机(h)
im|Idle stop (m)|空闲停机(m)
ms|Spend cap|花费上限
bf|Balance floor|余额底线
sn|Session name|会话名
sv|Provision|分配并启动
cc|Cancel|取消
stopBill|Stop & End Billing|停止并结束计费
cs|Confirm stop?|确认销毁？
ps|Stopping…|待确认停机
e2|+2h|续期+2h
cw|Cancel WD|取消定时
cfw|Confirm? No auto-stop|确认？将无定时
aw|Arm WD|开启定时
oc|Colab|打开 Colab
rk|Restart|重启内核
fw|Forward Port|转发端口
ar|WD Armed|定时生效
un|App-Only · Unarmed|仅主机 · 无定时
nos|No active sessions.|无运行中会话。
tn|Closing tunnel won't stop billing.|关闭隧道不停止计费。
lp|Local Port|本地端口
rp|VM Port (!8080)|VM端口(禁8080)
hp|Health Path|健康路径
op|Open Tunnel|开启转发
cl|Close|关闭
ds|Dismiss|清除
cu|Copy|复制
cr|Copy curl|复制 curl
ou|Open|打开
not|No forwards.|暂无转发。
sr|Search|搜索
in|VRAM Fit|显存拟合
ht|HF Token|HF Token(仅内存)
st|Set|暂存
dp|Deploy|开始部署
cnr|Cancel|取消部署
nor|No deploys.|暂无部署。
ip|Install Plugin|安装插件
ib|Install|安装
bm|Backend Mode|运行后端
sd|Demo (0 Billing)|演示(零计费)
srl|Real CLI|实机 CLI
acc|Accounts|账号与认证
act|Active|当前
sw|Switch|切换
cfs|Confirm Switch|确认切换
sww|Closes tunnels; old VMs guarded.|关闭隧道；旧机受守卫。
aa|Add OAuth2|添加账号
df|Defaults|默认策略
mc|Max Concurrent|最大并发
ss|Save|保存配置
dc|Environment|环境诊断
ok|Saved|已保存
cp|Copied|已复制
reconnected|✓ Reconnected|✓ 已重连
clost|Offline · retrying|断线 · 重试中
ry|Retry|重试
bt|Balance Trend|余额走势
pa1|A100 40/80G — set VRAM 70000.|A100 40/80G — 设显存 70000。
pd|Default G4 · watchdog armed.|默认 G4 · 自动布防。
se|Session|会话
hs|HF Token Set|已配置 HF Token
pls|Git URL or path|Git 地址或路径
up|Up|已运行
sp|Spend|已耗
ai|id (pro-2)|ID (pro-2)
al|Label|标签
an|auto|自动
pr|Present|已就绪
nf|Not found|未找到
mg|Missing|未安装
acr|Auto-create|自动生成
sk|SSH Key:|SSH 密钥:
pt|Path:|路径:
za|0 active|0 运行中
pv|Provisioning|分配中
bo|Browser|浏览器
ho|HTTP OK|HTTP 正常
he|HTTP Err|HTTP 异常
sf|Stop Failed — Billing!|停机失败 — 计费中！
ed|Ended|已结束
sut|Setup Required|环境待配置
suh|Install google-colab-cli & ADC.|安装 google-colab-cli 并认证 ADC。
ts|Target|部署目标
nv|New Session|新建会话
sof|Stop VM on fail|失败销毁新机
sbs|Stop VM?|停止计费？
swi|Switch Preview|切换预览
swt|Tunnels|关闭隧道
swwa|Watched|有定时
swu|Unwatched|无定时(计费)
swo|Unmanaged|未托管
pe|Local 1024–65535; VM !=8080.|本地 1024–65535；VM 禁 8080。
bn|Invalid number (≥0).|须为非负数。
me|measured|实测
es|est.|参考
ec|External CLI|外部 CLI
wb|Web|浏览器
ex|external|未托管`;
const ST = "up:已连接,starting:启动中,failed:失败,stopped:已停止,running:运行中,ready:就绪,cancelling:取消中,cancelled:已取消,cancel_failed:取消失败,user_stop:手动停止,watchdog_idle:空闲停机,watchdog_deadline:定时停机,guard_spend_cap:花费上限,guard_runtime:时长上限,guard_balance:余额底线,vanished:已消失";

export const STRINGS = { en: {}, "zh-CN": {} };
for (const l of RAW.split("\n")) { const [k, en, zh] = l.split("|"); STRINGS.en[k] = en; STRINGS["zh-CN"][k] = zh || en; }
for (const p of ST.split(",")) { const [k, zh] = p.split(":"); STRINGS.en[`s.${k}`] = k.replace(/_/g, " "); STRINGS["zh-CN"][`s.${k}`] = zh; }
export const tr = (l, k) => (STRINGS[l] || STRINGS.en)[k] ?? STRINGS.en[k] ?? k;
const F = Number.isFinite;
export const fmtDur = (s) => {
  if (s == null || !F(s) || s < 0) return "—";
  const n = s | 0, h = (n / 3600) | 0, m = ((n % 3600) / 60) | 0;
  return h ? `${h}h ${m}m` : m ? `${m}m` : `${n}s`;
};
export const fmtCu = (n) => (n == null || !F(n) ? "—" : (+n).toFixed(2));