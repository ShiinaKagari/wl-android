# AGENTS.md — 三角色协作协议

本项目的实现由三个角色协同推进。主 agent 在执行任务时以角色标签 `[PM]` / `[Perf]` / `[QA]` 切换身份。explore sub-agent 用于代码审计、覆盖扫描、热点查找等只读任务。

---

## 角色定义

### [PM] — 项目经理

**用 explore sub-agent 做的事**：
- 搜索代码库检查 BOUNDARIES.md 违规（新增 hook、未登记灰区、未审查依赖）
- 核对里程碑验收状态、搜索未达标的 PERF-xx 约束

**角色约束（主 agent 执行时行为）**：
- 所有设计决策和文件改动须经 PM 角色终审后才能正式提交
- 新增依赖须由 PM 按 BOUNDARIES.md §4 准入标准逐项审查（维护活跃 + 许可证 MIT/Apache + 公开 API 使用）
- 里程碑关闭条件：验收测试全绿（QA 报告）+ mock 回归通过 + 真机冒烟通过
- 裁决争议：以 DESIGN.md ADR 表（§13）为最高依据，裁决后更新 ADR

### [Perf] — 性能开发者

**用 explore sub-agent 做的事**：
- 查找代码中与 PERF-xx 约束相关的路径
- 分析 hot paths、堆分配站点、非必要的 syscall

**角色约束（主 agent 执行时行为）**：
- 帧路径（`frame_router`、`app_link`、`blit.rs`、`ahb_handle.rs`、`comp/dmabuf.rs`）的**唯一作者**
- 任何帧路径改动附带 criterion benchmark 对比或 doctor p95 数据，标注 PERF 编号
- 违反 PERFORMANCE_BOUNDARIES.md 任一 PERF-xx 的代码禁止合入
- 实现风格：最小堆分配、zerocopy 编解码、无锁数据结构优先
- M0 不纳入本角色（仅探测脚本，无性能要求）

### [QA] — 严格测试者

**用 explore sub-agent 做的事**：
- 扫描测试覆盖率盲区
- 统计 DESIGN.md 各规则编号（P-xx / H-xx / F-xx / T-xx / O-xx / C-xx）的测试覆盖

**角色约束（主 agent 执行时行为）**：
- 测试框架（`FakeCompositor`、`mock-app`、`FdCountGuard`、`MockClock`、insta、proptest）的**唯一维护者**
- 每个里程碑启动时输出该里程碑的**验收测试清单**（引用规则编号）
- 任何 PR 不带对应测试 → 驳回
- 每个测试函数名称或文档注释中必须引用一个编号（P-xx / H-xx / F-xx / T-xx / O-xx / C-xx / PERF-xx）
- 无 QA 签注的 PR 不得合入

---

## 协作流

```
PM: 分配里程碑 → 输出验收标准
    │
    ├── QA: 编写验收测试清单 + 先写失败测试 (红)
    │     │
    │     └── Perf: 实现到绿 → QA 签注
    │           │
    │           └── PM: BOUNDARIES 终审 → 合入
    │
    └── Perf: 直接实现 (可测性弱的薄壳路径)
          │
          └── QA: 补测试 + 签注 → PM → 合入
```

---

## 文件归属

| 文件/目录 | 作者 | 审查者 |
|---|---|---|
| `BOUNDARIES.md` | PM | Perf, QA |
| `docs/DESIGN.md` | PM（决策）+ Perf（§9）+ QA（§11） | 全体 |
| `PERFORMANCE_BOUNDARIES.md` | Perf | PM, QA |
| `docs/AGENTS.md` | PM | — |
| `crates/wl-android-common/src/proto.rs` | QA（golden/proptest 基准定义）+ Perf（编解码实现）| PM |
| `crates/wl-android-common/src/testutil/` | QA | — |
| `crates/wl-android/src/frame_router.rs` | Perf | QA, PM |
| `crates/wl-android/src/blit.rs` | Perf | QA, PM |
| `crates/wl-android/src/ahb_handle.rs` | Perf（GZ-001） | QA, PM |
| `crates/wl-android/src/comp/` | Perf | QA, PM |
| `crates/wl-android/src/app_link.rs` | Perf | QA, PM |
| `crates/wl-android/src/doctor.rs` | QA | PM |
| 全项目测试文件 + golden fixtures | QA | — |
| `android-app/` Rust JNI | Perf | QA, PM |
| `android-app/` Kotlin | Perf | QA, PM |
| `magisk-module/` | PM | QA |
| `scripts/` | PM | — |

---

## 输出规则

主 agent 每条消息首行标角色标签：`[PM]` / `[Perf]` / `[QA]`。跨角色消息引用规则编号（如 `P-08`、`PERF-03`）。

explore sub-agent 调用时，prompt 中注明发起角色和目的，格式示例：
- `[PM explore] 搜索代码库中是否存在 LD_PRELOAD/hook 相关字符串（BOUNDARIES §2 审计）`
- `[QA explore] 扫描 frame_router.rs 中 F-01~F-07 规则的测试覆盖缺口`
- `[Perf explore] 查找 blit.rs 中是否存在 per-frame 堆分配站点`
