#!/usr/bin/env node
// 薄启动器：真正的构建逻辑在 dist/cli.js（由 src/cli.ts 编译而来）。
// cli.js 只在被直接执行时自动跑 main，这里必须显式调用。
import("../dist/cli.js")
  .then((mod) => {
    mod.main(process.argv);
  })
  .catch((err) => {
    console.error(err);
    process.exit(1);
  });
