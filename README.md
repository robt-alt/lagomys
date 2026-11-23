# lagomys

lagomys 是一门面向 AI 助手的运维与测试脚本语言。助手可以用一段脚本完成本机排查、部署诊断或冒烟验收。运行器在执行前校验动作、参数和目标，按调用方授予的范围和预算执行，并返回结构化 JSON。

## 当前能力

- 内建只读操作可以读取文件、列出目录、查询进程状态、采样本机 CPU 和内存，以及探测 TCP 端口。
- 脚本可以筛选和截取数据、断言验收条件，并在异步任务等待超时后返回任务标识供续查。
- 平台动作通过 HTTP 描述文件或 MCP stdio 扩展接入。

当前规范版本为 0.3。运行器使用 Rust，构建目标为 Linux x86_64、Linux arm64 和 macOS arm64。

## 快速体验

在项目根目录构建。Linux 构建需要 `pkg-config` 和 libcurl 开发包。

```sh
cargo build --release --locked
```

下面的执行绑定授权读取 `Cargo.toml`，脚本读出内容并检查文件非空。

```sh
cat > binding.json <<'JSON'
{
  "spec": "0.3",
  "targets": [{ "kind": "file", "path": "Cargo.toml" }],
  "modules": [],
  "budget": { "seconds": 5, "actions": 1, "output_bytes": 4096 }
}
JSON

./target/release/lagomys - binding.json <<'OPS'
file=call file.read @0
check=assert file
out check.passed
OPS
```

执行成功时，结果中的 `status` 为 `success`，输出值为 `true`。

采样本机资源时，将绑定的 `targets` 设为 `[{"kind":"local"}]`。下面的脚本返回整机概况以及 CPU 和内存排行榜，每张榜最多 3 个进程。

```text
r=call sys.sample @0 3
out r.machine r.cpu r.memory
```

### Apache 2.0协议

lagomys 采用 Apache License 2.0。[查看许可证全文](LICENSE)
