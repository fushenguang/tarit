## Why

Tarit 今天"停止 VM"与"删除 VM"共用同一条终态路径 `stop_vm()` → `teardown_vm()`，后者会无条件删除 VM 的私有 overlay 磁盘（除非是 golden registry 持有的源镜像）。这条路径被三处调用：DELETE API、主机关闭前的 `shutdown_sweep`、以及 `readopt_one` 在网络/调度器恢复失败或 running-map 锁中毒时的清理分支。结果是：任何一次停止意图（包括主机重启这种非用户主动删除的场景）都可能悄悄清空用户数据。这与"进入生产可用"的基本要求相悖——数据丢失必须只发生在明确的删除意图下。同时，主机重启后所有 VM 目前需要人工逐个重建，没有类似 Docker `--restart=always` 的自愈能力，这也是团队日常使用中的真实摩擦。

## What Changes

- 新增 `POST /v1/vms/{id}/stop`：停止 VM 进程、释放网络/cgroup，但保留 overlay 磁盘与 store 记录。
- **BREAKING** `DELETE /v1/vms/{id}` 语义变更：不带参数时行为等价于 `stop`（磁盘与记录都保留，不清空）；只有显式传 `force=true` 才会停止并真正删除 overlay 磁盘、移除 store 记录。
- `shutdown_sweep`（主机关闭前的清扫）与 `readopt_one` 里当前调用 `teardown_vm` 的三处清理分支（网络恢复失败 / 调度器恢复失败 / running-map 锁中毒），以及 `quarantine_readopted_runtime`，全部改为走新的"只停不删"路径，不再默认清空磁盘。
- `VmStatus::Stopped` 语义重新定义：从"已停止且磁盘已删除的终态"变为"进程已停止、磁盘/记录仍保留、可恢复"。
- vmm-core 新增"复用已有 overlay 冷启动"原语：`create()` 目前强制 `O_CREAT|O_EXCL` 新建 overlay，无法接回已存在的磁盘；`restore()` 路径的 `prepare_restore_overlay` 虽有 `adopt_private` 分支能接管已存在的 overlay，但只能通过必须携带 RAM 快照路径的 `restore` RPC 触达。本变更新增一条不依赖 RAM 快照、纯冷启动接回已有磁盘的路径。
- 新增 `VmRecord.restart_policy` 字段（`no` | `always`），可在创建请求中指定。
- taritd 启动序列新增一步：`readopt_running_vms` 完成之后，对 `restart_policy = always` 且磁盘仍存在的 `Stopped` VM，用新的冷启动原语自动拉起。
- 新增 `POST /v1/vms/{id}/start`：手动将一个磁盘保留的 `Stopped` VM 用同一个冷启动原语重新拉起，供自动重启与手动重启共用。

## Capabilities

### New Capabilities
- `vm-stop`: 新增"只停止、保留磁盘与记录"的语义与 `POST /v1/vms/{id}/stop` 端点，`shutdown_sweep`/`readopt` 清理路径统一改用此语义。
- `vm-restart`: `restart_policy` 字段、主机重启后按策略自动冷启动、`POST /v1/vms/{id}/start` 手动重启端点，以及底层"复用已有 overlay 冷启动"原语。
- `vm-delete`: `DELETE /v1/vms/{id}` 的默认可恢复行为与显式 `force=true` 才真正清空磁盘/记录的语义。

### Modified Capabilities
（无——仓库 `openspec/specs/` 目前为空，这是第一个走完整流程的 change，没有既有 spec 需要修改。）

## Impact

- `orch/crates/taritd/src/supervisor.rs`：`stop_vm`/`teardown_vm` 拆分为"停止（保留磁盘）"与"清空（删除磁盘）"两条路径；`readopt_one`/`quarantine_readopted_runtime` 的三处清理分支；新增启动期自动重启扫描（挂在 `main.rs` 里 `readopt_running_vms` 之后）。
- `orch/crates/taritd/src/api.rs`：新增 `stop`/`start` 路由；`delete_vm` handler 增加 `force` 查询参数分支。
- `orch/crates/taritd/src/ops.rs`：`stop_local`/`stop_all_local` 调整为默认不清空磁盘；新增对应的 `start_local`/`delete_local(force)` 编排逻辑。
- `orch/crates/tarit-types` + `orch/crates/tarit-store`：`VmStatus::Stopped` 语义文档更新；`VmRecord` 新增 `restart_policy` 字段及对应的 store 迁移。
- `vmm/crates/vmm-core`：新增"复用已有 overlay 冷启动"原语（`controller.rs` 的 `create`/`prepare_restore_overlay` 附近）。
- `vmm/crates/vmm-api`：如该原语需要新的 RPC 请求变体来触达，需在 `rpc.rs` dispatch 增加对应分支。
- 文档：`README.md`、`orch/docs/API.md`、`orch/docs/CONFIGURATION.md` 需要同步新端点与语义变更。
- **BREAKING**：依赖"DELETE 立即清空磁盘"这一旧行为的外部脚本/自动化会受影响。目前没有已知的外部消费者（Huntaway 尚未对接这套 API，仍在调研阶段），风险可控，但会在 CHANGELOG 中显式标注。

## Data-loss / rollback

- **本变更的首要目的就是消除现有数据丢失风险**：改动前，stop/delete/shutdown_sweep/readopt-失败 四条路径都会删除 overlay 磁盘；改动后默认路径永不删除磁盘，只有显式 `force=true` 才会——净效果是让"误删"变得更难，而不是更容易。
- **迁移风险**：`VmStatus::Stopped` 语义变化后，任何假设"Stopped = 磁盘已清空、可以复用同一 VM ID 重新创建"的既有自动化都需要复核。目前确认没有这类外部消费者（见 Impact），风险仅限于内部；如后续发现遗漏，需要在 CHANGELOG 与迁移说明中明确标注。
- **新增失败模式**：`restart_policy = always` 引入了"启动期自动冷启动"这一新行为——如果保留的磁盘本身已损坏或不完整，taritd 每次启动都会尝试拉起一个必然失败的 VM。design.md 需要给出重试退避或失败标记策略，避免無限重试拖慢启动或刷屏日志；本 proposal 不展开具体机制。
- **回滚路径**：如果这次改动本身有 bug，可以整体回退到上一个 tag——由于默认行为变得更保守（磁盘保留而非删除），最坏情况是该清空的磁盘继续占用空间（可接受、可人工清理的失败模式），而不是数据丢失。真正的高风险点集中在 `force=true` 路径与新的"复用 overlay 冷启动"原语的正确性上，两者都必须有专门的失败测试覆盖（先写测试复现问题，再实现修复，符合仓库 TDD 要求）。
- 现有的 LVM 快照安全网（`tarit-snapshot.sh` + `tarit-snapshot-guard.service`）作为独立于应用层之外的最后一道防线继续保留，不因这次改动而移除或弱化。
