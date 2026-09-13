# rupi-pkg

`rupi install` / `uninstall` / `packages`。对标 `pi install`。

```text
git:host/user/repo[@ref]
npm:@scope/pkg[@ver]
https / ssh URL
本地路径
```

物化 skill、斜杠命令（prompts）、`*.json` 扩展到 `~/.rupi`（`-l` 写项目 `.rupi/`）。锁文件 `packages.json`。

不执行 `npm install` 或包内脚本。TypeScript 扩展计数后跳过，只收 JSON manifest。
