---
name: git-management
description: Use when managing Git workflows in this project: generating Conventional Commits, committing changes after pull --rebase, explaining conflicts, creating or switching branches, merging, pulling, stashing, reverting, resetting, cleaning, pushing, or handling other common Git operations.
---

# git-management · Git 管理 skill

## 何时使用

当用户要求执行或规划以下 Git 操作时使用本 skill：

- 提交代码、生成 commit message、暂存文件、查看改动
- pull / fetch / rebase / merge / push
- 创建分支、切换分支、合并分支、删除分支
- stash / restore / revert / reset / clean
- 处理本地与远程冲突，解释冲突代码和相关提交
- 查看提交记录、作者、文件变更来源、分支状态

## 总原则

- 全程中文回复，Git 术语可保留英文。
- 先确认当前分支、工作区状态、暂存区状态和远程跟踪分支。
- 默认保持当前提交分支不变；除非用户明确要求创建或切换分支，不主动切分支。
- 不猜测用户意图。涉及覆盖、丢弃、强推、回退、清理文件的操作，必须先解释影响并询问确认。
- 遇到冲突时不得擅自解决；必须解释冲突双方逻辑、相关提交和作者，并让用户决定处理方式。
- 不改 `.codex/config.toml`、hook、构建脚本，除非用户明确要求。

## 默认提交行为

默认情况下，git-management 只完成 Git 入库，不自动触发 `/ship`。

执行规则：

- 适用于普通 git-management 提交、pull/rebase 后提交、直接 push、设置 upstream 后 push。
- 执行完整标准提交流程，包括状态检查、diff 分析、必要暂存、`git pull --rebase`、commit 和 push。
- push 成功后不触发 `/ship`，并在最终汇报中明确写出：`/ship：未触发（默认纯提交）`。
- push 成功但没有新提交时，不触发 `/ship`。
- push 失败、rebase/merge 冲突、用户未确认高风险操作时，不触发 `/ship`。
- 如果本次 git-management 是由 `ship` skill 作为发布前置调用的，只完成同步、提交、push，然后返回当前 `/ship` 流程；不得再次触发新的 `/ship`。
- 不改变现有 `/ship` 主动发布流程；用户执行 `/ship` 时仍按 `ship` skill 的发布前置流程处理。

## Commit message 规范

commit message 必须遵循 Conventional Commits v1.0.0：

```text
<type>(<scope>): <subject>
```

允许在必要时使用：

```text
<type>(<scope>)!: <subject>

<body>

BREAKING CHANGE: <description>
```

项目允许的 `type` 仅限：

- `feat`：新增功能
- `fix`：修复问题
- `refactor`：重构且不改变外部行为
- `style`：格式、样式或不影响逻辑的调整
- `docs`：文档
- `test`：测试
- `chore`：工具、配置、依赖、脚本等维护

生成规则：

- `subject` 使用简短中文，动宾结构，避免句号结尾。
- `scope` 优先来自模块、目录、页面、脚本或能力名，例如 `git`、`scripts`、`api`、`android`。
- 一次提交只描述本次实际暂存的变更，不混入未暂存或无关文件。
- 若包含破坏性变更，使用 `!` 或 `BREAKING CHANGE:` footer，并解释影响。
- 提交前向用户展示最终 commit message；用户要求直接提交时，也要确保 message 合规。

## 标准提交流程

提交代码时按顺序执行：

1. 检查当前状态：

```bash
git status --short
git branch --show-current
git remote -v
git rev-parse --abbrev-ref --symbolic-full-name @{u}
```

2. 分析改动：

```bash
git diff --stat
git diff
git diff --cached --stat
git diff --cached
```

3. 若用户未指定暂存范围：
   - 只暂存与用户任务直接相关的文件。
   - 不暂存明显无关或用户已有改动。
   - 若无法判断文件归属，先询问用户。

提交授权约定：

- 用户明确要求提交、提交所有改动、commit、push、发布前置提交、直接提交或 skill 文件改动时，即视为允许暂存并提交对应范围内改动，不需要再次询问“是否允许提交”。
- 用户要求“提交所有改动”“直接提交”，或本次是发布前置提交 / skill 文件改动时，允许执行 `git add .` 并按实际暂存内容生成合规 commit message。
- 该约定只免除普通暂存、commit、push 的重复确认；涉及覆盖、丢弃、强推、reset、clean、rebase/merge 冲突处理等高风险操作，仍按本 skill 对应规则执行。

4. 提交前同步远程最新代码，默认使用 rebase：

```bash
git pull --rebase
```

要求：

- pull 前再次确认当前分支，pull 后确认分支未改变。
- 若没有 upstream，先报告并询问用户是否设置远程跟踪分支。
- 若工作区未提交改动会阻塞 rebase，优先说明状态；可在用户确认后使用 stash 临时保存。

5. 若同步无冲突：
   - 根据暂存区实际 diff 生成合规 commit message。
   - 执行 `git commit -m "<message>"`。
   - 提交后按 Push 流程推送到远程仓库。
   - push 成功后默认不触发 `/ship`；若本次是 ship 前置 git-management，则直接返回原 `/ship` 流程。
   - 汇报当前分支、commit hash、commit message、push 结果和 `/ship` 结果；默认提交需明确写出 `/ship：未触发（默认纯提交）`。

