# 让 AI 上传项目到 GitHub 的通用指令（对话提供 Token 版）

把下面「可复制指令」整段发给 AI，并直接把 GitHub Token 一起提供。
认证统一使用 Token：建仓库、推送代码、发布 Release 全部走 Token，不依赖 SSH。

---

## 可复制指令

```text
请把这个项目上传到 GitHub，并发布最新版本。

项目目录：{PROJECT_DIR}
GitHub 账号：{OWNER}
仓库名：{REPO}
可见性：{Public 或 Private}
版本号：{VERSION}
GitHub Token：{TOKEN}
需要上传到 Release 的附件：{列出文件，例如 dist/app_{VERSION}_amd64.deb 及其 .sha256；没有就写“无”}

要求：
1. 上传前先做仓库体检：检查未提交改动、现有 remote、大文件、密钥与临时文件，
   确认 .gitignore 覆盖 target/、.dev-env/、dist/、*.deb、*.tar.gz、*.log、__pycache__/ 等。
2. 开发过程文档、探针脚本、临时截图如需清理，移到仓库外归档，不要直接删除。
3. 认证使用我提供的 API Token：
   - 直接把 Token 放进环境变量 GH_TOKEN 使用，不要写入任何文件；
   - 绝不回显 Token、绝不写进 .git/config 或任何仓库文件；
   - 推送使用 HTTPS，并通过临时 credential helper 从环境变量读取 Token。
4. 用 REST API 创建仓库（POST /user/repos），不要要求我手动建仓库。
5. 远端仓库若已有初始 LICENSE/README，先 fetch 再在其上提交，不要强推、不要覆盖历史。
6. 提交署名使用：{NAME} <{EMAIL}>。
7. 推送 main 分支，创建并推送 annotated tag v{VERSION}。
8. 通过 API 创建正式 Release（非草稿、非预发布），只上传我列出的附件；
   上传后用 API 核对附件名称和字节数。
9. 全程告诉我每一步的结果；失败时说明原因和下一步，不要反复重试同一操作。
10. 完成后提醒我撤销本次使用的 Token，不要复用已经出现在对话里的 Token。
```

---

## AI 执行清单

### 1. 上传前体检

```bash
git status --short
git remote -v
find . -path ./.git -prune -o -type f -size +50M -print
find . -name '.env' -o -name '*.pem' -o -name '*.key' -o -name 'id_rsa*'
```

确认 `.gitignore` 至少包含：

```gitignore
**/target/
/.dev-env/
/dist/
**/*.deb
/*.tar.gz
/*.zip
/*.sha256
*.log
__pycache__/
*.pyc
.env
```

### 2. 使用用户提供的 Token

Token 由用户在对话中直接给出。AI 只在当前会话内通过环境变量使用，不落盘、不回显。

Token 类型：

- **classic PAT**：勾选 `repo` 即可覆盖建仓库、推送、Release。
- **fine-grained PAT**：需要 `Contents: Read and write` 与 `Administration: Read and write`。

先验证 Token（只打印账号，不打印 Token）：

```bash
export GH_TOKEN='{TOKEN}'
curl -sS -H "Authorization: Bearer $GH_TOKEN" \
  -H "Accept: application/vnd.github+json" \
  https://api.github.com/user | python3 -c 'import sys,json;d=json.load(sys.stdin);print(d.get("login") or d.get("message"))'
```

> 建议用户使用临时 Token，完成后立即撤销；对话中出现过的 Token 一律视为已暴露。

### 3. 创建仓库

```bash
curl -sS -X POST \
  -H "Authorization: Bearer $GH_TOKEN" \
  -H "Accept: application/vnd.github+json" \
  https://api.github.com/user/repos \
  -d '{"name":"REPO","private":false,"description":"项目简介"}'
```

### 4. 初始化与推送（HTTPS + 临时凭据）

```bash
export GH_TOKEN='{TOKEN}'

git init -b main
git remote add origin https://github.com/OWNER/REPO.git
git fetch origin
git reset origin/main          # 仅当远端已有初始提交
git config user.name  "NAME"
git config user.email "EMAIL"
git add -A
git commit -m "chore: 初始提交 vVERSION"

# 临时 credential helper：Token 只从环境变量读取，不落盘、不打印
git -c credential.helper='!f() { echo username=OWNER; echo "password=$GH_TOKEN"; }; f' \
  push -u origin main

git tag -a "vVERSION" -m "VERSION"
git -c credential.helper='!f() { echo username=OWNER; echo "password=$GH_TOKEN"; }; f' \
  push origin "vVERSION"

unset GH_TOKEN                  # 当前 shell 用完即清
```

不要把 Token 拼进 remote URL（例如 `https://TOKEN@github.com/...`），否则会留在
`.git/config`、shell 历史和日志里。

### 5. 发布 Release

```bash
export GH_TOKEN='{TOKEN}'

# 创建 Release，响应里的 id 用于上传附件
curl -sS -X POST \
  -H "Authorization: Bearer $GH_TOKEN" \
  -H "Accept: application/vnd.github+json" \
  https://api.github.com/repos/OWNER/REPO/releases \
  -d '{"tag_name":"vVERSION","name":"vVERSION","body":"发布说明","draft":false,"prerelease":false}'

# 上传附件（ID 替换为上一步返回值）
curl -sS -X POST \
  -H "Authorization: Bearer $GH_TOKEN" \
  -H "Content-Type: application/octet-stream" \
  --data-binary @dist/APP_VERSION_amd64.deb \
  "https://uploads.github.com/repos/OWNER/REPO/releases/ID/assets?name=APP_VERSION_amd64.deb"

# 核对附件
curl -sS -H "Authorization: Bearer $GH_TOKEN" \
  https://api.github.com/repos/OWNER/REPO/releases/tags/vVERSION

unset GH_TOKEN
```

### 6. 验收标准

- `main` 与 tag 已推送，远端 `git ls-remote` 能看到。
- 仓库首页 README 图片正常显示，相对路径无 404。
- Release 状态为正式版，附件名称、数量、字节数与本地一致。
- 仓库内无 `target/`、`.dev-env/`、`dist/`、Token、私钥。

---

## 安全红线

- Token 只在当前会话的环境变量中使用，不打印、不写入文件、不写进仓库、不拼进 remote URL。
- 不把 Token 写进 `.git/config` 或使用 `credential.helper store` 长期明文保存。
- 对话中直接提供 Token 视为已暴露：务必使用可撤销的临时 Token，完成后立即撤销。
- 不执行 `git push --force` 覆盖远端历史。
- 不提交构建产物和发布包；二进制走 Release。
- 删除或归档任何文件前，先确认它不是源码、文档、配置或最新发布包。
