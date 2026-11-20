#!/usr/bin/env node
// 薄启动器：真正的构建逻辑在 dist/cli.js（由 src/cli.ts 编译而来）
import("../dist/cli.js").catch((err) => {
  console.error(err);
  process.exit(1);
});