6. 若同步发生冲突，进入冲突处理流程。

## 冲突处理流程

发生 conflict 时立即暂停，不继续提交。

先收集事实：

```bash
git status --short
git diff --name-only --diff-filter=U
git diff
git log --oneline --decorate --graph --max-count=20
```

对每个冲突文件补充来源信息：

```bash
git log --follow --format='%h %an %ae %ad %s' --date=short -- <file>
git blame -L <start>,<end> -- <file>
```

必须向用户说明：

- 冲突文件和冲突片段位置。
- 当前本地改动的逻辑。
- 远程提交改动的逻辑。
- 相关提交 hash、作者和提交说明。
- 直接采用本地、采用远程、手工合并分别会产生什么影响。

询问用户如何处理后，才能编辑冲突文件。用户确认解决方案后：

```bash
git add <resolved-files>
git rebase --continue
```

若 rebase 继续出现新冲突，重复本流程。全部冲突解决后，再继续标准提交流程。

禁止：

- 不解释冲突就 `checkout --ours` 或 `checkout --theirs`。
- 不确认就 `rebase --abort`、`merge --abort`、`reset`。
- 为了绕过冲突而切换提交分支。

## 常用操作流程

### 创建或切换分支

- 创建分支前确认当前分支和工作区是否干净。
- 默认新分支使用 `codex/` 前缀，除非用户指定其他名称。
- 分支名使用小写、短横线，避免空格和中文。

```bash
git switch -c codex/<name>
git switch <branch>
```

切换分支前若存在未提交改动，必须说明这些改动可能被带到目标分支，并询问是否 stash、提交或继续切换。

### 拉取和同步

- 默认使用 `git pull --rebase`。
- 同步前检查工作区状态。
- 若用户明确要求 merge pull，才使用 `git pull --no-rebase` 或仓库配置策略。

```bash
git fetch --all --prune
git pull --rebase
```

### 合并分支

合并前必须说明：

- 当前分支
- 要合并进来的目标分支
- 是否会产生 merge commit
- 是否存在未提交改动

```bash
git fetch --all --prune
git merge <branch>
```

发生冲突时进入冲突处理流程。冲突解决后根据合并结果提交或继续 merge。

### Stash

stash 前说明会保存哪些文件，stash 后给出恢复命令：

```bash
git stash push -m "<reason>"
git stash list
git stash show --stat stash@{0}
git stash pop stash@{0}
```

`stash pop` 若发生冲突，进入冲突处理流程。

### Revert

`revert` 用于生成一个反向提交，适合撤销已发布提交。执行前必须说明目标 commit、影响文件和会产生的新提交。

```bash
git show --stat <commit>
git revert <commit>
```

发生冲突时进入冲突处理流程。

### Reset / restore / clean

这些操作可能丢弃本地改动，必须二次确认。

允许在用户确认后执行：

```bash
git restore <file>
git restore --staged <file>
git reset --soft <commit>
git reset --mixed <commit>
git clean -n
git clean -fd
```

`git reset --hard` 只有在用户明确点名并确认影响后才允许执行。执行前必须展示将丢弃的提交或文件。

### Push

push 前确认当前分支、远程分支和本地提交：

```bash
git branch --show-current
git log --oneline @{u}..HEAD
git push
```

禁止默认 force push。只有用户明确要求并二次确认后，才允许使用：

```bash
git push --force-with-lease
```

禁止使用 `git push --force`，除非用户明确点名且确认风险。

push 成功后：

- 若 `git log --oneline @{u}..HEAD` 在 push 前存在待推提交，push 成功即视为代码已到仓库，默认不触发 `/ship`。
- 若 push 前无待推提交且命令输出显示 `Everything up-to-date`，不触发 `/ship`。
- 若当前流程是 `ship` 的强制前置 git-management，只返回 `ship` 继续发布，不额外触发新的 `/ship`。

## 输出格式

执行 Git 管理任务时，最终汇报保持精简：

```text
改动摘要：
- ...

Git 结果：
- 当前分支：...
- 执行动作：...
- Commit：<hash> <message>
- /ship：未触发（默认纯提交） / 不适用

验证结果：
- ...

无法完成项：
- 无 / ...
```

发生冲突时汇报：

```text
冲突文件：
- <file>

冲突原因：
- 本地：...
- 远程：...
- 相关提交：<hash> <author> <subject>

待确认：
- 请选择采用本地、采用远程或手工合并方案。
```

## 禁止事项

- 不在用户未确认时执行 `reset --hard`、`clean -fd`、`push --force`、`push --force-with-lease`。
- 不擅自改分支、删分支、改 remote、改 hook。
- 不提交未检查的冲突标记：`<<<<<<<`、`=======`、`>>>>>>>`。
- 不把无关文件混入 commit。
- 不用不符合 Conventional Commits 的提交信息。
